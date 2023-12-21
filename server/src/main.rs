use anyhow::Result;
use aperturec_protocol::common_types::ClientId;
use aperturec_server::backend;
use aperturec_server::metrics;
use aperturec_server::server::*;
use aperturec_state_machine::*;
use aperturec_trace::{self as trace, log, Level};
use clap::Parser;
use gethostname::gethostname;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// External IP address for the server to listen on
    #[arg(short, long, default_value = "0.0.0.0")]
    bind_address: String,

    /// Display size specified as WIDTHxHEIGHT.
    #[arg(short, long, default_value = format!("{}x{}", backend::X::DEFAULT_MAX_WIDTH, backend::X::DEFAULT_MAX_HEIGHT))]
    screen_size: String,

    /// Server instance name
    #[arg(short, long, default_value = gethostname())]
    name: String,

    /// Port the control channel will listen on
    #[arg(short, long, default_value = "46452")]
    control_port: u16,

    /// Port the event channel will listen on
    #[arg(short, long, default_value = "46453")]
    event_port: u16,

    /// Initial port to bind for the encoders. A block of sequential ports must be available for
    /// each encoder starting with this one. A value of 0 will defer port selection to the OS
    #[arg(short = 'p', default_value = "46454")]
    encoder_port_start: u16,

    /// Initial ID that a client can use to connect to the server
    #[arg(short, long, default_value = "1234")]
    temp_client_id: u64,

    /// Initial client program to launch (likely a desktop environment)
    #[arg(short, long, default_value_t = String::from("gnome-shell"))]
    initial_program: String,

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
        args.control_port,
    );

    let config = ConfigurationBuilder::default()
        .control_channel_addr(format!("{}:{}", args.bind_address, args.control_port).parse()?)
        .event_channel_addr(format!("{}:{}", args.bind_address, args.event_port).parse()?)
        .media_channel_addr(format!("{}:{}", args.bind_address, args.encoder_port_start).parse()?)
        .name(args.name)
        .temp_client_id(ClientId(args.temp_client_id))
        .initial_program(args.initial_program)
        .max_width(dims[0])
        .max_height(dims[1])
        .build()?;

    let server = Server::<Created>::new(config);
    let server: Server<BackendInitialized<backend::X>> = server
        .try_transition()
        .await
        .expect("failed initializing X backend");
    let mut listening = server.try_transition().await.expect("failed listening");

    loop {
        let cc_accepted: Server<ControlChannelAccepted<_>> = try_transition_continue!(
            listening,
            listening,
            |e| log::error!("Error accepting CC: {}", e)
        );
        let client_authed = try_transition_continue!(cc_accepted, listening, |e| log::error!(
            "Error authenticating client: {}",
            e
        ));
        let channels_accepted = try_transition_continue!(
            client_authed,
            listening,
            |e| log::error!("Error accepting EC: {}", e)
        );

        let running = try_transition_continue!(channels_accepted, listening, |e| log::error!(
            "Error starting session: {}",
            e
        ));

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

        listening = transition!(session_complete, ChannelsListening<_>);
    }
}
