//! ApertureC client library for connecting to remote ApertureC servers.
//!
//! This library provides the core functionality for creating ApertureC client connections,
//! handling input/output, and managing display configurations.
//!
//! # Basic Usage
//!
//! ```no_run
//! use aperturec_client::{Client, config::Configuration, state::LockState};
//! use aperturec_graphics::display::Display;
//! use aperturec_graphics::geometry::*;
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! // Create configuration
//! let config = Configuration::default();
//! let lock_state = LockState::default();
//! let displays = vec![Display::new(Rect::new(Point::origin(), Size::new(800, 600)), true)];
//!
//! // Create and connect client
//! let mut client = Client::new(config, lock_state, &displays);
//! let connection = client.connect()?;
//!
//! // Receive events and send input
//! loop {
//!     match connection.wait_event()? {
//!         aperturec_client::Event::Draw(draw) => {
//!             // Handle screen update
//!         }
//!         aperturec_client::Event::Quit(_) => break,
//!         _ => {}
//!     }
//! }
//! # Ok(())
//! # }
//! ```
//!
//! # Metrics
//!
//! The aperturec client library supports optional metrics collection and export.
//! Metrics are process-global and must be initialized once before creating clients.
//!
//! ```no_run
//! use aperturec_client::{init_metrics, MetricsExporter, Client};
//! use aperturec_metrics::exporters::LogExporter;
//! use tracing::Level;
//! # use aperturec_client::{config::Configuration, state::LockState};
//! # use aperturec_graphics::display::Display;
//! # use aperturec_graphics::geometry::*;
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! // Initialize metrics once at program start
//! let exporters = vec![
//!     MetricsExporter::Log(LogExporter::new(Level::DEBUG)?),
//! ];
//! init_metrics(exporters)?;
//!
//! // Create clients as needed - they will share the metrics infrastructure
//! # let config = Configuration::default();
//! # let lock_state = LockState::default();
//! # let displays = vec![Display::new(Rect::new(Point::origin(), Size::new(800, 600)), true)];
//! let client = Client::new(config, lock_state, &displays);
//! # Ok(())
//! # }
//! ```
//!
//! See [`aperturec_metrics::exporters`] for available exporter types and configuration options.

use crate::args::PortForwardArg;
use crate::config::Configuration;

use aperturec_channel::{self as channel, Unified};
use aperturec_graphics::{
    display::{Display, DisplayConfiguration},
    geometry::*,
};
use aperturec_protocol as proto;
use aperturec_utils::channels::SenderExt;

use crossbeam::channel::{
    Receiver, RecvError, RecvTimeoutError, SendError, Sender, TryRecvError, bounded, select_biased,
    unbounded,
};
use secrecy::{ExposeSecret, zeroize::Zeroize};
use std::{
    cell::{OnceCell, RefCell},
    collections::BTreeMap,
    env::consts,
    error::Error,
    fmt,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};
use sysinfo::System;
use tracing::*;

mod channels;
mod frame;
mod metrics;

use channels::{
    control::{Notification as ToCC, PrimaryThreadNotification as FromCC},
    event::{self as ec, Notification as ToEC, PrimaryThreadNotification as FromEC},
    media::{Notification as ToMC, PrimaryThreadNotification as FromMC},
    tunnel::{Notification as ToTC, PrimaryThreadNotification as FromTC},
};

pub mod args;
pub mod config;
#[cfg(feature = "ffi-lib")]
pub mod ffi;
pub mod state;

pub use crate::frame::Draw;
pub use channels::event::Cursor;

/// Static guard to ensure metrics are only initialized once per process.
static METRICS_INITIALIZED: AtomicBool = AtomicBool::new(false);

/// Initialize metrics for the aperturec client library.
///
/// This function initializes global metrics infrastructure and should be called
/// once per process, before creating any Client instances. Metrics are automatically
/// stopped when the process exits.
///
/// # Arguments
/// * `exporters` - Iterator of metric exporters to enable. See [`aperturec_metrics::exporters`]
///   for available exporter types.
///
/// # Returns
/// * `Ok(())` if metrics were successfully initialized
/// * `Err(MetricsError::AlreadyInitialized)` if already initialized
/// * `Err(MetricsError::NoExporters)` if no exporters provided
/// * `Err(MetricsError::InitFailed(_))` if initialization failed
///
/// # Example
/// ```no_run
/// use aperturec_client::{init_metrics, MetricsExporter};
/// use aperturec_metrics::exporters::LogExporter;
/// use tracing::Level;
///
/// // Initialize metrics once at program start
/// let exporters = vec![
///     MetricsExporter::Log(LogExporter::new(Level::DEBUG).unwrap()),
/// ];
/// init_metrics(exporters).expect("Failed to initialize metrics");
///
/// // Metrics are now active for all clients in this process
/// ```
pub fn init_metrics(
    exporters: impl IntoIterator<Item = aperturec_metrics::exporters::Exporter>,
) -> Result<(), MetricsError> {
    let mut exporters = exporters.into_iter().peekable();

    if exporters.peek().is_none() {
        return Err(MetricsError::NoExporters);
    }

    if METRICS_INITIALIZED.swap(true, Ordering::Acquire) {
        return Err(MetricsError::AlreadyInitialized);
    }

    match aperturec_metrics::MetricsInitializer::default()
        .with_poll_rate_from_secs(3)
        .with_exporters(exporters)
        .init()
    {
        Ok(_) => {
            crate::metrics::setup_client_metrics();
            Ok(())
        }
        Err(e) => {
            METRICS_INITIALIZED.store(false, Ordering::SeqCst);
            Err(MetricsError::InitFailed(e))
        }
    }
}

/// Returns whether metrics have been initialized.
///
/// This can be used to check if metrics are active before attempting to initialize them.
pub fn metrics_initialized() -> bool {
    METRICS_INITIALIZED.load(Ordering::Acquire)
}

/// Display mode for the client window.
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

#[derive(Debug, derive_more::From)]
pub(crate) enum Notification {
    Terminate,
    ToCC(ToCC),
    ToEC(ToEC),
    ToMC(ToMC),
    ToTC(ToTC),
}

/// Reason for connection termination.
#[derive(Debug)]
pub enum QuitReason {
    /// Server initiated disconnect with an explanation.
    ServerGoodbye { server_reason: String },
    /// Connection ended due to an unrecoverable error.
    UnrecoverableError(Box<dyn Error + Send + Sync + 'static>),
}

impl fmt::Display for QuitReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            QuitReason::ServerGoodbye { server_reason } => write!(f, "{server_reason}"),
            QuitReason::UnrecoverableError(e) => {
                write!(f, "Encountered an unrecoverable error: {e}")
            }
        }
    }
}

/// Events received from the server.
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

/// Errors that occur when receiving events.
#[derive(Debug, thiserror::Error)]
pub enum EventError {
    #[error("event stream exhausted")]
    Exhausted,
}

/// Errors that occur during connection establishment.
#[derive(Debug, thiserror::Error)]
pub enum ConnectionError {
    #[error("error in TLS stack")]
    Tls(#[from] openssl::error::ErrorStack),
    #[error(transparent)]
    ChannelBuild(#[from] channel::endpoint::BuildError),
    #[error(transparent)]
    ChannelConnect(#[from] channel::endpoint::ConnectError),
    #[error(transparent)]
    Quic(#[from] channel::quic::Error),
    #[error("ITC recive failed")]
    ITCReceive(#[from] RecvError),
    #[error(transparent)]
    Control(#[from] channels::control::Error),
    #[error(transparent)]
    NotifyControl(#[from] SendError<channels::control::Notification>),
    #[error(transparent)]
    NotifyMedia(#[from] SendError<channels::media::Notification>),
    #[error(transparent)]
    NotifyTunnel(#[from] SendError<channels::tunnel::Notification>),
    #[error("unexpected server behavior: {0}")]
    UnexpectedServerBehavior(String),
}

/// Errors that occur when sending input to the server.
#[derive(Debug, thiserror::Error)]
pub enum InputError {
    #[error("connection primary thread died")]
    ThreadDied,
}

/// Errors that occur during metrics initialization.
#[derive(Debug, thiserror::Error)]
pub enum MetricsError {
    #[error("Metrics already initialized")]
    AlreadyInitialized,

    #[error("No exporters provided")]
    NoExporters,

    #[error("Metrics initialization failed: {0}")]
    InitFailed(#[from] anyhow::Error),
}

#[derive(Debug)]
struct ConnectionParameters {
    client_id: u64,
    server_name: String,
    display_config: DisplayConfiguration,
    allocated_tunnels: BTreeMap<u64, proto::tunnel::Response>,
}

#[derive(Debug, thiserror::Error)]
enum ConnectionParametersError {
    #[error("missing display configuration in server init")]
    MissingDisplayConfiguration,
    #[error("invalid display configuration: {0}")]
    InvalidDisplayConfiguration(#[from] aperturec_protocol::convenience::Error),
}

impl TryFrom<proto::control::ServerInit> for ConnectionParameters {
    type Error = ConnectionParametersError;
    fn try_from(
        server_init: proto::control::ServerInit,
    ) -> Result<Self, ConnectionParametersError> {
        let display_config = server_init
            .display_configuration
            .ok_or(ConnectionParametersError::MissingDisplayConfiguration)?
            .try_into()?;

        Ok(ConnectionParameters {
            client_id: server_init.client_id,
            server_name: server_init.server_name,
            display_config,
            allocated_tunnels: server_init.tunnel_responses.into_iter().collect(),
        })
    }
}

/// Active connection to an ApertureC server.
///
/// Provides methods for receiving events from the server and sending input.
/// Automatically disconnects when dropped.
pub struct Connection {
    connection_parameters: ConnectionParameters,
    event_rx: Receiver<Event>,
    pt_tx: Sender<Notification>,
    primary_thread: Option<thread::JoinHandle<()>>,
    is_active: Arc<AtomicBool>,
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.pt_tx.send_or_warn(Notification::Terminate);
        self.primary_thread.take().map(thread::JoinHandle::join);
    }
}

impl Connection {
    /// Returns the current display configuration.
    ///
    /// This reflects the arrangement of displays, their resolutions, and the layout
    /// of decoders assigned to each display.
    pub fn display_configuration(&self) -> DisplayConfiguration {
        self.connection_parameters.display_config.clone()
    }

    /// Disconnects from the remote server.
    ///
    /// Sends a goodbye message to the server indicating the UI exited normally,
    /// then closes the connection. This is the typical way to end a session.
    pub fn disconnect(self) {
        self.disconnect_with_reason(proto::control::ClientGoodbyeReason::UserRequested)
    }

    /// Disconnects from the remote server with a specific reason.
    ///
    /// Sends a goodbye message to the server with the specified reason, then closes
    /// the connection. Use this to communicate why the connection is ending when the
    /// reason differs from a normal UI exit.
    pub fn disconnect_with_reason(self, reason: proto::control::ClientGoodbyeReason) {
        if self.is_active.load(Ordering::Acquire) {
            self.pt_tx.send_or_warn(Notification::ToCC(ToCC::Goodbye {
                reason,
                id: self.connection_parameters.client_id,
            }));
        }
    }

    /// Polls for a client event without blocking.
    ///
    /// This checks if an event is available and returns immediately. Returns `Ok(None)` if
    /// no event is available.
    ///
    /// Events include screen updates, cursor changes, display configuration changes, and quit signals.
    pub fn poll_event(&self) -> Result<Option<Event>, EventError> {
        match self.event_rx.try_recv() {
            Ok(event) => Ok(Some(event)),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => Err(EventError::Exhausted),
        }
    }

    /// Waits for a client event, blocking until one is available.
    ///
    /// This blocks the calling thread until an event becomes available. Use [`Self::poll_event`]
    /// for non-blocking operation or [`Self::wait_event_timeout`] to wait with a timeout.
    ///
    /// Events include screen updates, cursor changes, display configuration changes, and quit signals.
    pub fn wait_event(&self) -> Result<Event, EventError> {
        self.event_rx.recv().map_err(|_| EventError::Exhausted)
    }

    /// Waits for a client event with a timeout.
    ///
    /// This blocks the calling thread until either an event becomes available or the timeout
    /// expires. Returns `Ok(None)` if the timeout expires without an event.
    pub fn wait_event_timeout(&self, timeout: Duration) -> Result<Option<Event>, EventError> {
        match self.event_rx.recv_timeout(timeout) {
            Ok(event) => Ok(Some(event)),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => Err(EventError::Exhausted),
        }
    }

    /// Sends a pointer motion event to the remote server.
    ///
    /// The coordinates are in the display's coordinate space as configured by the client.
    pub fn pointer_move(&self, x: usize, y: usize) -> Result<(), InputError> {
        self.send_user_event(ec::UserEvent::Pointer {
            pos: Point::new(x, y),
        })
    }

    /// Sends a mouse button press event to the remote server.
    ///
    /// The button press should be paired with a corresponding [`Self::mouse_button_release`].
    pub fn mouse_button_press(&self, code: u32, x: usize, y: usize) -> Result<(), InputError> {
        self.set_mouse_button_state(code, x, y, true)
    }

    /// Sends a mouse button release event to the remote server.
    ///
    /// This should be paired with a corresponding [`Self::mouse_button_press`].
    pub fn mouse_button_release(&self, code: u32, x: usize, y: usize) -> Result<(), InputError> {
        self.set_mouse_button_state(code, x, y, false)
    }

    fn set_mouse_button_state(
        &self,
        code: u32,
        x: usize,
        y: usize,
        is_pressed: bool,
    ) -> Result<(), InputError> {
        self.send_user_event(ec::UserEvent::MouseButton {
            code,
            is_pressed,
            pos: Point::new(x, y),
        })
    }

    /// Sends a key press event to the remote server.
    ///
    /// The key press should be paired with a corresponding [`Self::key_release`].
    pub fn key_press(&self, code: u32) -> Result<(), InputError> {
        self.set_key_state(code, true)
    }

    /// Sends a key release event to the remote server.
    ///
    /// This should be paired with a corresponding [`Self::key_press`].
    pub fn key_release(&self, code: u32) -> Result<(), InputError> {
        self.set_key_state(code, false)
    }

    fn set_key_state(&self, code: u32, is_pressed: bool) -> Result<(), InputError> {
        self.send_user_event(ec::UserEvent::Key { code, is_pressed })
    }

    fn send_user_event(&self, ue: ec::UserEvent) -> Result<(), InputError> {
        self.pt_tx
            .send(Notification::ToEC(ToEC::UserEvent(ue)))
            .map_err(|_| InputError::ThreadDied)?;
        Ok(())
    }

    /// Requests a display configuration change from the remote server.
    pub fn request_display_change<'a>(
        &self,
        displays: impl IntoIterator<Item = &'a Display>,
    ) -> Result<(), InputError> {
        self.send_user_event(ec::UserEvent::DisplayChange {
            displays: displays.into_iter().cloned().collect(),
        })
    }
}

/// ApertureC client for connecting to remote servers.
///
/// Create a client with configuration, lock state, and initial display settings,
/// then call [`Self::connect`] to establish a connection.
pub struct Client {
    config: RefCell<Configuration>,
    lock_state: state::LockState,
    initial_displays_request: Vec<Display>,
    requested_tunnels: BTreeMap<u64, proto::tunnel::Description>,
    client_info: OnceCell<proto::control::ClientInfo>,
    client_init: OnceCell<proto::control::ClientInit>,
}

impl Client {
    fn get_init(&self) -> &proto::control::ClientInit {
        let ci = self.client_init.get_or_init(|| {
            let client_caps = proto::common::ClientCaps {
                supported_codecs: vec![
                    proto::common::Codec::Raw.into(),
                    proto::common::Codec::Zlib.into(),
                    proto::common::Codec::Jpegxl.into(),
                ],
            };
            proto::control::ClientInitBuilder::default()
                .auth_token(self.config.borrow().auth_token.expose_secret())
                .client_info(self.get_info().clone())
                .client_caps(client_caps)
                .max_decoder_count(self.config.borrow().decoder_max.get() as u32)
                .client_specified_program_cmdline(
                    self.config
                        .borrow()
                        .program_cmdline
                        .clone()
                        .unwrap_or(String::from("")),
                )
                .is_caps_locked(self.lock_state.is_caps_locked)
                .is_num_locked(self.lock_state.is_num_locked)
                .is_scroll_locked(self.lock_state.is_scroll_locked)
                .tunnel_requests(self.requested_tunnels.clone())
                .build()
                .expect("Failed to generate ClientInit!")
        });
        self.config.borrow_mut().auth_token.zeroize();
        ci
    }

    fn get_info(&self) -> &proto::control::ClientInfo {
        self.client_info.get_or_init(|| {
            let sys = sysinfo::System::new_with_specifics(
                sysinfo::RefreshKind::nothing()
                    .with_cpu(sysinfo::CpuRefreshKind::everything())
                    .with_memory(sysinfo::MemoryRefreshKind::everything()),
            );
            let displays = {
                let single_fs = || {
                    let size = self
                        .initial_displays_request
                        .first()
                        .expect("no monitors")
                        .size();
                    vec![proto::common::DisplayInfo {
                        area: Some(
                            proto::common::Rectangle::try_from_size_at_origin(size)
                                .expect("Failed to generate area"),
                        ),
                        is_enabled: true,
                    }]
                };
                match self.config.borrow().initial_display_mode {
                    DisplayMode::Windowed { size } => {
                        vec![proto::common::DisplayInfo {
                            area: Some(
                                proto::common::Rectangle::try_from_size_at_origin(size)
                                    .expect("Failed to generate area"),
                            ),
                            is_enabled: true,
                        }]
                    }
                    DisplayMode::MultiFullscreen => self
                        .initial_displays_request
                        .iter()
                        .filter(|d| d.is_enabled)
                        .map(|Display { area, .. }| proto::common::DisplayInfo {
                            area: Some((*area).try_into().expect("Failed to generate area")),
                            is_enabled: true,
                        })
                        .collect(),
                    DisplayMode::SingleFullscreen => single_fs(),
                }
            };

            proto::control::ClientInfoBuilder::default()
                .version(proto::common::SemVer::from_cargo().expect("extract version from cargo"))
                .os(match consts::OS {
                    "linux" => proto::control::Os::Linux,
                    "windows" => proto::control::Os::Windows,
                    "macos" => proto::control::Os::Mac,
                    "ios" => proto::control::Os::Ios,
                    _ => panic!("Unsupported OS"),
                })
                .os_version(System::os_version().unwrap())
                .bitness(if cfg!(target_pointer_width = "64") {
                    proto::control::Bitness::B64
                } else {
                    proto::control::Bitness::B32
                })
                .endianness(if cfg!(target_endian = "big") {
                    proto::control::Endianness::Big
                } else {
                    proto::control::Endianness::Little
                })
                .architecture(match consts::ARCH {
                    "x86" => proto::control::Architecture::X86,
                    "x86_64" => proto::control::Architecture::X86,
                    "aarch64" => proto::control::Architecture::Arm,
                    arch => panic!("Unsupported architcture {arch}"),
                })
                .cpu_id(sys.cpus()[0].brand().to_string())
                .number_of_cores(sys.cpus().len() as u64)
                .amount_of_ram(sys.total_memory().to_string())
                .displays(displays)
                .build()
                .expect("Failed to generate ClientInfo")
        })
    }

    /// Creates a new ApertureC client.
    ///
    /// The client is configured with connection parameters, keyboard lock state,
    /// and the initial display layout to request from the server.
    pub fn new<'a>(
        config: Configuration,
        lock_state: state::LockState,
        initial_displays_request: impl IntoIterator<Item = &'a Display>,
    ) -> Self {
        Self {
            lock_state,
            initial_displays_request: initial_displays_request.into_iter().cloned().collect(),
            requested_tunnels: PortForwardArg::into_tunnel_requests(
                &config.client_bound_tunnel_reqs,
                &config.server_bound_tunnel_reqs,
            ),
            client_info: OnceCell::new(),
            client_init: OnceCell::new(),
            config: config.into(),
        }
    }

    /// Connects the client to the remote ApertureC server.
    ///
    /// This establishes the QUIC connection, performs TLS handshake, and initiates the session.
    /// The client must be successfully connected before sending input or receiving events.
    pub fn connect(&mut self) -> Result<Connection, ConnectionError> {
        let (cc, ec, mc, tc) = self.setup_unified_channel()?;

        let (pt_tx, pt_rx) = bounded(0);
        let (event_tx, event_rx) = unbounded();

        let (to_cc_tx, to_cc_rx) = bounded(0);
        let (from_cc_tx, from_cc_rx) = bounded(0);
        channels::control::setup(cc, from_cc_tx, to_cc_rx);

        let client_init = self.get_init();
        debug!(?client_init);
        to_cc_tx.send(channels::control::Notification::Init(
            client_init.clone().into(),
        ))?;

        debug!("Client Init sent, waiting for ServerInit...");
        let server_init = match from_cc_rx.recv()? {
            FromCC::Error(error) => {
                warn!(%error);
                return Err(error.into());
            }
            FromCC::ServerInit(server_init) => server_init,
            FromCC::ServerGoodbye(reason) => {
                warn!(%reason, "received server goodbye");
                return Err(ConnectionError::UnexpectedServerBehavior(format!(
                    "received server goodbye: {reason}"
                )));
            }
        };
        debug!(?server_init);

        let connection_parameters: ConnectionParameters =
            server_init
                .try_into()
                .map_err(|e: ConnectionParametersError| {
                    ConnectionError::UnexpectedServerBehavior(e.to_string())
                })?;
        debug!(?connection_parameters);

        let config = self.config.borrow();
        if connection_parameters.display_config.encoder_count() > config.decoder_max.get() {
            return Err(ConnectionError::UnexpectedServerBehavior(format!(
                "server allocated {} decoders, but only {} are enabled in the client",
                connection_parameters.display_config.encoder_count(),
                config.decoder_max
            )));
        }

        let (to_ec_tx, to_ec_rx) = bounded(0);
        let (from_ec_tx, from_ec_rx) = bounded(0);
        channels::event::setup(ec, from_ec_tx, to_ec_rx);

        let (to_mc_tx, to_mc_rx) = bounded(0);
        let (from_mc_tx, from_mc_rx) = bounded(0);
        channels::media::setup(mc, from_mc_tx, to_mc_rx);
        to_mc_tx.send(channels::media::Notification::DisplayConfiguration(
            connection_parameters.display_config.clone(),
        ))?;

        let (to_tc_tx, to_tc_rx) = bounded(0);
        let (from_tc_tx, from_tc_rx) = bounded(0);
        channels::tunnel::setup(self.requested_tunnels.clone(), tc, from_tc_tx, to_tc_rx);
        to_tc_tx.send(channels::tunnel::Notification::Allocations(
            connection_parameters.allocated_tunnels.clone(),
        ))?;

        info!(
            "Connected to server @ {} ({})!",
            &config.server_addr, &connection_parameters.server_name,
        );

        let is_active = Arc::new(AtomicBool::new(true));
        let is_active_pt = is_active.clone();
        let primary_thread = thread::spawn(move || {
            let _ = trace_span!("primary-thread").entered();
            loop {
                select_biased! {
                    recv(pt_rx) -> pt_msg_res => {
                        let Ok(pt_msg) = pt_msg_res else {
                            warn!("connection dropped without terminating");
                            break;
                        };
                        match pt_msg {
                            Notification::Terminate => break,
                            Notification::ToCC(msg) => to_cc_tx.send_or_warn(msg),
                            Notification::ToEC(msg) => to_ec_tx.send_or_warn(msg),
                            Notification::ToMC(msg) => to_mc_tx.send_or_warn(msg),
                            Notification::ToTC(msg) => to_tc_tx.send_or_warn(msg),
                        }
                    },
                    recv(from_cc_rx) -> cc_msg_res => {
                        let Ok(cc_msg) = cc_msg_res else {
                            warn!("CC terminated unexpectedly");
                            break;
                        };

                        match cc_msg {
                            FromCC::ServerInit(si) => warn!(?si, "gratuitous server init"),
                            FromCC::ServerGoodbye(server_reason) => {
                                info!(%server_reason, "server exiting");
                                event_tx
                                    .send_or_warn(Event::Quit(QuitReason::ServerGoodbye { server_reason }));
                                break;
                            }
                            FromCC::Error(error) => {
                                error!(%error, "control channel");
                                event_tx.send_or_warn(Event::Quit(QuitReason::UnrecoverableError(
                                    error.into(),
                                )));
                                break;
                            }
                        }
                    },
                    recv(from_ec_rx) -> ec_msg_res => {
                        let Ok(ec_msg) = ec_msg_res else {
                            warn!("EC terminated unexpectedly");
                            break;
                        };

                        match ec_msg {
                            FromEC::DisplayConfiguration(dc) => {
                                to_mc_tx.send_or_warn(ToMC::DisplayConfiguration(dc.clone()));
                                event_tx.send_or_warn(Event::DisplayChange(dc));
                            }
                            FromEC::Cursor(cursor) => {
                                event_tx.send_or_warn(Event::CursorChange(cursor));
                            }
                            FromEC::Error(error) => {
                                if let ec::Error::Protocol(error) = error {
                                    warn!(%error, "event channel protocol error");
                                } else {
                                    error!(%error, "event channel");
                                    event_tx.send_or_warn(Event::Quit(QuitReason::UnrecoverableError(
                                        error.into(),
                                    )));
                                    break;
                                }
                            }
                        }
                    },
                    recv(from_mc_rx) -> mc_msg_res => {
                        let Ok(mc_msg) = mc_msg_res else {
                            warn!("MC terminated unexpectedly");
                            break;
                        };

                        match mc_msg {
                            FromMC::Draw(draw) => {
                                event_tx.send_or_warn(Event::Draw(draw));
                            }
                            FromMC::Error(error) => {
                                error!(%error, "media channel");
                                event_tx.send_or_warn(Event::Quit(QuitReason::UnrecoverableError(
                                    error.into(),
                                )));
                                break;
                            }
                        }

                    },
                    recv(from_tc_rx) -> tc_msg_res => {
                        let Ok(tc_msg) = tc_msg_res else {
                            warn!("TC terminated unexpectedly");
                            break;
                        };

                        let FromTC::ChannelError(error) = tc_msg;
                        warn!(%error, "tunnel channel");
                    }
                }
            }
            is_active_pt.store(false, Ordering::Release);
            to_cc_tx.send_or_warn(ToCC::Terminate);
            to_ec_tx.send_or_warn(ToEC::Terminate);
            to_mc_tx.send_or_warn(ToMC::Terminate);
            to_tc_tx.send_or_warn(ToTC::Terminate);
            debug!("sent termination signal to all channels");
        });

        Ok(Connection {
            event_rx,
            pt_tx,
            primary_thread: Some(primary_thread),
            connection_parameters,
            is_active,
        })
    }

    fn setup_unified_channel(
        &mut self,
    ) -> Result<
        (
            channel::ClientControl,
            channel::ClientEvent,
            channel::ClientMedia,
            channel::ClientTunnel,
        ),
        ConnectionError,
    > {
        let config = self.config.borrow();
        let server_input = config.server_addr.as_str();
        let (server_addr, server_port) =
            if let Ok(socket_addr) = server_input.parse::<std::net::SocketAddr>() {
                // Successfully parsed a SocketAddr (IPv4 or IPv6 with port)
                (socket_addr.ip().to_string(), Some(socket_addr.port()))
            } else if let Ok(ip) = server_input.parse::<std::net::IpAddr>() {
                // Parsed an IP address (v4 or v6) without a port
                (ip.to_string(), None)
            } else if let Some((host_part, port_str)) = server_input.rsplit_once(':') {
                // Assume DNS name with a port if the part after the colon is a valid u16
                if let Ok(parsed_port) = port_str.parse::<u16>() {
                    (host_part.to_string(), Some(parsed_port))
                } else {
                    (server_input.to_string(), None)
                }
            } else {
                // Default to the entire input as host with no port specified
                (server_input.to_string(), None)
            };

        let mut channel_client_builder = channel::endpoint::ClientBuilder::default();
        for cert in &config.additional_tls_certificates {
            debug!("Adding cert: {:?}", cert);
            channel_client_builder = channel_client_builder
                .additional_tls_pem_certificate(&String::from_utf8_lossy(&cert.to_pem()?));
        }
        if config.allow_insecure_connection {
            channel_client_builder = channel_client_builder.allow_insecure_connection();
        }
        let mut channel_client = channel_client_builder.build_sync()?;
        let channel_session = channel_client.connect(&server_addr, server_port)?;
        let (cc, ec, mc, tc) = channel_session.split();
        Ok((cc, ec, mc, tc))
    }
}
