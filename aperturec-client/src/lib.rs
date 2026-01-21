pub mod args;
pub mod config;
pub mod state;

mod channels;
#[cfg(feature = "ffi-lib")]
pub mod ffi;
pub mod frame;
mod metrics;

use crate::args::Args;
use crate::channels::event::Cursor;
use crate::frame::Draw;

use aperturec_graphics::{display::*, prelude::*};
use aperturec_protocol::control;
use aperturec_utils::warn_early;
use config::Configuration;
use state::LockState;

use anyhow::Result;
use clap::Parser;
use crossbeam::channel::RecvError;
use gethostname::gethostname;
use openssl::x509::X509;
use secrecy::SecretString;
use std::env;
use std::error::Error;
use std::fs;
use std::time::Duration;
use tracing::*;
use tracing_subscriber::prelude::*;

/// Display mode for the client window or fullscreen configuration.
#[derive(PartialEq, Clone, Copy, Debug)]
pub enum DisplayMode {
    /// Windowed mode with specified dimensions.
    Windowed { size: Size },
    /// Single-monitor fullscreen using the primary display.
    SingleFullscreen,
    /// Multi-monitor fullscreen spanning all enabled displays.
    MultiFullscreen,
}

const DEFAULT_RESOLUTION: Size = Size::new(800, 600);

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

pub fn run() -> Result<()> {
    let args = if env::args().count() == 2 {
        let arg1 = env::args().nth(1).unwrap();
        if let Ok(parsed) = Args::from_uri(&arg1) {
            parsed
        } else if let Ok(uri) = env::var("AC_URI") {
            warn_early!(
                "CLI arguments are ignored when using AC_URI. Unset AC_URI if you would like to use CLI arguments."
            );
            Args::from_uri(&uri)?
        } else {
            Args::parse()
        }
    } else if let Ok(uri) = env::var("AC_URI") {
        if env::args().count() > 1 {
            warn_early!(
                "CLI arguments are ignored when using AC_URI. Unset AC_URI if you would like to use CLI arguments."
            );
        }
        Args::from_uri(&uri)?
    } else {
        Args::parse()
    };

    let (log_layer, _guard) = args.log.as_tracing_layer()?;
    tracing_subscriber::registry().with(log_layer).init();

    info!("ApertureC Client Startup");

    let config = {
        // Scope config_builder to ensure it is dropped and any auth-token leaves memory
        let mut config_builder = config::ConfigurationBuilder::default();
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

    unimplemented!("run client");
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
