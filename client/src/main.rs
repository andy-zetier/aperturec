use aperturec_client::metrics;
use aperturec_client::{client, gtk3};
use aperturec_metrics::exporters::{CsvExporter, Exporter, LogExporter, PushgatewayExporter};
use aperturec_trace::{self as trace, log, Level};

use anyhow::Result;
use clap::Parser;
use gethostname::gethostname;
use std::time::Duration;
use sysinfo::{CpuRefreshKind, RefreshKind, SystemExt};

const DEFAULT_RESOLUTION: (u64, u64) = (800, 600);

#[derive(Debug, clap::Args)]
#[group(required = false, multiple = false)]
pub struct ResolutionGroup {
    /// Display size specified as WIDTHxHEIGHT [default 800x600].
    #[arg(short, long)]
    resolution: Option<String>,

    /// Set resolution to your primary display's current size and startup in fullscreen mode.
    /// Fullscreen mode can be toggled at any time with Ctrl+Alt+Enter
    #[arg(short, long, action = clap::ArgAction::SetTrue)]
    fullscreen: bool,
}

impl From<ResolutionGroup> for (u64, u64) {
    fn from(g: ResolutionGroup) -> (u64, u64) {
        match (g.resolution, g.fullscreen) {
            (Some(resolution), false) => {
                let dims: Vec<u64> = resolution
                    .to_lowercase()
                    .split('x')
                    .map(|d| {
                        d.parse().unwrap_or_else(|e| {
                            panic!("Failed to parse resolution '{}': {}", resolution, e)
                        })
                    })
                    .collect();
                if dims.len() != 2 {
                    panic!("Invalid resolution: {}", resolution);
                }
                (dims[0], dims[1])
            }
            (None, true) => {
                let full = gtk3::get_fullscreen_dims();
                (full.0 as u64, full.1 as u64)
            }
            (None, false) => DEFAULT_RESOLUTION,
            (Some(_), true) => unreachable!("screen-size and fullscreen are both set"),
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

    /// Initial port to bind for the decoders. A block of decoder_max ports must be available
    /// starting with this one. A value of 0 will defer port selection to the OS
    #[arg(short = 'p', long = "port", default_value_t = 0)]
    decoder_port_start: u16,

    /// Maximum frames per second.
    #[arg(long = "fps", default_value_t = 30)]
    fps_max: u16,

    /// Duration in seconds between subsequent keepalive attempts. This value may need to be
    /// tweaked depending on your NAT configuration.
    #[arg(short = 'k', default_value_t = 23)]
    keepalive_timeout: u64,

    /// Log metric data to a CSV file at the provided path
    #[arg(long, default_value = None)]
    metrics_csv: Option<String>,

    /// Log metric data at the DEBUG level (-vvv)
    #[arg(long)]
    metrics_log: bool,

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

    /// Initial ID of the client. Must match the server's --temp-client-id.
    #[arg(short, long, default_value_t = 1234)]
    temp_client_id: u64,

    /// Log level verbosity, defaults to Warning if not specified. Multiple -v options increase the
    /// verbosity. The maximum is 3.
    #[arg(short, action = clap::ArgAction::Count)]
    verbosity: u8,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let log_verbosity = match args.verbosity {
        0 => Level::WARN,
        1 => Level::INFO,
        2 => Level::DEBUG,
        _ => Level::TRACE,
    };
    trace::Configuration::new("client")
        .cmdline_verbosity(log_verbosity)
        .initialize()?;

    log::info!("ApertureC Client Startup");

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
        .decoder_port_start(args.decoder_port_start)
        .win_height(height)
        .win_width(width)
        .id(args.temp_client_id)
        .max_fps(Duration::from_secs_f32(1.0 / (args.fps_max as f32)))
        .keepalive_timeout(Duration::from_secs(args.keepalive_timeout));
    if let Some(program_cmdline) = args.program_cmdline {
        config_builder.program_cmdline(program_cmdline);
    }
    let config = config_builder.build()?;

    log::debug!("{:#?}", config);

    let mut metrics_started = false;
    if args.metrics_log || args.metrics_csv.is_some() || args.metrics_pushgateway.is_some() {
        let mut exporters: Vec<Exporter> = vec![];
        if args.metrics_log {
            match LogExporter::new(Level::DEBUG) {
                Ok(le) => exporters.push(Exporter::Log(le)),
                Err(err) => log::warn!("Failed to setup Log exporter: {}, disabling", err),
            }
        }
        if let Some(path) = args.metrics_csv {
            match CsvExporter::new(path) {
                Ok(csve) => exporters.push(Exporter::Csv(csve)),
                Err(err) => log::warn!("Failed to setup CSV exporter: {}, disabling", err),
            }
        }
        if let Some(url) = args.metrics_pushgateway {
            match PushgatewayExporter::new(
                url.to_owned(),
                "aperturec_client".to_owned(),
                args.decoder_port_start,
            ) {
                Ok(pge) => exporters.push(Exporter::Pushgateway(pge)),
                Err(err) => log::warn!("Failed to setup Pushgateway exporter: {}, disabling", err),
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
