use aperturec_server::backend;
use aperturec_server::metrics;
use aperturec_server::server::*;
use aperturec_state_machine::*;
use aperturec_utils::log;

use anyhow::Result;
use clap::Parser;
use futures::future;
use gethostname::gethostname;
use std::path::PathBuf;
use tracing::*;
use tracing_subscriber::prelude::*;

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

    #[clap(flatten)]
    tls: TlsGroup,

    #[clap(flatten)]
    log: log::LogArgGroup,

    /// Log metric data at the DEBUG level (-vvv)
    #[arg(long)]
    metrics_log: bool,

    /// Log metric data to a CSV file at the provided path
    #[arg(long, default_value = None)]
    metrics_csv: Option<String>,

    /// Serve Prometheus metrics at the given (optional) bind address. Defaults to 127.0.0.1:8080
    #[arg(long, num_args = 0..=1, default_value = None, default_missing_value = "127.0.0.1:8080")]
    metrics_prometheus: Option<String>,

    /// Send metric data to Pushgateway instance at the provided URL
    #[arg(long, default_value = None)]
    metrics_pushgateway: Option<String>,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let args = Args::parse();

    let (log_layer, _log_guard) = args.log.as_tracing_layer()?;
    #[allow(unused_mut)]
    let mut layers = vec![log_layer.boxed()];
    #[cfg(feature = "tokio-console")]
    {
        layers.push(console_subscriber::spawn().boxed());
    }
    tracing_subscriber::registry().with(layers).init();

    info!("ApertureC Server Startup");

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
        args.metrics_prometheus,
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
        .tls_configuration(args.tls.into())
        .allow_client_exec(args.allow_client_exec);

    if let Some(root_program_cmdline) = args.root_program_cmdline.into() {
        config_builder.root_process_cmdline(root_program_cmdline);
    }
    let config = config_builder.build()?;

    let server = Server::<Created>::new(config)?;

    let backend_init = try_transition_async!(server, BackendInitialized::<backend::X>)
        .map_err(|recovered| recovered.error)
        .expect("failed initializing X backend");
    let listening = try_transition!(backend_init, Listening::<_>)
        .map_err(|recovered| recovered.error)
        .expect("failed listening");
    let mut session_terminated: Server<SessionTerminated<_>> =
        transition!(listening, SessionTerminated::<_>);

    let main_task = tokio::task::spawn(async {
        loop {
            let listening = match session_terminated.try_transition() {
                Ok(listening) => listening,
                Err(recovered) => {
                    error!("Error preparing the server to listen: {}", recovered.error);
                    break Err::<(), _>(recovered.error);
                }
            };

            let accepted = try_transition_continue_async!(listening, session_terminated, |e| {
                future::ready(error!("Error accepting client: {}", e))
            });

            let remote_addr = accepted.remote_addr()?;
            info!(
                "Client connected from {}:{}",
                remote_addr.ip(),
                remote_addr.port()
            );

            let authenticated =
                try_transition_continue_async!(accepted, session_terminated, |e| future::ready(
                    error!("Error authenticating client: {}", e)
                ));
            info!("Client authenticated");

            let running = try_transition_continue_async!(authenticated, session_terminated, |e| {
                future::ready(error!("Error starting session: {}", e))
            });

            session_terminated = match running.try_transition().await {
                Ok(session_complete) => session_complete,
                Err(Recovered { stateful, error }) => {
                    error!("Session ended with error: {}", error);
                    stateful
                }
            };
            info!(
                "Client disconnected from {}:{}",
                remote_addr.ip(),
                remote_addr.port()
            );
        }
    });

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            info!("Received Ctrl-C, exiting");
            aperturec_metrics::stop();
            Ok(())
        }
        main_task_res = main_task => {
            match main_task_res {
                Ok(Ok(())) => warn!("main task exited naturally"),
                Ok(Err(e)) => error!("main task exited with error: {}", e),
                Err(e) => error!("main task panicked with error: {}", e),
            }
            panic!("main task exited");
        }
    }
}
