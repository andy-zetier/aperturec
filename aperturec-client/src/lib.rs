pub mod client;
#[cfg(feature = "ffi-lib")]
pub mod ffi;
pub mod frame;
pub mod gtk3;
pub mod metrics;

use aperturec_graphics::prelude::*;
use aperturec_utils::{args, warn_early};

use anyhow::{Result, anyhow, ensure};
use clap::Parser;
use gethostname::gethostname;
use openssl::x509::X509;
use secrecy::SecretString;
use std::env;
use std::fs;
use std::iter;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::thread;
use tracing::*;
use tracing_subscriber::prelude::*;
use url::Url;

fn parse_resolution(s: &str) -> Result<Size, String> {
    let re = regex::Regex::new(r"^(\d+)[xX](\d+)$").expect("build regex");
    let Some((_, [width, height])) = re.captures(s).map(|c| c.extract()) else {
        return Err("Resolution must be in the form WIDTHxHEIGHT".to_string());
    };
    let width = width.parse().map_err(|w| format!("Invalid width: '{w}'"))?;
    let height = height
        .parse()
        .map_err(|h| format!("Invalid height: '{h}'"))?;
    Ok(Size::new(width, height))
}

#[derive(Debug, clap::Args)]
#[group(required = false, multiple = false)]
struct ResolutionGroup {
    /// Display size specified as WIDTHxHEIGHT
    #[arg(short, long, default_value = format!("{:?}", gtk3::DEFAULT_RESOLUTION), value_parser = parse_resolution)]
    resolution: Size,

    /// Set resolution to your displays' current sizes and startup in multi-monitor fullscreen
    /// mode. Fullscreen mode can be toggled at any time with Ctrl+Alt+Enter
    #[arg(short, long, action = clap::ArgAction::SetTrue)]
    fullscreen: bool,

    /// Set resolution to the current display's size and startup in single-monitor fullscreen mode.
    /// Single-monitor fullscreen mode can be toggled at any time with Ctrl+Alt+Shift+Enter
    #[arg(long, action = clap::ArgAction::SetTrue)]
    single_fullscreen: bool,
}

impl From<ResolutionGroup> for client::DisplayMode {
    fn from(g: ResolutionGroup) -> Self {
        if g.fullscreen {
            client::DisplayMode::MultiFullscreen
        } else if g.single_fullscreen {
            client::DisplayMode::SingleFullscreen
        } else {
            client::DisplayMode::Windowed { size: g.resolution }
        }
    }
}

#[derive(Parser, Debug)]
#[command(author, version, about)]
struct Args {
    #[clap(flatten)]
    resolution: ResolutionGroup,

    /// Maximum number of decoders to use
    #[arg(short, long, default_value_t = thread::available_parallelism().unwrap())]
    decoder_max: NonZeroUsize,

    /// Program to launch and display on connection.
    ///
    /// If left unspecified, the client will open the server-specified root program, resuming any
    /// previous state. If specified, the client will force the server to launch a new instance of
    /// the specified program, which will be killed on client disconnect.
    ///
    /// Specifying this argument requires the server to have set `--allow-client-exec`.
    #[arg(index = 2)]
    program_cmdline: Option<String>,

    /// Hostname or IP address of the server, optionally including a control port. Eg. mybox.com,
    /// 10.10.10.10:46454, myotherbox.io:12345, [::1]
    #[arg(index = 1)]
    server_address: String,

    /// Additional TLS certificates to enable connections to servers that are not serving
    /// certificates signed by already installed CAs
    #[arg(short, long, default_values_os_t = Vec::<PathBuf>::new())]
    additional_tls_certificates: Vec<PathBuf>,

    /// Disable server certificate validation. Similar to `curl -k / --insecure`
    #[arg(short = 'k', long, action)]
    insecure: bool,

    /// Specifies that connections to the given TCP port on the client side are to be forwarded to
    /// the given host and port on the server side. Arguments are given in the form
    /// `[bind_address:]bind_port:forward_address:forward_port`
    ///
    /// If `bind_address` is left unspecified, the socket will bind to 0.0.0.0.
    #[arg(short = 'L', long)]
    local: Vec<client::PortForwardArg>,

    /// Specifies that connections to the given TCP port on the server side are to be forwarded to
    /// the given host and port on the client side. Arguments are given in the form
    /// `[bind_address:]bind_port:forward_address:forward_port`
    ///
    /// If `bind_address` is left unspecified, the socket will bind to 0.0.0.0. If `bind_port` is
    /// set to 0, the server will dynamically allocate a bind port and log it.
    #[arg(short = 'R', long)]
    remote: Vec<client::PortForwardArg>,

    #[clap(flatten)]
    log: args::log::LogArgGroup,

    #[clap(flatten)]
    auth_token: args::auth_token::AuthTokenAllArgGroup,

    #[clap(flatten)]
    metrics: args::metrics::MetricsArgGroup,
}

fn args_from_uri(uri: &str) -> Result<Args> {
    const URI_SCHEME: &str = "aperturec";

    let parsed_uri = Url::parse(uri)?;
    ensure!(
        parsed_uri.scheme() == URI_SCHEME,
        "URI scheme should be '{}', is '{}'",
        URI_SCHEME,
        parsed_uri.scheme()
    );
    ensure!(parsed_uri.username() == "", "URI provides username");
    ensure!(parsed_uri.fragment().is_none(), "URI provides fragment");

    let mut host = parsed_uri
        .host_str()
        .ok_or(anyhow!("URI provides no host"))?
        .to_string();
    if let Some(port) = parsed_uri.port() {
        host = format!("{host}:{port}");
    }
    let args = parsed_uri.query_pairs().flat_map(|(k, v)| {
        let k = if k.len() == 1 {
            format!("-{k}")
        } else {
            format!("--{k}")
        };

        if v.is_empty() {
            vec![k]
        } else {
            vec![k, v.to_string()]
        }
    });
    Ok(Args::try_parse_from(
        iter::once(env!("CARGO_PKG_NAME").to_string())
            .chain(iter::once(host))
            .chain(args),
    )?)
}

pub fn run() -> Result<()> {
    let args = if env::args().count() == 2 {
        let arg1 = env::args().nth(1).unwrap();
        if let Ok(parsed) = args_from_uri(&arg1) {
            parsed
        } else if let Ok(uri) = env::var("AC_URI") {
            warn_early!(
                "CLI arguments are ignored when using AC_URI. Unset AC_URI if you would like to use CLI arguments."
            );
            args_from_uri(&uri)?
        } else {
            Args::parse()
        }
    } else if let Ok(uri) = env::var("AC_URI") {
        if env::args().count() > 1 {
            warn_early!(
                "CLI arguments are ignored when using AC_URI. Unset AC_URI if you would like to use CLI arguments."
            );
        }
        args_from_uri(&uri)?
    } else {
        Args::parse()
    };

    let (log_layer, _guard) = args.log.as_tracing_layer()?;
    tracing_subscriber::registry().with(log_layer).init();

    info!("ApertureC Client Startup");

    let config = {
        // Scope config_builder to ensure it is dropped and any auth-token leaves memory
        let mut config_builder = client::ConfigurationBuilder::default();
        let auth_token = match args.auth_token.into_token()? {
            Some(token) => token,
            None => SecretString::from(rpassword::prompt_password(format!(
                "Authentication token for {}: ",
                args.server_address
            ))?),
        };
        config_builder
            .decoder_max(args.decoder_max)
            .name(gethostname().into_string().unwrap())
            .server_addr(args.server_address)
            .auth_token(auth_token)
            .initial_display_mode(args.resolution.into())
            .allow_insecure_connection(args.insecure)
            .client_bound_tunnel_reqs(args.local)
            .server_bound_tunnel_reqs(args.remote);
        if let Some(program_cmdline) = args.program_cmdline {
            config_builder.program_cmdline(program_cmdline);
        }
        for cert_path in args.additional_tls_certificates {
            config_builder.additional_tls_certificate(X509::from_pem(&fs::read(cert_path)?)?);
        }
        config_builder.build()?
    };
    debug!(?config);

    let metrics_exporters = args.metrics.to_exporters(env!("CARGO_CRATE_NAME"));
    if !metrics_exporters.is_empty() {
        aperturec_metrics::MetricsInitializer::default()
            .with_poll_rate_from_secs(3)
            .with_exporters(metrics_exporters)
            .init()
            .expect("Failed to setup metrics");
        metrics::setup_client_metrics();
    }

    client::run_client(config.clone())?;
    aperturec_metrics::stop();

    Ok(())
}

#[cfg(feature = "ffi-lib")]
#[unsafe(no_mangle)]
pub extern "C" fn run_aperturec_client() -> libc::c_int {
    match run() {
        Ok(()) => 0,
        Err(error) => {
            error!(%error);
            1
        }
    }
}
