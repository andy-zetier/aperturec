// Public modules
pub mod args;
pub mod config;
pub mod state;

// Internal modules (private by default, expose what's needed)
mod channels;
pub mod client;
#[cfg(feature = "ffi-lib")]
pub mod ffi;
pub mod frame;
pub mod gtk3;
pub mod metrics;

// Re-exports for convenience
pub use crate::channels::event::Cursor;
pub use crate::frame::Draw;
pub use aperturec_metrics::exporters::Exporter as MetricsExporter;

use aperturec_graphics::{display::*, prelude::*};
use aperturec_protocol::control;
use aperturec_utils::{self as utils, warn_early};
use config::Configuration;
use state::LockState;

use anyhow::{Result, anyhow, ensure};
use clap::Parser;
use crossbeam::channel::RecvError;
use gethostname::gethostname;
use openssl::x509::X509;
use secrecy::SecretString;
use std::env;
use std::error::Error;
use std::fs;
use std::iter;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::thread;
use std::time::Duration;
use tracing::*;
use tracing_subscriber::prelude::*;
use url::Url;

/// Display mode for the client window or fullscreen configuration.
///
/// Note: This enum is temporarily duplicated from `client::DisplayMode`.
/// The duplication will be resolved when the implementation is completed by
/// migrating the client module to use this public API version.
#[derive(PartialEq, Clone, Copy, Debug)]
pub enum DisplayMode {
    /// Windowed mode with specified dimensions.
    Windowed { size: Size },
    /// Single-monitor fullscreen using the primary display.
    SingleFullscreen,
    /// Multi-monitor fullscreen spanning all enabled displays.
    MultiFullscreen,
}

/// Reason for connection termination.
#[derive(Debug)]
pub enum QuitReason {
    /// Server initiated disconnect with an explanation.
    ServerGoodbye { server_reason: String },
    /// Connection ended due to an unrecoverable error.
    UnrecoverableError(Box<dyn Error + Send + Sync + 'static>),
}

/// Events that can be received from the server.
#[derive(Debug)]
pub enum Event {
    /// Screen update with pixel data to render.
    Draw(Draw),
    /// Cursor appearance or visibility changed.
    CursorChange(Cursor),
    /// Display configuration changed.
    DisplayChange(DisplayConfiguration),
    /// Connection terminated.
    Quit(QuitReason),
}

/// Errors that can occur when polling or waiting for events.
#[derive(Debug, thiserror::Error)]
pub enum EventError {
    /// Event stream has been exhausted.
    #[error("event stream exhausted")]
    Exhausted,
}

/// Errors that can occur during connection establishment.
#[derive(Debug, thiserror::Error)]
pub enum ConnectionError {
    /// TLS stack error.
    #[error("error in TLS stack")]
    Tls(#[from] openssl::error::ErrorStack),
    /// Channel build error.
    #[error(transparent)]
    ChannelBuild(#[from] aperturec_channel::endpoint::BuildError),
    /// Channel connect error.
    #[error(transparent)]
    ChannelConnect(#[from] aperturec_channel::endpoint::ConnectError),
    /// QUIC protocol error.
    #[error(transparent)]
    Quic(#[from] aperturec_channel::quic::Error),
    /// Inter-thread communication receive failed.
    #[error("ITC receive failed")]
    ITCReceive(#[from] RecvError),
    /// Control channel error.
    #[error("Control channel error: {0}")]
    Control(String),
    /// Failed to send control notification.
    #[error("Failed to send control notification: {0}")]
    NotifyControl(String),
    /// Failed to send media notification.
    #[error("Failed to send media notification: {0}")]
    NotifyMedia(String),
    /// Failed to send tunnel notification.
    #[error("Failed to send tunnel notification: {0}")]
    NotifyTunnel(String),
    /// Unexpected server behavior.
    #[error("unexpected server behavior: {0}")]
    UnexpectedServerBehavior(String),
}

/// Errors that can occur when sending input events.
#[derive(Debug, thiserror::Error)]
pub enum InputError {
    /// Connection primary thread died.
    #[error("connection primary thread died")]
    ThreadDied,
}

/// Errors that can occur during metrics initialization.
#[derive(Debug, thiserror::Error)]
pub enum MetricsError {
    /// Metrics have already been initialized.
    #[error("Metrics already initialized")]
    AlreadyInitialized,
    /// No exporters were provided.
    #[error("No exporters provided")]
    NoExporters,
    /// Metrics initialization failed.
    #[error("Metrics initialization failed: {0}")]
    InitFailed(#[from] anyhow::Error),
}

/// Initialize metrics for the aperturec client library.
///
/// This must be called before creating any clients if metrics are desired.
/// Can only be called once per process lifetime.
///
/// # Arguments
///
/// * `exporters` - Collection of metric exporters to use
///
/// # Errors
///
/// Returns an error if metrics are already initialized or if initialization fails.
pub fn init_metrics(
    _exporters: impl IntoIterator<Item = MetricsExporter>,
) -> Result<(), MetricsError> {
    todo!()
}

/// Returns whether metrics have been initialized.
///
/// # Returns
///
/// `true` if metrics have been initialized, `false` otherwise.
pub fn metrics_initialized() -> bool {
    todo!()
}

/// ApertureC client instance.
///
/// The client manages connection setup and lifecycle. Use this to create
/// connections to an ApertureC server.
pub struct Client;

impl Client {
    /// Creates a new ApertureC client.
    ///
    /// # Arguments
    ///
    /// * `config` - Client configuration
    /// * `lock_state` - Initial keyboard lock state
    /// * `initial_displays_request` - Initial display configuration to request
    ///
    /// # Returns
    ///
    /// A new client instance ready to connect.
    pub fn new<'a>(
        _config: Configuration,
        _lock_state: LockState,
        _initial_displays_request: impl IntoIterator<Item = &'a Display>,
    ) -> Self {
        todo!()
    }

    /// Connects the client to the remote ApertureC server.
    ///
    /// This establishes the connection and performs the initial handshake.
    ///
    /// # Returns
    ///
    /// A `Connection` object that can be used to interact with the server.
    ///
    /// # Errors
    ///
    /// Returns an error if the connection cannot be established.
    pub fn connect(&mut self) -> Result<Connection, ConnectionError> {
        todo!()
    }
}

/// Active connection to an ApertureC server.
///
/// This represents an established connection that can send input events
/// and receive display updates from the server.
pub struct Connection;

impl Connection {
    /// Returns the current display configuration.
    ///
    /// # Returns
    ///
    /// The current display configuration from the server.
    pub fn display_configuration(&self) -> DisplayConfiguration {
        todo!()
    }

    /// Disconnects from the remote server.
    ///
    /// This performs a graceful disconnect.
    pub fn disconnect(self) {
        todo!()
    }

    /// Disconnects from the remote server with a specific reason.
    ///
    /// # Arguments
    ///
    /// * `reason` - The reason for disconnection to send to the server
    pub fn disconnect_with_reason(self, _reason: control::ClientGoodbyeReason) {
        todo!()
    }

    /// Polls for a client event without blocking.
    ///
    /// # Returns
    ///
    /// `Ok(Some(event))` if an event is available, `Ok(None)` if no event is ready,
    /// or an error if the event stream is exhausted.
    pub fn poll_event(&self) -> Result<Option<Event>, EventError> {
        todo!()
    }

    /// Waits for a client event, blocking until one is available.
    ///
    /// # Returns
    ///
    /// The next event from the server.
    ///
    /// # Errors
    ///
    /// Returns an error if the event stream is exhausted.
    pub fn wait_event(&self) -> Result<Event, EventError> {
        todo!()
    }

    /// Waits for a client event with a timeout.
    ///
    /// # Arguments
    ///
    /// * `timeout` - Maximum time to wait for an event
    ///
    /// # Returns
    ///
    /// `Ok(Some(event))` if an event arrives before the timeout,
    /// `Ok(None)` if the timeout expires, or an error if the stream is exhausted.
    pub fn wait_event_timeout(&self, _timeout: Duration) -> Result<Option<Event>, EventError> {
        todo!()
    }

    /// Sends a pointer motion event to the remote server.
    ///
    /// # Arguments
    ///
    /// * `x` - X coordinate in screen space
    /// * `y` - Y coordinate in screen space
    ///
    /// # Errors
    ///
    /// Returns an error if the input thread has died.
    pub fn pointer_move(&self, _x: usize, _y: usize) -> Result<(), InputError> {
        todo!()
    }

    /// Sends a mouse button press event to the remote server.
    ///
    /// # Arguments
    ///
    /// * `code` - Mouse button code
    /// * `x` - X coordinate in screen space
    /// * `y` - Y coordinate in screen space
    ///
    /// # Errors
    ///
    /// Returns an error if the input thread has died.
    pub fn mouse_button_press(&self, _code: u32, _x: usize, _y: usize) -> Result<(), InputError> {
        todo!()
    }

    /// Sends a mouse button release event to the remote server.
    ///
    /// # Arguments
    ///
    /// * `code` - Mouse button code
    /// * `x` - X coordinate in screen space
    /// * `y` - Y coordinate in screen space
    ///
    /// # Errors
    ///
    /// Returns an error if the input thread has died.
    pub fn mouse_button_release(&self, _code: u32, _x: usize, _y: usize) -> Result<(), InputError> {
        todo!()
    }

    /// Sends a key press event to the remote server.
    ///
    /// # Arguments
    ///
    /// * `code` - Key code
    ///
    /// # Errors
    ///
    /// Returns an error if the input thread has died.
    pub fn key_press(&self, _code: u32) -> Result<(), InputError> {
        todo!()
    }

    /// Sends a key release event to the remote server.
    ///
    /// # Arguments
    ///
    /// * `code` - Key code
    ///
    /// # Errors
    ///
    /// Returns an error if the input thread has died.
    pub fn key_release(&self, _code: u32) -> Result<(), InputError> {
        todo!()
    }

    /// Requests a display configuration change from the remote server.
    ///
    /// # Arguments
    ///
    /// * `displays` - The new display configuration to request
    ///
    /// # Errors
    ///
    /// Returns an error if the input thread has died.
    pub fn request_display_change<'a>(
        &self,
        _displays: impl IntoIterator<Item = &'a Display>,
    ) -> Result<(), InputError> {
        todo!()
    }
}

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
#[command(author, about, disable_version_flag = true)]
struct Args {
    /// Print version information
    #[arg(short = 'V', long = "version", action = clap::ArgAction::SetTrue)]
    version: bool,

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
    log: utils::args::log::LogArgGroup,

    #[clap(flatten)]
    auth_token: utils::args::auth_token::AuthTokenAllArgGroup,

    #[clap(flatten)]
    metrics: utils::args::metrics::MetricsArgGroup,
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

pub fn run(frontend_name: &str, frontend_version: &str) -> Result<()> {
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

    let version = format!(
        "{} v{} ({} v{})",
        frontend_name,
        frontend_version,
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION"),
    );

    if args.version {
        println!("{version}");
        return Ok(());
    }

    let (log_layer, _guard) = args.log.as_tracing_layer()?;
    tracing_subscriber::registry().with(log_layer).init();

    info!("{version}");

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
