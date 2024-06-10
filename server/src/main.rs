use aperturec_channel::transport::datagram;
use aperturec_server::backend;
use aperturec_server::metrics;
use aperturec_server::server::*;
use aperturec_state_machine::*;
use aperturec_trace::{self as trace, log, Level};

use anyhow::Result;
use clap::Parser;
use gethostname::gethostname;
use std::path::PathBuf;
use std::str::FromStr;

#[derive(Debug, clap::Args)]
#[group(required = true, multiple = false)]
pub struct RootProgramGroup {
    /// Command-line invocation for root program to launch (likely a desktop environment)
    #[arg(index = 1)]
    root_program_cmdline: Option<String>,

    /// Allow no root program to be set. This will result in clients connecting to an
    /// empty screen unless a root program is launched out of band
    #[arg(long)]
    no_root_program: bool,
}

#[derive(Debug, clap::Args, Clone)]
#[group(required = true, multiple = true)]
struct TlsGroup {
    /// Path to TLS certificate PEM file
    #[arg(short, long, requires = "private_key", conflicts_with_all = &["tls_save_directory", "external_addresses"])]
    certificate: Option<PathBuf>,

    /// Path to TLS private key PEM file
    #[arg(short, long, requires = "certificate", conflicts_with_all = &["tls_save_directory", "external_addresses"])]
    private_key: Option<PathBuf>,

    /// Path to save TLS material which is generated at runtime
    #[arg(short = 'd', long, conflicts_with_all = &["private_key", "certificate"])]
    tls_save_directory: Option<PathBuf>,

    /// Domain names the server will operate on
    #[arg(short, long = "external-address", requires = "tls_save_directory", conflicts_with_all =  &["private_key", "certificate"], default_value = gethostname())]
    external_addresses: Option<Vec<String>>,
}

#[derive(Clone, Debug)]
enum WindowSizeOption {
    Size(usize),
    Negotiated,
    Disabled,
}

impl FromStr for WindowSizeOption {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_uppercase().as_str() {
            "NONE" => Ok(WindowSizeOption::Disabled),
            "NEGOTIATED" => Ok(WindowSizeOption::Negotiated),
            _ => {
                let val = s.parse()?;
                if val < datagram::MAX_SIZE {
                    anyhow::bail!("window size cannot be smaller than {}", datagram::MAX_SIZE)
                }
                Ok(WindowSizeOption::Size(val))
            }
        }
    }
}

impl From<WindowSizeOption> for Option<usize> {
    fn from(w: WindowSizeOption) -> Option<usize> {
        match w {
            WindowSizeOption::Disabled => None,
            WindowSizeOption::Negotiated => Some(0),
            WindowSizeOption::Size(sz) => Some(sz),
        }
    }
}

impl From<RootProgramGroup> for Option<String> {
    fn from(g: RootProgramGroup) -> Option<String> {
        match (g.root_program_cmdline, g.no_root_program) {
            (Some(root_program_cmdline), false) => Some(root_program_cmdline),
            (None, true) => None,
            _ => unreachable!("root-program and no-root-program in invalid state"),
        }
    }
}

impl From<TlsGroup> for TlsConfiguration {
    fn from(g: TlsGroup) -> TlsConfiguration {
        match (
            g.certificate,
            g.private_key,
            g.tls_save_directory,
            g.external_addresses,
        ) {
            (Some(certificate_path), Some(private_key_path), None, _) => {
                TlsConfiguration::Provided {
                    certificate_path,
                    private_key_path,
                }
            }
            (None, None, Some(save_directory), Some(external_addresses)) => {
                TlsConfiguration::Generated {
                    save_directory,
                    external_addresses,
                }
            }
            cfg => unreachable!("invalid TLS configuration: {:?}", cfg),
        }
    }
}

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[clap(flatten)]
    root_program_cmdline: RootProgramGroup,

    /// Allow clients to specify programs to execute at connection time. See the client's
    /// `program` argument for details
    #[arg(short, long, action = clap::ArgAction::SetTrue, default_value = "false")]
    allow_client_exec: bool,

    /// External IP address for the server to listen on, optionally including a port
    #[arg(short, long, default_value = "0.0.0.0")]
    bind_address: String,

    /// Display size specified as WIDTHxHEIGHT.
    #[arg(short, long, default_value = format!("{}x{}", backend::X::DEFAULT_MAX_WIDTH, backend::X::DEFAULT_MAX_HEIGHT))]
    screen_size: String,

    /// Server instance name
    #[arg(short, long, default_value = gethostname())]
    name: String,

    /// Initial ID that a client can use to connect to the server
    #[arg(short, long, default_value = "1234")]
    temp_client_id: u64,

    /// Maximum data transmit rate in whole megabits per second (Mbps). Specifying "none" turns off
    /// max bit rate throttling.
    #[arg(long, default_value = "25")]
    mbps_max: String,

    /// Flow control window size in bytes. Leaving this unspecified will negotiate the window size
    /// between Client and Server. Specifying "none" turns off windowing. The window size cannot be
    /// smaller than one message.
    #[arg(long, default_value = "Negotiated", value_parser = clap::value_parser!(WindowSizeOption))]
    window_size: WindowSizeOption,

    #[clap(flatten)]
    tls: TlsGroup,

    /// Log level verbosity, defaults to Warning if not specified. Multiple -v options increase the
    /// verbosity. The maximum is 3. Overwrites the behavior set via AC_TRACE_FILTER environment
    /// variable and --trace-filter argument
    #[arg(short, action = clap::ArgAction::Count)]
    verbosity: u8,

    /// Log metric data at the DEBUG level (-vvv)
    #[arg(long)]
    metrics_log: bool,

    /// Log metric data to a CSV file at the provided path
    #[arg(long, default_value = None)]
    metrics_csv: Option<String>,

    /// Send metric data to Pushgateway instance at the provided URL
    #[arg(long, default_value = None)]
    metrics_pushgateway: Option<String>,

    /// Output directory for tracing data
    #[arg(long, env=trace::OUTDIR_ENV_VAR, default_value = trace::default_output_path_display())]
    trace_output_directory: PathBuf,

    /// Trace filter directive as defined by the
    /// [`EnvFilter`](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html)
    /// documentation
    #[arg(long, env=trace::FILTER_ENV_VAR, default_value = trace::default_filter_directive())]
    trace_filter: String,

    /// Log to rotating files in the specified output directory in addition to stdout/stderr
    #[arg(long, default_value_t = false)]
    log_to_file: bool,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let args = Args::parse();
    let log_verbosity = match args.verbosity {
        0 => Level::WARN,
        1 => Level::INFO,
        2 => Level::DEBUG,
        _ => Level::TRACE,
    };
    trace::Configuration::new("server")
        .cmdline_verbosity(log_verbosity)
        .output_directory(&args.trace_output_directory)
        .trace_filter(&args.trace_filter)
        .log_to_file(args.log_to_file)
        .initialize()?;

    let dims: Vec<usize> = args
        .screen_size
        .to_lowercase()
        .split('x')
        .map(|d| {
            d.parse()
                .unwrap_or_else(|_| panic!("Failed to parse resolution '{}'", d))
        })
        .collect();

    if dims.len() != 2 {
        panic!("Invalid resolution");
    }

    metrics::setup_server_metrics(
        args.metrics_log,
        args.metrics_csv,
        args.metrics_pushgateway,
        std::process::id(),
    );

    let mut config_builder = ConfigurationBuilder::default();
    config_builder
        .bind_addr(args.bind_address)
        .name(args.name)
        .temp_client_id(args.temp_client_id)
        .max_width(dims[0])
        .max_height(dims[1])
        .mbps_max(match args.mbps_max.to_uppercase().as_str() {
            "NONE" => None,
            mbps => Some(mbps.parse()?),
        })
        .window_size(args.window_size.into())
        .tls_configuration(args.tls.into())
        .allow_client_exec(args.allow_client_exec);

    if let Some(root_program_cmdline) = args.root_program_cmdline.into() {
        config_builder.root_process_cmdline(root_program_cmdline);
    }
    let config = config_builder.build()?;

    let server = Server::<Created>::new(config)?;
    let server: Server<BackendInitialized<backend::X>> = server
        .try_transition()
        .await
        .expect("failed initializing X backend");
    let mut listening = server.try_transition().await.expect("failed listening");

    loop {
        let accepted: Server<Accepted<_>> =
            try_transition_continue_async!(listening, listening, |e| async move {
                log::error!("Error accepting CC: {}", e)
            });
        let client_authed = try_transition_continue_async!(accepted, listening, |e| async move {
            log::error!("Error authenticating client: {}", e)
        });

        let running = try_transition_continue_async!(client_authed, listening, |e| async move {
            log::error!("Error starting session: {}", e)
        });

        let session_complete = match running.try_transition().await {
            Ok(session_complete) => {
                log::info!("Session ended successfully");
                session_complete
            }
            Err(Recovered { stateful, error }) => {
                log::error!("Session ended with error: {}", error);
                stateful
            }
        };

        listening = transition_async!(session_complete, Listening<_>);
    }
}
