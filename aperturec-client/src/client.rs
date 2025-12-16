use crate::frame::*;
use crate::gtk3::show_blocking_error;
use crate::gtk3::{self, ClientSideItcChannels, GtkUi, ItcChannels, LockState};
use crate::metrics::EventChannelSendLatency;

use aperturec_channel::{
    self as channel, Flushable, Receiver as _, Sender as _, TimeoutReceiver as _, Unified,
};
use aperturec_graphics::{
    display::{self, Display},
    euclid_collections::*,
    geometry::*,
};
use aperturec_metrics::time;
use aperturec_protocol::common::*;
use aperturec_protocol::control::{
    self as cm, Architecture, Bitness, ClientGoodbye, ClientGoodbyeBuilder, ClientGoodbyeReason,
    ClientInfo, ClientInfoBuilder, ClientInit, ClientInitBuilder, Endianness, Os,
    client_to_server as cm_c2s, server_to_client as cm_s2c,
};
use aperturec_protocol::event::{
    self, Button, ButtonStateBuilder, DisplayEvent, KeyEvent, MappedButton, PointerEvent,
    PointerEventBuilder, button, client_to_server as em_c2s, server_to_client as em_s2c,
};
use aperturec_protocol::tunnel;
use aperturec_utils::log::*;

use anyhow::{Error, Result, anyhow, bail};
use async_channel::Sender as AsyncSender;
use crossbeam::channel::{Receiver, RecvTimeoutError, Sender, bounded, select, unbounded};
use derive_builder::Builder;
<<<<<<< HEAD
use openssl::x509::X509;
use secrecy::{ExposeSecret, SecretString, zeroize::Zeroize};
=======
use gtk::glib;
>>>>>>> 2eb05c82 (working through major refactor removing client module)
use std::collections::BTreeMap;
use std::env::consts;
use std::io::{ErrorKind, prelude::*};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::num::NonZeroUsize;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use std::{assert, thread};
use strum::EnumDiscriminants;
use sysinfo::{CpuRefreshKind, MemoryRefreshKind, RefreshKind, System};
use tracing::*;

const DEFAULT_CHANNEL_TIMEOUT: Duration = Duration::from_millis(1);

//
// Internal ITC channel messaging
//
#[derive(Builder, Clone, Copy, Default, Debug)]
pub struct MouseButtonEventMessage {
    button: u32,
    is_pressed: bool,
    pos: Point,
}

impl MouseButtonEventMessage {
    pub fn to_pointer_event(&self) -> PointerEvent {
        PointerEventBuilder::default()
            .button_states(vec![
                ButtonStateBuilder::default()
                    .button(MouseButtonEventMessage::button_from_code(self.button))
                    .is_depressed(self.is_pressed)
                    .build()
                    .expect("Failed to build ButtonState"),
            ])
            .location(Location {
                x_position: self.pos.x as u64,
                y_position: self.pos.y as u64,
            })
            .cursor(Cursor::Default)
            .build()
            .expect("Failed to build PointerEvent")
    }

    fn button_from_code(button: u32) -> Button {
        let kind = Some(match MappedButton::try_from(button as i32 + 1) {
            Ok(mapped) => button::Kind::MappedButton(mapped.into()),
            Err(_) => button::Kind::UnmappedButton(button),
        });
        Button { kind }
    }
}

#[derive(Builder, Clone, Copy, Default, Debug)]
pub struct PointerEventMessage {
    pos: Point,
}

impl PointerEventMessage {
    pub fn to_pointer_event(&self) -> PointerEvent {
        PointerEventBuilder::default()
            .button_states(vec![])
            .location(Location {
                x_position: self.pos.x as u64,
                y_position: self.pos.y as u64,
            })
            .cursor(Cursor::Default)
            .build()
            .expect("Failed to build PointerEvent")
    }
}

#[derive(Builder, Clone, Copy, Default, Debug)]
pub struct KeyEventMessage {
    key: u32,
    is_pressed: bool,
}

impl KeyEventMessage {
    pub fn to_key_event(&self) -> KeyEvent {
        KeyEvent {
            key: self.key,
            down: self.is_pressed,
        }
    }
}

#[derive(Clone, Debug)]
pub struct DisplayEventMessage {
    pub(crate) displays: Vec<Display>,
}

impl DisplayEventMessage {
    pub fn to_display_event(&self) -> DisplayEvent {
        event::DisplayEvent {
            displays: self
                .displays
                .iter()
                .map(|di| DisplayInfo {
                    area: Some(
                        di.area
                            .try_into()
                            .expect("Failed to convert Rect to Rectangle"),
                    ),
                    is_enabled: di.is_enabled,
                })
                .collect(),
        }
    }
}

#[derive(Debug)]
pub enum EventMessage {
    MouseButtonEventMessage(MouseButtonEventMessage),
    PointerEventMessage(PointerEventMessage),
    KeyEventMessage(KeyEventMessage),
    DisplayEventMessage(DisplayEventMessage),
    UiClosed,
}

trait ServerGoodbyeReasonExt {
    fn friendly_str(&self) -> &'static str;
}

impl ServerGoodbyeReasonExt for s2c::ServerGoodbyeReason {
    fn friendly_str(&self) -> &'static str {
        match self {
            cm::ServerGoodbyeReason::AuthenticationFailure => {
                "Authentication failure, check auth token"
            }
            cm::ServerGoodbyeReason::ClientExecDisallowed => {
                "Server does not allow client specified applications"
            }
            cm::ServerGoodbyeReason::InactiveTimeout => "Client has been inactive for too long",
            cm::ServerGoodbyeReason::NetworkError => "A network error has occurred",
            cm::ServerGoodbyeReason::OtherLogin => "Server was logged into elsewhere",
            cm::ServerGoodbyeReason::ProcessExited => "Remote application has terminated",
            cm::ServerGoodbyeReason::ProcessLaunchFailed => {
                "Failed to launch remote application, check server logs"
            }
            cm::ServerGoodbyeReason::ShuttingDown => "Server is shutting down",
            _ => self.as_str_name(),
        }
    }
}

enum ControlMessage {
    Quit(QuitReason),
}

pub enum TunnelState {
    Opening {
        queued: Vec<u8>,
        should_terminate: Arc<AtomicBool>,
    },
    Opened {
        to_tx: Sender<Vec<u8>>,
        should_terminate: Arc<AtomicBool>,
    },
    HalfClosed,
    FullyClosed,
}

#[derive(PartialEq, Clone, Copy, Debug)]
pub enum DisplayMode {
    Windowed { size: Size },
    SingleFullscreen,
    MultiFullscreen,
}

impl DisplayMode {
    pub fn is_fullscreen(&self) -> bool {
        matches!(
            self,
            DisplayMode::SingleFullscreen | DisplayMode::MultiFullscreen
        )
    }
}

//
// Client structs
//

impl Client {
    fn new(config: &Configuration) -> Self {
        Self {
            config: config.clone(),
            monitor_geometry: None,
            should_stop: Arc::new(AtomicBool::new(false)),
            local_addr: None,
            id: None,
            server_name: None,
            control_jh: None,
            display_config: None,
            requested_tunnels: generate_tunnel_requests(
                &config.client_bound_tunnel_reqs,
                &config.server_bound_tunnel_reqs,
            ),
            allocated_tunnels: BTreeMap::default(),
        }
    }

    pub fn startup(config: &Configuration, itc: ClientSideItcChannels) -> Result<Self> {
        let mut this = Client::new(config);

        this.monitor_geometry = Some(gtk3::get_monitors_geometry()?);
        debug!(monitor_geometry=?this.monitor_geometry);

        let (notify_control_tx, notify_control_rx) = unbounded();

        let (cc, ec, mc, tc) = this.setup_unified_channel()?;
        this.setup_control_channel(
            cc,
            itc.ui_tx.clone(),
            notify_control_tx.clone(),
            notify_control_rx,
        )?;
        this.setup_event_channel(
            ec,
            itc.notify_event_rx,
            notify_control_tx,
            itc.notify_media_tx,
            itc.ui_tx.clone(),
        )?;
        this.setup_media_channel(mc, itc.notify_media_rx, itc.ui_tx)?;
        this.setup_tunnel_channel(tc)?;

        Ok(this)
    }

    pub fn shutdown(mut self) {
        if self.control_jh.is_some() {
            let _ = self.control_jh.take().unwrap().join();
        }
    }

    fn generate_client_caps(&self) -> ClientCaps {
        ClientCaps {
            supported_codecs: vec![Codec::Raw.into(), Codec::Zlib.into(), Codec::Jpegxl.into()],
        }
    }

    fn generate_client_info(&self) -> ClientInfo {
        let sys = sysinfo::System::new_with_specifics(
            RefreshKind::nothing()
                .with_cpu(CpuRefreshKind::everything())
                .with_memory(MemoryRefreshKind::everything()),
        );
        let displays = {
            let single_fs = || {
                let size = self
                    .monitor_geometry
                    .as_ref()
                    .unwrap()
                    .iter()
                    .next()
                    .expect("no monitors")
                    .usable_area
                    .size;
                vec![DisplayInfo {
                    area: Some(
                        Rectangle::try_from_size_at_origin(size).expect("Failed to generate area"),
                    ),
                    is_enabled: true,
                }]
            };
            match self.config.initial_display_mode {
                DisplayMode::Windowed { size } => {
                    vec![DisplayInfo {
                        area: Some(
                            Rectangle::try_from_size_at_origin(size)
                                .expect("Failed to generate area"),
                        ),
                        is_enabled: true,
                    }]
                }
                #[cfg(not(any(target_os = "windows", target_os = "macos")))]
                DisplayMode::MultiFullscreen => self
                    .monitor_geometry
                    .as_ref()
                    .unwrap()
                    .iter()
                    .map(|MonitorGeometry { total_area, .. }| DisplayInfo {
                        area: Some(
                            Rectangle::try_from(total_area).expect("Failed to generate area"),
                        ),
                        is_enabled: true,
                    })
                    .collect(),
                #[cfg(not(any(target_os = "windows", target_os = "macos")))]
                DisplayMode::SingleFullscreen => single_fs(),
                #[cfg(any(target_os = "windows", target_os = "macos"))]
                DisplayMode::MultiFullscreen | DisplayMode::SingleFullscreen => single_fs(),
            }
        };

        ClientInfoBuilder::default()
            .version(SemVer::from_cargo().expect("extract version from cargo"))
            .os(match consts::OS {
                "linux" => Os::Linux,
                "windows" => Os::Windows,
                "macos" => Os::Mac,
                "ios" => Os::Ios,
                _ => panic!("Unsupported OS"),
            })
            .os_version(System::os_version().unwrap())
            .bitness(if cfg!(target_pointer_width = "64") {
                Bitness::B64
            } else {
                Bitness::B32
            })
            .endianness(if cfg!(target_endian = "big") {
                Endianness::Big
            } else {
                Endianness::Little
            })
            .architecture(match consts::ARCH {
                "x86" => Architecture::X86,
                "x86_64" => Architecture::X86,
                "aarch64" => Architecture::Arm,
                arch => panic!("Unsupported architcture {arch}"),
            })
            .cpu_id(sys.cpus()[0].brand().to_string())
            .number_of_cores(sys.cpus().len() as u64)
            .amount_of_ram(sys.total_memory().to_string())
            .displays(displays)
            .build()
            .expect("Failed to generate ClientInfo")
    }

    fn generate_client_init(&mut self) -> ClientInit {
        let lock_state = LockState::get_current();
        let ci = ClientInitBuilder::default()
            .auth_token(self.config.auth_token.expose_secret())
            .client_info(self.generate_client_info())
            .client_caps(self.generate_client_caps())
            .max_decoder_count(self.config.decoder_max.get() as u32)
            .client_specified_program_cmdline(
                self.config
                    .program_cmdline
                    .clone()
                    .unwrap_or(String::from("")),
            )
            .is_caps_locked(lock_state.is_caps_locked)
            .is_num_locked(lock_state.is_num_locked)
            .is_scroll_locked(lock_state.is_scroll_locked)
            .tunnel_requests(self.requested_tunnels.clone())
            .build()
            .expect("Failed to generate ClientInit!");
        self.config.auth_token.zeroize();
        ci
    }

    fn setup_media_channel(
        &mut self,
        mut mc: channel::ClientMedia,
        notify_media_rx: Receiver<display::DisplayConfiguration>,
        ui_tx: AsyncSender<UiMessage>,
    ) -> Result<()> {
        let (mm_rx_tx, mm_rx_rx) = bounded(0);
        let should_stop = self.should_stop.clone();
        thread::spawn::<_, Result<()>>(move || {
            let res = loop {
                if should_stop.load(Ordering::Relaxed) {
                    break Ok(());
                }
                let msg = mc.receive_timeout(DEFAULT_CHANNEL_TIMEOUT)?;
                mm_rx_tx.send(msg)?;
            };
            should_stop.store(true, Ordering::Relaxed);
            res
        });

        let mut framer = Framer::new(self.display_config.as_ref().unwrap().clone());

        let should_stop = self.should_stop.clone();
        thread::spawn::<_, Result<()>>(move || {
            let res: Result<()> = 'outer: loop {
                if should_stop.load(Ordering::Relaxed) {
                    break Ok(());
                }

                select! {
                    recv(mm_rx_rx) -> msg_res => {
                        let Some(msg) = msg_res? else {
                            continue;
                        };

                        let Some(msg) = msg.message else {
                            warn!("media message with empty body");
                            continue;
                        };
                        if let Err(error) = framer.report_mm(msg) {
                            warn!(%error, "error processing media message");
                        }
                    },
                    recv(notify_media_rx) -> msg_res => {
                        let display_config = msg_res?;
                        if display_config.id > framer.display_config.id {
                            framer = Framer::new(display_config.clone());
                        }
                        if let Err(error) = ui_tx.try_send(UiMessage::DisplayChange { display_config }) {
                            warn!(%error, "failed sending display config to UI");
                            break Err(error.into());
                        }
                    },
                }

                if framer.has_draws() {
                    for draw in framer.get_draws_and_reset() {
                        if let Err(error) = ui_tx.try_send(UiMessage::Draw { draw }) {
                            warn!(%error, "failed sending draw to UI");
                            break 'outer Err(error.into());
                        }
                    }
                }
            };

            should_stop.store(true, Ordering::Relaxed);
            res
        });

        Ok(())
    }

    fn setup_control_channel(
        &mut self,
        client_cc: channel::ClientControl,
        ui_tx: AsyncSender<UiMessage>,
        notify_control_tx: Sender<ControlMessage>,
        notify_control_rx: Receiver<ControlMessage>,
    ) -> Result<()> {
        let (mut client_cc_read, mut client_cc_write) = client_cc.split();

        {
            // Scope to ensure client init with any auth token data is dropped
            let client_init = self.generate_client_init();
            debug!(?client_init);
            client_cc_write.send(client_init.into())?;
        }

        debug!("Client Init sent, waiting for ServerInit...");
        let si = match client_cc_read.receive() {
            Ok(cm_s2c::Message::ServerInit(si)) => si,
            Ok(cm_s2c::Message::ServerGoodbye(gb)) => bail!("{}", gb.reason().friendly_str()),
            Err(other) => bail!("Failed to read ServerInit: {other:?}"),
        };

        debug!("{:#?}", si);

        //
        // Update client config with info from Server
        //
        self.id = Some(si.client_id);
        self.server_name = Some(si.server_name);
        self.display_config = Some(
            si.display_configuration
                .expect("No decoder configuration provided")
                .try_into()?,
        );

        assert!(
            self.display_config.as_ref().unwrap().encoder_count() <= self.config.decoder_max.into(),
            "Server returned {} decoders, but our max is {}",
            self.display_config.as_ref().unwrap().encoder_count(),
            self.config.decoder_max
        );

        self.allocated_tunnels = si.tunnel_responses;

        info!(
            "Connected to server @ {} ({})!",
            &self.config.server_addr,
            &self.server_name.as_ref().unwrap(),
        );
        debug!(client_id = ?self.id.unwrap());

        //
        // Setup Control Channel TX/RX threads
        //
        // The QUIC tx/rx threads read/write network control channel messages from/to appropriate
        // ITC channels. This allows the core Control Channel thread to select!() on bounded ITC
        // channels to drive execution and avoid mixing network and ITC reads.
        //
        let (control_tx_tx, control_tx_rx) = unbounded();
        let (control_rx_tx, control_rx_rx) = unbounded();

        let should_stop = self.should_stop.clone();
        let server_addr = self.config.server_addr.clone();
        thread::spawn(move || {
            loop {
                if should_stop.load(Ordering::Relaxed) {
                    break;
                }
                match client_cc_read.receive_timeout(DEFAULT_CHANNEL_TIMEOUT) {
                    Ok(None) => continue,
                    Ok(Some(cm_s2c)) => {
                        let is_server_gb = matches!(cm_s2c, cm_s2c::Message::ServerGoodbye(_));
                        if let Err(err) = control_rx_tx.send(Ok(cm_s2c)) {
                            error!("Failed to send: {}", err);
                            break;
                        }
                        if is_server_gb {
                            trace!("server goodbye, stopping CC rx thread");
                            break;
                        }
                    }
                    Err(err) => {
                        if !should_stop.load(Ordering::Relaxed) {
                            error!(
                                "Failed to receive with error: {}. Server may already be gone",
                                err
                            );
                            control_rx_tx.send(Err(err)).unwrap_or_else(|e| {
                                warn!("failed to forward error to Tx half: {}", e)
                            });
                        }
                        break;
                    }
                }
            }
            should_stop.store(true, Ordering::Relaxed);
            trace!("Control channel rx exiting");
        });

        let should_stop = self.should_stop.clone();
        thread::spawn(move || {
            while let Ok(cm_c2s) = control_tx_rx.recv() {
                debug!(?cm_c2s);
                if should_stop.load(Ordering::Relaxed) {
                    break;
                }
                let is_client_gb = matches!(cm_c2s, cm_c2s::Message::ClientGoodbye(_));
                if is_client_gb {
                    should_stop.store(true, Ordering::Relaxed);
                }
                if let Err(err) = client_cc_write.send(cm_c2s) {
                    error!("Failed to send: {}", err);
                    break;
                }

                if is_client_gb {
                    debug!("client goodbye sent, exiting CC Tx");
                    let _ = client_cc_write.flush();
                    break;
                }
            }
            should_stop.store(true, Ordering::Relaxed);
            trace!("Control channel tx exiting");
        });

        //
        // Setup SIGINT handler
        //
        let should_stop = self.should_stop.clone();
        ctrlc::set_handler(move || {
            info!("Received Ctrl-C, exiting");
            notify_control_tx
                .send(ControlMessage::Quit(QuitReason::SigintReceived))
                .expect("Failed to notify control channel of SIGINT");
            should_stop.store(true, Ordering::Relaxed);
        })
        .unwrap_or_else(|e| error!("Unable to install SIGINT handler: '{}'", e));

        //
        // Setup core Control Channel thread
        //
        let client_id = self.id.unwrap();
        let should_stop = self.should_stop.clone();

        self.control_jh = Some(thread::spawn(move || {
            let mut quit_reason = String::from("Goodbye");
            debug!("Control channel started");
            loop {
                select! {
                    recv(control_rx_rx) -> msg => match msg {
                        Ok(Ok(cm_s2c::Message::ServerGoodbye(gb))) => {
                            let reason = gb.reason().friendly_str().to_string();
                            quit_reason = reason.clone();

                            match gb.reason() {
                                cm::ServerGoodbyeReason::ShuttingDown => warn!("{}", &reason),
                                cm::ServerGoodbyeReason::OtherLogin => info!("{}. Exiting", &reason),
                                cm::ServerGoodbyeReason::InactiveTimeout => info!("{}. Exiting", &reason),
                                _ => error!(reason),
                            };

                            ui_tx.try_send(UiMessage::ShowModal {
                                title: format!("{} disconnected", &server_addr),
                                text: reason,
                            })
                            .unwrap_or_else(|e| warn!(%e, "Failed to send ShowModal"));
                            break;
                        },
                        Ok(Ok(_)) => {
                            warn!("Unexpected message received on control channel");
                        },
                        Ok(Err(err)) => match err.downcast::<std::io::Error>() {
                            Ok(ioe) => match ioe.kind() {
                                ErrorKind::WouldBlock | ErrorKind::Interrupted => (),
                                _ => {
                                    quit_reason = format!("Couldn't read control message: {ioe:?}");
                                    error!("{}", quit_reason);
                                    ui_tx.try_send(UiMessage::ShowModal {
                                        title: "I/O Error".to_string(),
                                        text: quit_reason.clone(),
                                    })
                                    .unwrap_or_else(|e| warn!(%e, "Failed to send ShowModal"));
                                    let _ = control_tx_tx.send(ClientGoodbye::new(client_id, ClientGoodbyeReason::Terminating.into()).into());
                                    trace!("Sent ClientGoodbye: Terminating");
                                }
                            },
                            Err(other) => {
                                quit_reason = format!("Fatal error reading control message: {other:?}");
                                let _ = control_tx_tx.send(ClientGoodbye::new(
                                            client_id,
                                            ClientGoodbyeReason::Terminating.into(),
                                            ).into());
                                trace!("Sent ClientGoodbye: Terminating");
                                if !should_stop.load(Ordering::Relaxed) {
                                    error!("{}", quit_reason);
                                }
                                ui_tx.try_send(UiMessage::ShowModal {
                                    title: "Error".to_string(),
                                    text: quit_reason.clone(),
                                })
                                .unwrap_or_else(|e| warn!(%e, "Failed to send ShowModal"));
                                break;
                            }
                        },
                        Err(err) => {
                            quit_reason = format!("Failed to recv from control channel rx: {err}");
                            if !should_stop.load(Ordering::Relaxed) {
                                error!("{}", quit_reason);
                            }
                            ui_tx.try_send(UiMessage::ShowModal {
                                title: "Error".to_string(),
                                text: quit_reason.clone(),
                            })
                            .unwrap_or_else(|e| warn!(%e, "Failed to send ShowModal"));
                            break;
                        }
                    },
                    recv(notify_control_rx) -> msg => match msg {
                        Ok(ControlMessage::Quit(quit_reason)) => {
                            debug!("Received quit message");
                            control_tx_tx
                                .send(quit_reason.to_client_goodbye(client_id).into())
                                .unwrap_or_else(|error| warn!(%error, "Failed to send client goodbye to control channel"));
                            break;
                        }
                        Err(err) => {
                            error!("Failed to recv from ITC channel: {}", err);
                            break;
                        }
                    },
                }
            } // loop

            ui_tx
                .try_send(UiMessage::Quit(quit_reason))
                .unwrap_or_else(|e| warn!(%e, "Failed to send UiMessage::Quit"));

            should_stop.store(true, Ordering::Relaxed);
            debug!("Control channel exiting");
        })); // thread::spawn

        Ok(())
    }

    fn setup_event_channel(
        &mut self,
        client_ec: channel::ClientEvent,
        notify_event_rx: Receiver<EventMessage>,
        notify_control_tx: Sender<ControlMessage>,
        notify_media_tx: Sender<display::DisplayConfiguration>,
        ui_tx: AsyncSender<UiMessage>,
    ) -> Result<()> {
        let (mut ec_rx, mut ec_tx) = client_ec.split();
        let should_stop = self.should_stop.clone();

        thread::spawn::<_, Result<()>>(move || {
            debug!("Event channel Rx started");
            loop {
                if should_stop.load(Ordering::Relaxed) {
                    break;
                }
                let Some(msg) = ec_rx.receive_timeout(DEFAULT_CHANNEL_TIMEOUT)? else {
                    continue;
                };
                trace!(?msg);

                if let em_s2c::Message::DisplayConfiguration(ref display_config) = msg {
                    notify_media_tx.send((*display_config).clone().try_into()?)?;
                } else {
                    let ui_msg = UiMessage::try_from(msg)?;
                    ui_tx.try_send(ui_msg).map_err(|err| anyhow!(err))?;
                }
            }
            should_stop.store(true, Ordering::Relaxed);
            Ok(())
        });

        let should_stop = self.should_stop.clone();
        thread::spawn(move || {
            debug!("Event channel Tx started");
            for event_msg in notify_event_rx.iter() {
                match &event_msg {
                    EventMessage::PointerEventMessage(_) => trace!(?event_msg),
                    _ => debug!(?event_msg),
                }

                let msg = match event_msg {
                    EventMessage::UiClosed => {
                        if let Err(error) = ec_tx.flush() {
                            warn!(%error, "Failed flushing event channel");
                        }
                        let _ = notify_control_tx.send(ControlMessage::Quit(QuitReason::UiExited));
                        break;
                    }
                    EventMessage::MouseButtonEventMessage(mbem) => {
                        em_c2s::Message::from(mbem.to_pointer_event())
                    }
                    EventMessage::PointerEventMessage(pem) => {
                        em_c2s::Message::from(pem.to_pointer_event())
                    }
                    EventMessage::KeyEventMessage(kem) => em_c2s::Message::from(kem.to_key_event()),
                    EventMessage::DisplayEventMessage(dem) => {
                        em_c2s::Message::from(dem.to_display_event())
                    }
                };

                if should_stop.load(Ordering::Relaxed) {
                    break;
                }

                let send_res = time!(EventChannelSendLatency, ec_tx.send(msg));
                if let Err(err) = send_res {
                    notify_control_tx
                        .send(ControlMessage::Quit(QuitReason::EventChannelDied))
                        .unwrap_or_else(|error| warn!(%error, "failed to send quit message to control channel thread"));
                    error!("Failed to send Event message: {}", err);
                    break;
                }
            } // for event_rx.iter()

            debug!("Event channel exiting");
        }); // thread::spawn

        Ok(())
    }
}

pub fn run_client(config: Configuration) -> Result<()> {
    //
    // Create ITC channels
    //
    let itc = ItcChannels::new();

    //
    // Try to start the client
    //
    let client = match Client::startup(&config, itc.client_half) {
        Ok(c) => c,
        Err(e) => {
            show_blocking_error(
                &format!("Can't connect to {0}!", config.server_addr),
                &format!("{e}"),
            );
            return Err(e);
        }
    };

    let ui = match GtkUi::new(
        itc.gtk_half,
        config.initial_display_mode,
        client.monitor_geometry.as_ref().unwrap().clone(),
        client.display_config.as_ref().unwrap().clone(),
    ) {
        Ok(ui) => ui,
        Err(e) => {
            show_blocking_error("Failed to start GTK UI", &format!("{e}"));
            return Err(e);
        }
    };

    //
    // Start up the UI on main thread
    //
    ui.run();

    client.shutdown();

    Ok(())
}

#[cfg(test)]
mod test {
    use super::*;
    use aperturec_protocol::control::ServerInitBuilder;
    use test_log::test;

    fn generate_configuration(
        auth_token: SecretString,
        dec_max: NonZeroUsize,
        size: Size,
        server_port: u16,
    ) -> Configuration {
        ConfigurationBuilder::default()
            .auth_token(auth_token)
            .decoder_max(dec_max)
            .name(String::from("test_client"))
            .server_addr(format!("127.0.0.1:{server_port}"))
            .initial_display_mode(DisplayMode::Windowed { size })
            .build()
            .expect("Failed to build Configuration!")
    }

    #[test]
    fn startup() {
        let material =
            channel::tls::Material::ec_self_signed::<_, &str>([], []).expect("tls material");
        let pem_material: channel::tls::PemMaterial =
            material.clone().try_into().expect("convert to PEM");
        let mut qserver = channel::endpoint::ServerBuilder::default()
            .bind_addr("0.0.0.0:0")
            .tls_pem_certificate(&pem_material.certificate)
            .tls_pem_private_key(&pem_material.pkey)
            .build_sync()
            .expect("Create qserver");

        let mut config = generate_configuration(
            SecretString::default(),
            8.try_into().unwrap(),
            Size::new(800, 600),
            qserver.local_addr().expect("local addr").port(),
        );
        config
            .additional_tls_certificates
            .push(material.certificate);
        let _sthread = thread::spawn(move || {
            let qsession = qserver.accept().expect("server accept");
            let (mut cc, _ec, _mc, _tc) = qsession.split();
            let _ = cc.receive().expect("Receive ClientInit");
            cc.send(
                ServerInitBuilder::default()
                    .client_id(7890_u64)
                    .server_name(String::from("fake quic server"))
                    .display_configuration(
                        DisplayConfigurationBuilder::default()
                            .id(0_u64)
                            .display_decoder_infos(vec![
                                DisplayDecoderInfoBuilder::default()
                                    .display(
                                        DisplayInfoBuilder::default()
                                            .area(
                                                RectangleBuilder::default()
                                                    .location(Location::new(0, 0))
                                                    .dimension(Dimension::new(800_u64, 600_u64))
                                                    .build()
                                                    .expect("Rectangle build"),
                                            )
                                            .is_enabled(true)
                                            .build()
                                            .expect("DisplayInfo build"),
                                    )
                                    .decoder_areas(vec![
                                        RectangleBuilder::default()
                                            .location(Location::new(0, 0))
                                            .dimension(Dimension::new(800_u64, 600_u64))
                                            .build()
                                            .expect("Rectangle build"),
                                    ])
                                    .build()
                                    .expect("DisplayDecoderInfo build"),
                            ])
                            .build()
                            .expect("DisplayConfiguration build"),
                    )
                    .build()
                    .unwrap()
                    .into(),
            )
            .expect("Send ServerInit");

            //
            // Calling receive() forces the fake quic server thread to block and ensures ServerInit
            // is delivered to the Client.
            //
            let _ = cc.receive().expect("Server receive");
            panic!("Fake Quic Server Exited!");
        });

        let itc = ItcChannels::new();
        let _client = Client::startup(&config, itc.client_half).expect("startup");
    }
}
