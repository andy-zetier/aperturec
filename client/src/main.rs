use aperturec_client::client;

use anyhow::Result;
use clap::Parser;
use gethostname::gethostname;
use simple_logger::SimpleLogger;
use std::time::Duration;
use sysinfo::{CpuRefreshKind, RefreshKind, SystemExt};

#[derive(Parser, Debug)]
struct Args {
    /// Maximum number of decoders to use. Each decoder will consume one thread and one UDP port. A
    /// value of 0 uses the number of CPU cores available.
    #[arg(short, long, default_value_t = 0)]
    decoder_max: u16,

    /// Maximum frames per second.
    #[arg(long = "fps", default_value_t = 30)]
    fps_max: u16,

    /// Display size specified as WIDTHxHEIGHT.
    #[arg(index = 2, default_value = "800x600")]
    screen_size: String,

    /// Log level verbosity, defaults to Error if not specified. Multiple -v options increase the
    /// verbosity. The maximum is 4.
    #[arg(short, action = clap::ArgAction::Count)]
    verbosity: u8,

    /// IP address of the server including control channel port. Eg. 10.10.10.11:46454 or
    /// [::1]:46454
    #[arg(index = 1)]
    server_address: String,

    /// Initial ID of the client. Must match the server's --temp-client-id.
    #[arg(short, long, default_value_t = 1234)]
    temp_client_id: u64,

    /// Initial UDP port number for the decoders. A total of decoder_max ports must be open
    /// starting with this one.
    #[arg(short = 'p', long = "port", default_value_t = 46454)]
    decoder_port_start: u64,
}

fn main() -> Result<()> {
    let args = Args::parse();
    SimpleLogger::new()
        .with_level(match args.verbosity {
            0 => log::LevelFilter::Error,
            1 => log::LevelFilter::Warn,
            2 => log::LevelFilter::Info,
            3 => log::LevelFilter::Debug,
            _ => log::LevelFilter::Trace,
        })
        .init()
        .expect("Failed to initialize logging");

    log::info!("ApertureC Client Startup");

    let decoder_max = if args.decoder_max != 0 {
        args.decoder_max
    } else {
        let sys = sysinfo::System::new_with_specifics(
            RefreshKind::new().with_cpu(CpuRefreshKind::everything()),
        );
        sys.cpus().len().try_into().unwrap()
    };

    let dims: Vec<u64> = args
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

    let config = client::ConfigurationBuilder::default()
        .decoder_max(decoder_max)
        .name(gethostname().into_string().unwrap())
        .server_addr(args.server_address.parse().expect(
                "Invalid server address. Must include control channel port number. eg '65.56.128.5:12345'"))
        .bind_address(format!(
                "{}:{}",
                if args.server_address.starts_with("127.0.0.1:") {
                    String::from("127.0.0.1")
                } else {
                    String::from("0.0.0.0")
                },
                args.decoder_port_start).parse().expect("Invalid bind_address or port_start"))
        .win_height(dims[1])
        .win_width(dims[0])
        .id(args.temp_client_id)
        .max_fps(Duration::from_secs_f32(1.0/(args.fps_max as f32)))
        .build()?;

    log::debug!("{:#?}", config);

    client::run_client(config.clone())?;

    Ok(())
}
