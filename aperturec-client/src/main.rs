use aperturec_client::{client, gtk3, metrics};
use aperturec_metrics::exporters::{
    CsvExporter, Exporter, LogExporter, PrometheusExporter, PushgatewayExporter,
};
use aperturec_utils::*;

use anyhow::{anyhow, ensure, Result};
use clap::Parser;
use gethostname::gethostname;
use openssl::x509::X509;
use std::env;
use std::fs;
use std::iter;
use std::path::PathBuf;
use sysinfo::{CpuRefreshKind, RefreshKind, SystemExt};
use tracing::*;
use tracing_subscriber::prelude::*;
use url::Url;

#[derive(Debug, clap::Args)]
#[group(required = false, multiple = false)]
pub struct ResolutionGroup {
    /// Display size specified as WIDTHxHEIGHT
    #[arg(short, long, default_value_t = format!("{}x{}", gtk3::DEFAULT_RESOLUTION.0, gtk3::DEFAULT_RESOLUTION.1))]
    resolution: String,

    /// Set resolution to your primary display's current size and startup in fullscreen mode.
    /// Fullscreen mode can be toggled at any time with Ctrl+Alt+Enter
    #[arg(short, long, action = clap::ArgAction::SetTrue)]
    fullscreen: bool,
}

impl From<ResolutionGroup> for (u64, u64) {
    fn from(g: ResolutionGroup) -> (u64, u64) {
        if g.fullscreen {
            let full = gtk3::get_fullscreen_dims().expect("fullscreen dims");
            (full.0 as u64, full.1 as u64)
        } else {
            let dims: Vec<u64> = g
                .resolution
                .to_lowercase()
                .split('x')
                .map(|d| {
                    d.parse().unwrap_or_else(|e| {
                        panic!("Failed to parse resolution '{}': {}", g.resolution, e)
                    })
                })
                .collect();
            if dims.len() != 2 {
                panic!("Invalid resolution: {}", g.resolution);
            }
            (dims[0], dims[1])
        }
    }
}

#[derive(Parser, Debug)]
struct Args {
    #[clap(flatten)]
    resolution: ResolutionGroup,

    /// Maximum number of decoders to use. Each decoder will consume one thread and one UDP port. A
    /// value of 0 uses the number of CPU cores available.
    #[arg(short, long, default_value_t = 0)]
    decoder_max: u16,

    /// Log metric data to a CSV file at the provided path
    #[arg(long, default_value = None)]
    metrics_csv: Option<String>,

    /// Log metric data at the DEBUG level (-vvv)
    #[arg(long)]
    metrics_log: bool,

    /// Serve Prometheus metrics at the given (optional) bind address. Defaults to 127.0.0.1:8080
    #[arg(long, num_args = 0..=1, default_value = None, default_missing_value = "127.0.0.1:8080")]
    metrics_prometheus: Option<String>,

    /// Send metric data to Pushgateway instance at the provided URL
    #[arg(long, default_value = None)]
    metrics_pushgateway: Option<String>,

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

    /// Initial ID of the client. Must match the server's --temp-client-id.
    #[arg(short, long, default_value_t = 1234)]
    temp_client_id: u64,

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
    log: log::LogArgGroup,
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
        host = format!("{}:{}", host, port);
    }
    let args = parsed_uri.query_pairs().flat_map(|(k, v)| {
        let k = if k.len() == 1 {
            format!("-{}", k)
        } else {
            format!("--{}", k)
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

fn main() -> Result<()> {
    let args = match env::var("AC_URI") {
        Ok(uri) => args_from_uri(&uri)?,
        Err(_) => Args::parse(),
    };

    let (log_layer, _guard) = args.log.as_tracing_layer()?;
    tracing_subscriber::registry().with(log_layer).init();

    info!("ApertureC Client Startup");

    let decoder_max = if args.decoder_max != 0 {
        args.decoder_max
    } else {
        let sys = sysinfo::System::new_with_specifics(
            RefreshKind::new().with_cpu(CpuRefreshKind::everything()),
        );
        sys.cpus().len().try_into().unwrap()
    };

    let (width, height) = args.resolution.into();

    let mut config_builder = client::ConfigurationBuilder::default();
    config_builder
        .decoder_max(decoder_max)
        .name(gethostname().into_string().unwrap())
        .server_addr(args.server_address)
        .win_height(height)
        .win_width(width)
        .temp_id(args.temp_client_id)
        .allow_insecure_connection(args.insecure)
        .client_bound_tunnel_reqs(args.local)
        .server_bound_tunnel_reqs(args.remote);
    if let Some(program_cmdline) = args.program_cmdline {
        config_builder.program_cmdline(program_cmdline);
    }
    for cert_path in args.additional_tls_certificates {
        config_builder.additional_tls_certificate(X509::from_pem(&fs::read(cert_path)?)?);
    }
    let config = config_builder.build()?;

    debug!("{:#?}", config);

    let mut metrics_started = false;

    if args.metrics_log
        || args.metrics_csv.is_some()
        || args.metrics_pushgateway.is_some()
        || args.metrics_prometheus.is_some()
    {
        let mut exporters: Vec<Exporter> = vec![];
        if args.metrics_log {
            match LogExporter::new(Level::DEBUG) {
                Ok(le) => exporters.push(Exporter::Log(le)),
                Err(err) => warn!("Failed to setup Log exporter: {}, disabling", err),
            }
        }
        if let Some(path) = args.metrics_csv {
            match CsvExporter::new(path) {
                Ok(csve) => exporters.push(Exporter::Csv(csve)),
                Err(err) => warn!("Failed to setup CSV exporter: {}, disabling", err),
            }
        }
        if let Some(url) = args.metrics_pushgateway {
            match PushgatewayExporter::new(
                url.to_owned(),
                "aperturec_client".to_owned(),
                std::process::id(),
            ) {
                Ok(pge) => exporters.push(Exporter::Pushgateway(pge)),
                Err(err) => warn!("Failed to setup Pushgateway exporter: {}, disabling", err),
            }
        }
        if let Some(bind_addr) = args.metrics_prometheus {
            match PrometheusExporter::new(&bind_addr) {
                Ok(pe) => exporters.push(Exporter::Prometheus(pe)),
                Err(err) => warn!("Failed to setup Prometheus exporter: {}, disabling", err),
            }
        }

        aperturec_metrics::MetricsInitializer::default()
            .with_poll_rate_from_secs(3)
            .with_exporters(exporters)
            .init()
            .expect("Failed to setup metrics");
        metrics_started = true;

        metrics::setup_client_metrics();
    }

    client::run_client(config.clone())?;
    if metrics_started {
        aperturec_metrics::stop();
    }

    Ok(())
}
