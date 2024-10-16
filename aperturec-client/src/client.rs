use crate::frame::*;
use crate::gtk3::{image::Image, ClientSideItcChannels, GtkUi, ItcChannels, LockState};

use aperturec_channel::{
    self as channel, client::states as channel_states, Receiver as _, Sender as _, UnifiedClient,
};
use aperturec_graphics::prelude::*;
use aperturec_protocol::common::*;
use aperturec_protocol::control::{
    server_to_client as cm_s2c, Architecture, Bitness, ClientGoodbye, ClientGoodbyeBuilder,
    ClientGoodbyeReason, ClientInfo, ClientInfoBuilder, ClientInit, ClientInitBuilder, Endianness,
    Os,
};
use aperturec_protocol::event::{
    button, client_to_server as em_c2s, server_to_client as em_s2c, Button, ButtonStateBuilder,
    DisplayEvent, KeyEvent, MappedButton, PointerEvent, PointerEventBuilder,
};
use aperturec_state_machine::*;

use anyhow::{anyhow, bail, Result};
use crossbeam::channel::{never, select, unbounded, Receiver, Sender};
use derive_builder::Builder;
use gtk::glib;
use openssl::x509::X509;
use std::collections::{BTreeMap, BTreeSet};
use std::env::consts;
use std::io::ErrorKind;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::{assert, thread};
use sysinfo::{CpuExt, CpuRefreshKind, RefreshKind, SystemExt};
use tracing::*;

const VERSION_MAJOR: &str = env!("CARGO_PKG_VERSION_MAJOR");
const VERSION_MINOR: &str = env!("CARGO_PKG_VERSION_MINOR");
const VERSION_PATCH: &str = env!("CARGO_PKG_VERSION_PATCH");

//
// Internal ITC channel messaging
//
#[derive(Builder, Clone, Copy, Default)]
pub struct MouseButtonEventMessage {
    button: u32,
    is_pressed: bool,
    pos: Point,
}

impl MouseButtonEventMessage {
    pub fn to_pointer_event(&self) -> PointerEvent {
        PointerEventBuilder::default()
            .button_states(vec![ButtonStateBuilder::default()
                .button(MouseButtonEventMessage::button_from_code(self.button))
                .is_depressed(self.is_pressed)
                .build()
                .expect("Failed to build ButtonState")])
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

#[derive(Builder, Clone, Copy, Default)]
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

#[derive(Builder, Clone, Copy, Default)]
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

#[derive(Builder, Clone, Copy, Debug, Default)]
pub struct DisplayEventMessage {
    size: (u32, u32),
}

impl DisplayEventMessage {
    pub fn to_display_event(&self) -> DisplayEvent {
        DisplayEvent {
            display_size: Some(Dimension::new(self.size.0.into(), self.size.1.into())),
        }
    }
}

#[derive(Builder, Clone, Default)]
pub struct GoodbyeMessage {
    reason: String,
}

impl GoodbyeMessage {
    const NETWORK_ERROR: &'static str = "Network Error";
    const TERMINATING: &'static str = "Terminating";

    pub fn to_client_goodbye(&self, client_id: u64) -> ClientGoodbye {
        match self.reason.as_str() {
            GoodbyeMessage::NETWORK_ERROR => ClientGoodbyeBuilder::default()
                .client_id(client_id)
                .reason(ClientGoodbyeReason::NetworkError)
                .build()
                .expect("Build ClientGoodbye"),
            GoodbyeMessage::TERMINATING => ClientGoodbyeBuilder::default()
                .client_id(client_id)
                .reason(ClientGoodbyeReason::Terminating)
                .build()
                .expect("Build ClientGoodbye"),
            _ => ClientGoodbyeBuilder::default()
                .client_id(client_id)
                .reason(ClientGoodbyeReason::UserRequested)
                .build()
                .expect("Build ClientGoodbye"),
        }
    }

    pub fn new(reason: String) -> Self {
        GoodbyeMessageBuilder::default()
            .reason(reason)
            .build()
            .expect("Build GoodbyeMessage")
    }

    pub fn new_network_error() -> Self {
        Self::new(Self::NETWORK_ERROR.to_string())
    }

    pub fn new_terminating() -> Self {
        Self::new(Self::TERMINATING.to_string())
    }
}

pub enum EventMessage {
    MouseButtonEventMessage(MouseButtonEventMessage),
    PointerEventMessage(PointerEventMessage),
    KeyEventMessage(KeyEventMessage),
    DisplayEventMessage(DisplayEventMessage),
}

pub enum UiMessage {
    Quit(String),
    CursorImage { id: usize, cursor_data: CursorData },
    CursorChange { id: usize },
    DisplayChange { dc_id: u64, display_size: Size },
}

impl TryFrom<em_s2c::Message> for UiMessage {
    type Error = anyhow::Error;
    fn try_from(msg: em_s2c::Message) -> Result<Self> {
        match msg {
            em_s2c::Message::CursorChange(cc) => Ok(UiMessage::CursorChange {
                id: cc.id.try_into()?,
            }),
            em_s2c::Message::CursorImage(ci) => Ok(UiMessage::CursorImage {
                id: ci.id.try_into()?,
                cursor_data: CursorData {
                    data: ci.data,
                    width: ci.width.try_into()?,
                    height: ci.height.try_into()?,
                    x_hot: ci.x_hot.try_into()?,
                    y_hot: ci.y_hot.try_into()?,
                },
            }),
            em_s2c::Message::DisplayConfiguration(dc) => {
                let config: DisplayConfig = dc.try_into()?;
                Ok(config.into())
            }
        }
    }
}

impl From<DisplayConfig> for UiMessage {
    fn from(dc: DisplayConfig) -> UiMessage {
        UiMessage::DisplayChange {
            dc_id: dc.id,
            display_size: dc.display_size,
        }
    }
}

#[derive(Debug)]
pub struct CursorData {
    pub data: Vec<u8>,
    pub width: i32,
    pub height: i32,
    pub x_hot: i32,
    pub y_hot: i32,
}

pub enum ControlMessage {
    UiClosed(GoodbyeMessage),
    EventChannelDied(GoodbyeMessage),
    SigintReceived,
}

pub struct DisplayConfig {
    pub id: u64,
    pub display_size: Size,
    pub areas: BTreeMap<u32, Box2D>,
}

impl TryFrom<DisplayConfiguration> for DisplayConfig {
    type Error = anyhow::Error;
    fn try_from(display_config: DisplayConfiguration) -> Result<DisplayConfig> {
        let decoder_ids_valid = display_config
            .areas
            .keys()
            .copied()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .eq(0..display_config.areas.len() as u32);
        if !decoder_ids_valid {
            bail!("Decoder IDs are not contiguous from 0..N");
        }

        let mut areas = BTreeMap::new();

        for (id, decoder_area) in display_config.areas.into_iter() {
            debug!(?decoder_area);
            if let DecoderArea {
                location: Some(location),
                dimension: Some(dimension),
                ..
            } = decoder_area
            {
                let area = Rect {
                    origin: Point::new(location.x_position as usize, location.y_position as usize),
                    size: Size::new(dimension.width as usize, dimension.height as usize),
                }
                .to_box2d();
                areas.insert(id, area);
            } else {
                bail!("Invalid decoder area: {:?}", decoder_area);
            }
        }

        let display_size = display_config
            .display_size
            .ok_or_else(|| anyhow!("Missing display_size"))?;

        Ok(DisplayConfig {
            id: display_config.id,
            display_size: Size::new(display_size.width as usize, display_size.height as usize),
            areas,
        })
    }
}

//
// Client structs
//
#[derive(Builder, Clone, Debug, PartialEq)]
pub struct Configuration {
    pub name: String,
    pub temp_id: u64,
    pub decoder_max: u16,
    pub server_addr: String,
    pub win_width: u64,
    pub win_height: u64,
    #[builder(setter(strip_option), default)]
    pub program_cmdline: Option<String>,
    #[builder(setter(name = "additional_tls_certificate", custom), default)]
    pub additional_tls_certificates: Vec<X509>,
    #[builder(default)]
    pub allow_insecure_connection: bool,
}

impl ConfigurationBuilder {
    pub fn additional_tls_certificate(&mut self, additional_tls_cert: X509) {
        if self.additional_tls_certificates.is_none() {
            self.additional_tls_certificates = Some(vec![]);
        }
        self.additional_tls_certificates
            .as_mut()
            .unwrap()
            .push(additional_tls_cert);
    }
}

pub struct Client {
    config: Configuration,
    id: Option<u64>,
    server_name: Option<String>,
    control_jh: Option<thread::JoinHandle<()>>,
    should_stop: Arc<AtomicBool>,
    display_config: Option<DisplayConfig>,
    local_addr: Option<SocketAddr>,
}

impl Client {
    fn new(config: &Configuration) -> Self {
        Self {
            config: config.clone(),
            should_stop: Arc::new(AtomicBool::new(false)),
            local_addr: None,
            id: None,
            server_name: None,
            control_jh: None,
            display_config: None,
        }
    }

    pub fn startup(config: &Configuration, itc: ClientSideItcChannels) -> Result<Self> {
        let mut this = Client::new(config);

        let (cc, ec, mc) = this.setup_unified_channel()?;
        this.setup_control_channel(
            cc,
            itc.ui_tx.clone(),
            itc.notify_control_tx.clone(),
            itc.notify_control_rx,
        )?;
        this.setup_event_channel(
            ec,
            itc.notify_event_rx,
            itc.notify_control_tx,
            itc.notify_media_tx,
            itc.ui_tx,
        )?;
        this.setup_media_channel(mc, itc.notify_media_rx, itc.img_tx, itc.img_rx)?;

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
            RefreshKind::new()
                .with_cpu(CpuRefreshKind::everything())
                .with_memory(),
        );
        ClientInfoBuilder::default()
            .version(SemVer::new(
                VERSION_MAJOR
                    .parse::<u64>()
                    .expect("Failed to parse version major!"),
                VERSION_MINOR
                    .parse::<u64>()
                    .expect("Failed to parse version minor!"),
                VERSION_PATCH
                    .parse::<u64>()
                    .expect("Failed to parse version patch!"),
            ))
            .build_id(build_id::get().to_hyphenated().to_string())
            .os(match consts::OS {
                "linux" => Os::Linux,
                "windows" => Os::Windows,
                "macos" => Os::Mac,
                _ => panic!("Unsupported OS"),
            })
            .os_version(sys.os_version().unwrap())
            .ssl_library("UNHANDLED".to_string())
            .ssl_version("UNHANDLED".to_string())
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
                arch => panic!("Unsupported architcture {}", arch),
            })
            .cpu_id(sys.cpus()[0].brand().to_string())
            .number_of_cores(sys.cpus().len() as u64)
            .amount_of_ram(sys.total_memory().to_string())
            .display_size(Dimension::new(
                self.config.win_width,
                self.config.win_height,
            ))
            .build()
            .expect("Failed to generate ClientInfo")
    }

    fn generate_client_init(&self) -> ClientInit {
        let lock_state = LockState::get_current();
        ClientInitBuilder::default()
            .temp_id(self.config.temp_id)
            .client_info(self.generate_client_info())
            .client_caps(self.generate_client_caps())
            .max_decoder_count::<u32>(self.config.decoder_max.into())
            .client_specified_program_cmdline(
                self.config
                    .program_cmdline
                    .clone()
                    .unwrap_or(String::from("")),
            )
            .is_caps_locked(lock_state.is_caps_locked)
            .is_num_locked(lock_state.is_num_locked)
            .is_scroll_locked(lock_state.is_scroll_locked)
            .build()
            .expect("Failed to generate ClientInit!")
    }

    fn setup_media_channel(
        &mut self,
        mut mc: channel::ClientMedia,
        notify_media_rx: Receiver<DisplayConfig>,
        img_tx: glib::Sender<Image>,
        img_rx: Receiver<Image>,
    ) -> Result<()> {
        let (mm_rx_tx, mm_rx_rx) = unbounded();
        thread::spawn::<_, Result<()>>(move || loop {
            let msg = mc.receive()?;
            mm_rx_tx.send(msg)?;
        });

        let mut framer = Framer::new(
            self.display_config
                .as_ref()
                .unwrap()
                .areas
                .values()
                .copied()
                .collect::<Vec<_>>()
                .as_slice(),
        );

        let mut display_config_id = self.display_config.as_ref().unwrap().id;

        thread::spawn::<_, Result<()>>(move || loop {
            let draw_recv = if framer.has_draws() {
                Some(&img_rx)
            } else {
                None
            };

            select! {
                recv(mm_rx_rx) -> msg_res => {
                    let msg = match msg_res?.message {
                        Some(msg) => msg,
                        None => {
                            warn!("media message with empty body");
                            continue;
                        }
                    };
                    if let Err(e) = framer.report_mm(msg) {
                        warn!("Error processing media message: {}", e)
                    }
                },
                recv(notify_media_rx) -> msg_res => {
                    let new_config = msg_res?;

                    framer = Framer::new(
                        new_config
                        .areas
                        .values()
                        .copied()
                        .collect::<Vec<_>>()
                        .as_slice(),
                    );

                    display_config_id = new_config.id;

                    debug!("Display configuration id: {}", display_config_id);
                },
                recv(draw_recv.unwrap_or(&never())) -> img_res => {
                    let mut img = img_res?;

                    if img.display_config_id != display_config_id {
                        if img.display_config_id < display_config_id {
                            debug!("Dropping Image with DCI {:?} (< {:?})", img.display_config_id, display_config_id);
                        } else {
                            warn!("Dropping Image with unexpected DCI {:?}, current DCI is {:?}", img.display_config_id, display_config_id);
                        }
                        continue;
                    }

                    for draw in framer.get_draws_and_reset() {
                        img.draw(&draw);
                    }
                    img_tx.send(img)?;
                },
            }
        });
        Ok(())
    }

    fn setup_control_channel(
        &mut self,
        client_cc: channel::ClientControl,
        ui_tx: glib::Sender<UiMessage>,
        notify_control_tx: Sender<ControlMessage>,
        notify_control_rx: Receiver<ControlMessage>,
    ) -> Result<()> {
        let ci = self.generate_client_init();
        debug!("{:#?}", &ci);

        let (mut client_cc_read, mut client_cc_write) = client_cc.split();
        client_cc_write.send(ci.into())?;

        debug!("Client Init sent, waiting for ServerInit...");
        let si = match client_cc_read.receive() {
            Ok(cm_s2c::Message::ServerInit(si)) => si,
            Ok(cm_s2c::Message::ServerGoodbye(gb)) => panic!("Server sent goodbye: {:?}", gb),
            Err(other) => panic!("Failed to read ServerInit: {:?}", other),
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
            self.display_config.as_ref().unwrap().areas.keys().len()
                <= self.config.decoder_max.into(),
            "Server returned {} decoders, but our max is {}",
            self.display_config.as_ref().unwrap().areas.len(),
            self.config.decoder_max
        );

        self.config.win_height = self.display_config.as_ref().unwrap().display_size.height as u64;
        self.config.win_width = self.display_config.as_ref().unwrap().display_size.width as u64;

        info!(
            "Connected to server @ {} ({}) as client {}!",
            &self.config.server_addr,
            &self.server_name.as_ref().unwrap(),
            &self.id.unwrap()
        );

        ui_tx
            .send(UiMessage::DisplayChange {
                dc_id: self.display_config.as_ref().unwrap().id,
                display_size: self.display_config.as_ref().unwrap().display_size,
            })
            .expect("Failed to send UI message");

        //
        // Setup Control Channel TX/RX threads
        //
        // The QUIC tx/rx threads read/write network control channel messages from/to appropriate
        // ITC channels. This allows the core Control Channel thread to select!() on bounded ITC
        // channels to drive execution and avoid mixing network and ITC reads.
        //
        let (control_tx_tx, control_tx_rx) = unbounded();
        let (control_rx_tx, control_rx_rx) = unbounded();

        thread::spawn(move || {
            loop {
                match client_cc_read.receive() {
                    Ok(cm_s2c) => {
                        if let Err(err) = control_rx_tx.send(Ok(cm_s2c)) {
                            error!("Failed to send: {}", err);
                            break;
                        }
                    }
                    Err(err) => {
                        error!("Failed to receive: {}", err);
                        control_rx_tx.send(Err(err)).unwrap();
                        break;
                    }
                }
            }
            trace!("Control channel rx exiting");
        });

        thread::spawn(move || {
            while let Ok(cm_c2s) = control_tx_rx.recv() {
                if let Err(err) = client_cc_write.send(cm_c2s) {
                    error!("Failed to send: {}", err);
                    break;
                }
            }
            trace!("Control channel tx exiting");
        });

        //
        // Setup SIGINT handler
        //
        ctrlc::set_handler(move || {
            warn!("SIGINT received, exiting");
            notify_control_tx
                .send(ControlMessage::SigintReceived)
                .expect("Failed to notify control channel of SIGINT");
        })
        .unwrap_or_else(|e| error!("Unable to install SIGINT handler: '{}'", e));

        //
        // Setup core Control Channel thread
        //
        let client_id = self.id.unwrap();
        let should_stop = self.should_stop.clone();

        self.control_jh = Some(thread::spawn(move || {
            debug!("Control channel started");
            loop {
                select! {
                    recv(control_rx_rx) -> msg => match msg {
                        Ok(Ok(cm_s2c::Message::ServerGoodbye(_))) => {
                            if let Err(err) =
                                ui_tx.send(UiMessage::Quit(String::from("Goodbye!")))
                                {
                                    warn!("Failed to send QuitMessage: {}", err);
                                }
                            break;
                        },
                        Ok(Ok(_)) => {
                            warn!("Unexpected message received on control channel");
                        },
                        Ok(Err(err)) => match err.downcast::<std::io::Error>() {
                            Ok(ioe) => match ioe.kind() {
                                ErrorKind::WouldBlock | ErrorKind::Interrupted => (),
                                _ => {
                                    let _ = ui_tx
                                        .send(UiMessage::Quit(format!("{:?}", ioe)));
                                    let _ = control_tx_tx.send(ClientGoodbye::new(client_id, ClientGoodbyeReason::Terminating.into()).into());
                                    trace!("Sent ClientGoodbye: Terminating");
                                    error!("Fatal I/O error reading control message: {:?}", ioe);
                                    break;
                                }
                            },
                            Err(other) => {
                                let _ = ui_tx
                                    .send(UiMessage::Quit(format!("{:?}", other)));
                                let _ = control_tx_tx.send(ClientGoodbye::new(
                                            client_id,
                                            ClientGoodbyeReason::Terminating.into(),
                                            ).into());
                                trace!("Sent ClientGoodbye: Terminating");
                                error!("Fatal error reading control message: {:?}", other);
                                break;
                            }
                        },
                        Err(err) => {
                            error!("Failed to recv from RX ITC channel: {}", err);
                            break;
                        }
                    },
                    recv(notify_control_rx) -> msg => match msg {
                        Ok(ControlMessage::UiClosed(gm)) => {
                            control_tx_tx.send(gm.to_client_goodbye(client_id).into())
                                .unwrap_or_else(|e| error!("Failed to send client goodbye to control channel: {}", e));
                            trace!("Sent ClientGoodbye: User Requested");
                            info!("Disconnecting from server, sent ClientGoodbye");
                            break;
                        },
                        Ok(ControlMessage::EventChannelDied(gm)) => {
                            ui_tx.send(UiMessage::Quit("Event Channel Died".to_string()))
                                .unwrap_or_else(|e| error!("Failed to send QuitMessage to UI: {}", e));
                            control_tx_tx.send(gm.to_client_goodbye(client_id).into())
                                .unwrap_or_else(|e| error!("Failed to send client goodbye to control channel: {}", e));
                            trace!("Sent ClientGoodbye: Network Error");
                            break;
                        },
                        Ok(ControlMessage::SigintReceived) => {
                            ui_tx.send(UiMessage::Quit("SIGINT".to_string()))
                                .unwrap_or_else(|e| error!("Failed to send QuitMessage to UI: {}", e));
                            control_tx_tx.send(ClientGoodbye::new(client_id, ClientGoodbyeReason::Terminating.into()).into())
                                .unwrap_or_else(|e| error!("Failed to send client goodbye to control channel: {}", e));
                            trace!("Sent ClientGoodbye: SIGINT");
                            break;
                        },
                        Err(err) => {
                            error!("Failed to recv from ITC channel: {}", err);
                            break;
                        }
                    },
                }
            } // loop

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
        notify_media_tx: Sender<DisplayConfig>,
        ui_tx: glib::Sender<UiMessage>,
    ) -> Result<()> {
        let (mut ec_rx, mut ec_tx) = client_ec.split();
        let should_stop = self.should_stop.clone();

        thread::spawn::<_, Result<()>>(move || {
            debug!("Event channel Rx started");
            loop {
                let msg = ec_rx.receive()?;

                if let em_s2c::Message::DisplayConfiguration(ref display_config) = msg {
                    notify_media_tx.send((*display_config).clone().try_into()?)?;
                }

                ui_tx.send(msg.try_into()?)?;

                if should_stop.load(Ordering::Relaxed) {
                    break Ok(());
                }
            }
        });

        let should_stop = self.should_stop.clone();
        thread::spawn(move || {
            debug!("Event channel Tx started");
            for event_msg in notify_event_rx.iter() {
                let msg = match event_msg {
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

                if let Err(err) = ec_tx.send(msg) {
                    let _ = notify_control_tx.send(ControlMessage::EventChannelDied(
                        GoodbyeMessage::new_network_error(),
                    ));
                    error!("Failed to send Event message: {}", err);
                    break;
                }
            } // for event_rx.iter()

            debug!("Event channel exiting");
        }); // thread::spawn

        Ok(())
    }

    fn setup_unified_channel(
        &mut self,
    ) -> Result<(
        channel::ClientControl,
        channel::ClientEvent,
        channel::ClientMedia,
    )> {
        let (server_addr, server_port) = match self.config.server_addr.rsplit_once(':') {
            Some((addr, port)) => (addr, Some(port.parse()?)),
            None => (&*self.config.server_addr, None),
        };

        let mut channel_builder = channel::client::Builder::default().server_addr(server_addr);
        if let Some(port) = server_port {
            channel_builder = channel_builder.server_port(port);
        }
        for cert in &self.config.additional_tls_certificates {
            debug!("Adding cert: {:?}", cert);
            channel_builder = channel_builder
                .additional_tls_pem_certificate(&String::from_utf8_lossy(&cert.to_pem()?));
        }
        if self.config.allow_insecure_connection {
            channel_builder = channel_builder.allow_insecure_connection();
        }
        let channel = channel_builder.build_sync()?;
        let channel = try_transition!(channel, channel_states::Connected).map_err(|r| r.error)?;
        self.local_addr = Some(channel.local_addr()?);
        let channel = try_transition!(channel, channel_states::Ready).map_err(|r| r.error)?;
        let (cc, ec, mc, _) = channel.split();
        Ok((cc, ec, mc))
    }

    pub fn get_height(&self) -> i32 {
        self.config.win_height.try_into().unwrap()
    }

    pub fn get_width(&self) -> i32 {
        self.config.win_width.try_into().unwrap()
    }
}

pub fn run_client(config: Configuration) -> Result<()> {
    //
    // Create ITC channels
    //
    let itc = ItcChannels::new();

    //
    // Create Client and start up channels
    //
    let client = Client::startup(&config, itc.client_half)?;

    //
    // Start up the UI on main thread
    //
    GtkUi::run_ui(
        itc.gtk_half,
        client.get_width(),
        client.get_height(),
        client
            .display_config
            .as_ref()
            .unwrap()
            .areas
            .values()
            .copied()
            .collect::<Vec<_>>()
            .as_slice(),
    );

    client.shutdown();

    Ok(())
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::gtk3;
    use aperturec_channel::UnifiedServer;
    use aperturec_protocol::control::ServerInitBuilder;

    fn generate_configuration(
        temp_id: u64,
        dec_max: u16,
        width: u64,
        height: u64,
        server_port: u16,
    ) -> Configuration {
        ConfigurationBuilder::default()
            .temp_id(temp_id)
            .decoder_max(dec_max)
            .name(String::from("test_client"))
            .server_addr(format!("127.0.0.1:{}", server_port))
            .win_width(width)
            .win_height(height)
            .build()
            .expect("Failed to build Configuration!")
    }

    fn generate_default_configuration() -> Configuration {
        generate_configuration(1234, 4, 800, 600, 8765)
    }

    #[test]
    fn client_initialization() {
        let config = generate_default_configuration();
        let client = Client::new(&config);

        assert_eq!(client.config, config);
    }

    #[test]
    fn startup() {
        let material =
            channel::tls::Material::ec_self_signed::<_, &str>([], []).expect("tls material");
        let pem_material: channel::tls::PemMaterial =
            material.clone().try_into().expect("convert to PEM");
        let qserver = channel::server::Builder::default()
            .bind_addr("0.0.0.0:0")
            .tls_pem_certificate(&pem_material.certificate)
            .tls_pem_private_key(&pem_material.pkey)
            .build_sync()
            .expect("Create qserver");

        let (width, height) = gtk3::get_fullscreen_dims();

        let mut config = generate_configuration(
            1234,
            8,
            width.try_into().unwrap(),
            height.try_into().unwrap(),
            qserver.local_addr().expect("local addr").port(),
        );
        config
            .additional_tls_certificates
            .push(material.certificate);
        let _sthread = thread::spawn(move || {
            let qserver = try_transition!(qserver).expect("server listen");
            let qserver = try_transition!(qserver).expect("server ready");
            let (mut cc, _ec, _mc, _residual) = qserver.split();
            let _ = cc.receive().expect("Receive ClientInit");
            cc.send(
                ServerInitBuilder::default()
                    .client_id(7890_u64)
                    .server_name(String::from("fake quic server"))
                    .display_configuration(DisplayConfiguration::new(
                        0,
                        Some(Dimension::new(800, 600)),
                        BTreeMap::new(),
                    ))
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
