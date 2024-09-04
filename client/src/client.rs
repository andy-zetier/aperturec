use crate::frame::*;
use crate::gtk3::{image::Image, ClientSideItcChannels, GtkUi, ItcChannels};

use aperturec_channel::{
    self as channel, client::states as channel_states, Receiver as _, Sender as _, UnifiedClient,
};
use aperturec_graphics::prelude::*;
use aperturec_protocol::common::*;
use aperturec_protocol::control::{
    server_to_client as cm_s2c, Architecture, Bitness, ClientGoodbye, ClientGoodbyeBuilder,
    ClientGoodbyeReason, ClientInfo, ClientInfoBuilder, ClientInit, ClientInitBuilder, DecoderArea,
    Endianness, Os,
};
use aperturec_protocol::event::{
    button, client_to_server as em_c2s, server_to_client as em_s2c, Button, ButtonStateBuilder,
    KeyEvent, MappedButton, PointerEvent, PointerEventBuilder,
};
use aperturec_state_machine::*;
use aperturec_trace::log;

use anyhow::Result;
use crossbeam::channel::{select, unbounded, Receiver, Sender};
use derive_builder::Builder;
use gtk::glib;
use openssl::x509::X509;
use socket2::{Domain, Socket, Type};
use std::collections::BTreeSet;
use std::env::consts;
use std::io::ErrorKind;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use std::{assert, thread};
use sysinfo::{CpuExt, CpuRefreshKind, RefreshKind, SystemExt};

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
}

pub enum UiMessage {
    Quit(String),
    CursorImage { id: usize, cursor_data: CursorData },
    CursorChange { id: usize },
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

//
// Client structs
//
#[derive(Builder, Clone, Debug, PartialEq)]
pub struct Configuration {
    pub name: String,
    pub temp_id: u64,
    pub decoder_max: u16,
    pub server_addr: String,
    pub max_fps: Duration,
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

fn get_recv_buffer_size(sock_addr: SocketAddr) -> usize {
    let sock =
        Socket::new(Domain::for_address(sock_addr), Type::DGRAM, None).expect("Socket create");
    sock.recv_buffer_size().expect("SO_RCVBUF")
}

pub struct Client {
    config: Configuration,
    id: Option<u64>,
    server_name: Option<String>,
    control_jh: Option<thread::JoinHandle<()>>,
    should_stop: Arc<AtomicBool>,
    decoder_areas: Vec<Box2D>,
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
            decoder_areas: vec![],
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
        this.setup_event_channel(ec, itc.notify_event_rx, itc.notify_control_tx, itc.ui_tx)?;
        this.setup_media_channel(mc, itc.img_tx, itc.img_rx)?;

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
            .recv_buffer_size(get_recv_buffer_size(self.local_addr.expect("local address")) as u64)
            .build()
            .expect("Failed to generate ClientInfo")
    }

    fn generate_client_init(&self) -> ClientInit {
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
            .build()
            .expect("Failed to generate ClientInit!")
    }

    fn setup_media_channel(
        &mut self,
        mut mc: channel::ClientMedia,
        img_tx: glib::Sender<Image>,
        img_rx: Receiver<Image>,
    ) -> Result<()> {
        let (mm_rx_tx, mm_rx_rx) = unbounded();
        thread::spawn::<_, Result<()>>(move || loop {
            let msg = mc.receive()?;
            mm_rx_tx.send(msg)?;
        });

        let mut framer = Framer::new(&self.decoder_areas);
        thread::spawn::<_, Result<()>>(move || loop {
            let msg = mm_rx_rx.recv()?;

            let msg = match msg.message {
                Some(msg) => msg,
                None => {
                    log::warn!("media message with empty body");
                    continue;
                }
            };

            if let Err(e) = framer.report_mm(msg) {
                log::warn!("Error processing media message: {}", e);
            }

            if !framer.has_draws() {
                continue;
            }

            loop {
                select! {
                    recv(mm_rx_rx) -> msg_res => {
                        let msg = match msg_res?.message {
                            Some(msg) => msg,
                            None => {
                                log::warn!("media message with empty body");
                                continue;
                            }
                        };
                        if let Err(e) = framer.report_mm(msg) {
                            log::warn!("Error processing media message: {}", e)
                        }
                    },
                    recv(img_rx) -> img_res => {
                        let mut img = img_res?;
                        for draw in framer.get_draws_and_reset() {
                            img.draw(&draw);
                        }
                        img_tx.send(img)?;
                        break;
                    }
                }
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
        log::debug!("{:#?}", &ci);

        let (mut client_cc_read, mut client_cc_write) = client_cc.split();
        client_cc_write.send(ci.into())?;

        log::debug!("Client Init sent, waiting for ServerInit...");
        let si = match client_cc_read.receive() {
            Ok(cm_s2c::Message::ServerInit(si)) => si,
            Ok(cm_s2c::Message::ServerGoodbye(gb)) => panic!("Server sent goodbye: {:?}", gb),
            Err(other) => panic!("Failed to read ServerInit: {:?}", other),
        };

        log::debug!("{:#?}", si);

        //
        // Update client config with info from Server
        //
        self.id = Some(si.client_id);
        self.server_name = Some(si.server_name);
        let display_size = si.display_size.expect("No display size provided");
        self.config.win_height = display_size.height;
        self.config.win_width = display_size.width;

        let decoder_ids_valid = si
            .decoder_areas
            .iter()
            .map(|da| da.decoder_id)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .eq(0..si.decoder_areas.len() as u32);
        if !decoder_ids_valid {
            panic!("Decoder IDs are not contiguous from 0..N");
        }

        for decoder_area in si.decoder_areas.into_iter() {
            log::debug!("Decoder area: {:?}", decoder_area);
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
                self.decoder_areas.push(area);
            } else {
                panic!("Invalid decoder area: {:?}", decoder_area);
            }
        }

        assert!(
            self.decoder_areas.len() <= self.config.decoder_max.into(),
            "Server returned {} decoders, but our max is {}",
            self.decoder_areas.len(),
            self.config.decoder_max
        );

        log::info!(
            "Connected to server @ {} ({}) as client {}!",
            &self.config.server_addr,
            &self.server_name.as_ref().unwrap(),
            &self.id.unwrap()
        );

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
                            log::error!("Failed to send: {}", err);
                            break;
                        }
                    }
                    Err(err) => {
                        log::error!("Failed to receive: {}", err);
                        control_rx_tx.send(Err(err)).unwrap();
                        break;
                    }
                }
            }
            log::trace!("Control channel rx exiting");
        });

        thread::spawn(move || {
            while let Ok(cm_c2s) = control_tx_rx.recv() {
                if let Err(err) = client_cc_write.send(cm_c2s) {
                    log::error!("Failed to send: {}", err);
                    break;
                }
            }
            log::trace!("Control channel tx exiting");
        });

        //
        // Setup SIGINT handler
        //
        ctrlc::set_handler(move || {
            log::warn!("SIGINT received, exiting");
            notify_control_tx
                .send(ControlMessage::SigintReceived)
                .expect("Failed to notify control channel of SIGINT");
        })
        .unwrap_or_else(|e| log::error!("Unable to install SIGINT handler: '{}'", e));

        //
        // Setup core Control Channel thread
        //
        let client_id = self.id.unwrap();
        let should_stop = self.should_stop.clone();

        self.control_jh = Some(thread::spawn(move || {
            log::debug!("Control channel started");
            loop {
                select! {
                    recv(control_rx_rx) -> msg => match msg {
                        Ok(Ok(cm_s2c::Message::ServerGoodbye(_))) => {
                            if let Err(err) =
                                ui_tx.send(UiMessage::Quit(String::from("Goodbye!")))
                                {
                                    log::warn!("Failed to send QuitMessage: {}", err);
                                }
                            break;
                        },
                        Ok(Ok(_)) => {
                            log::warn!("Unexpected message received on control channel");
                        },
                        Ok(Err(err)) => match err.downcast::<std::io::Error>() {
                            Ok(ioe) => match ioe.kind() {
                                ErrorKind::WouldBlock | ErrorKind::Interrupted => (),
                                _ => {
                                    let _ = ui_tx
                                        .send(UiMessage::Quit(format!("{:?}", ioe)));
                                    let _ = control_tx_tx.send(ClientGoodbye::new(client_id, ClientGoodbyeReason::Terminating.into()).into());
                                    log::trace!("Sent ClientGoodbye: Terminating");
                                    log::error!("Fatal I/O error reading control message: {:?}", ioe);
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
                                log::trace!("Sent ClientGoodbye: Terminating");
                                log::error!("Fatal error reading control message: {:?}", other);
                                break;
                            }
                        },
                        Err(err) => {
                            log::error!("Failed to recv from RX ITC channel: {}", err);
                            break;
                        }
                    },
                    recv(notify_control_rx) -> msg => match msg {
                        Ok(ControlMessage::UiClosed(gm)) => {
                            control_tx_tx.send(gm.to_client_goodbye(client_id).into())
                                .unwrap_or_else(|e| log::error!("Failed to send client goodbye to control channel: {}", e));
                            log::trace!("Sent ClientGoodbye: User Requested");
                            log::info!("Disconnecting from server, sent ClientGoodbye");
                            break;
                        },
                        Ok(ControlMessage::EventChannelDied(gm)) => {
                            ui_tx.send(UiMessage::Quit("Event Channel Died".to_string()))
                                .unwrap_or_else(|e| log::error!("Failed to send QuitMessage to UI: {}", e));
                            control_tx_tx.send(gm.to_client_goodbye(client_id).into())
                                .unwrap_or_else(|e| log::error!("Failed to send client goodbye to control channel: {}", e));
                            log::trace!("Sent ClientGoodbye: Network Error");
                            break;
                        },
                        Ok(ControlMessage::SigintReceived) => {
                            ui_tx.send(UiMessage::Quit("SIGINT".to_string()))
                                .unwrap_or_else(|e| log::error!("Failed to send QuitMessage to UI: {}", e));
                            control_tx_tx.send(ClientGoodbye::new(client_id, ClientGoodbyeReason::Terminating.into()).into())
                                .unwrap_or_else(|e| log::error!("Failed to send client goodbye to control channel: {}", e));
                            log::trace!("Sent ClientGoodbye: SIGINT");
                            break;
                        },
                        Err(err) => {
                            log::error!("Failed to recv from ITC channel: {}", err);
                            break;
                        }
                    },
                }
            } // loop

            should_stop.store(true, Ordering::Relaxed);
            log::debug!("Control channel exiting");
        })); // thread::spawn

        Ok(())
    }

    fn setup_event_channel(
        &mut self,
        client_ec: channel::ClientEvent,
        notify_event_rx: Receiver<EventMessage>,
        notify_control_tx: Sender<ControlMessage>,
        ui_tx: glib::Sender<UiMessage>,
    ) -> Result<()> {
        let (mut ec_rx, mut ec_tx) = client_ec.split();
        let should_stop = self.should_stop.clone();

        thread::spawn(move || {
            log::debug!("Event channel Rx started");
            loop {
                match ec_rx.receive() {
                    Ok(msg) => match msg.try_into() {
                        Ok(cm) => {
                            if let Err(err) = ui_tx.send(cm) {
                                log::error!("Failed to send cursor message: {}", err);
                                break;
                            }
                        }
                        Err(err) => {
                            log::error!("Failed to convert event message: {}", err);
                            break;
                        }
                    },
                    Err(err) => {
                        log::error!("Failed to receive: {}", err);
                        break;
                    }
                }

                if should_stop.load(Ordering::Relaxed) {
                    break;
                }
            }
        });

        let should_stop = self.should_stop.clone();
        thread::spawn(move || {
            log::debug!("Event channel Tx started");
            for event_msg in notify_event_rx.iter() {
                let msg = match event_msg {
                    EventMessage::MouseButtonEventMessage(mbem) => {
                        em_c2s::Message::from(mbem.to_pointer_event())
                    }
                    EventMessage::PointerEventMessage(pem) => {
                        em_c2s::Message::from(pem.to_pointer_event())
                    }
                    EventMessage::KeyEventMessage(kem) => em_c2s::Message::from(kem.to_key_event()),
                };

                if should_stop.load(Ordering::Relaxed) {
                    break;
                }

                if let Err(err) = ec_tx.send(msg) {
                    let _ = notify_control_tx.send(ControlMessage::EventChannelDied(
                        GoodbyeMessage::new_network_error(),
                    ));
                    log::error!("Failed to send Event message: {}", err);
                    break;
                }
            } // for event_rx.iter()

            log::debug!("Event channel exiting");
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
            log::debug!("Adding cert: {:?}", cert);
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

    pub fn get_fps(&self) -> Duration {
        self.config.max_fps
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
        client.get_fps(),
        &client.decoder_areas,
    );

    client.shutdown();

    Ok(())
}

#[cfg(test)]
mod test {
    use super::*;
    use aperturec_channel::UnifiedServer;
    use aperturec_protocol::control::ServerInitBuilder;

    use std::sync::Once;

    fn setup() {
        static INIT: Once = Once::new();
        INIT.call_once(|| {
            aperturec_trace::Configuration::new("test")
                .initialize()
                .expect("trace init");
        });
    }

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
            .max_fps(Duration::from_secs((1 / 30u16).into()))
            .build()
            .expect("Failed to build Configuration!")
    }

    fn generate_default_configuration() -> Configuration {
        generate_configuration(1234, 4, 800, 600, 8765)
    }

    #[test]
    fn client_initialization() {
        setup();
        let config = generate_default_configuration();
        let client = Client::new(&config);

        assert_eq!(client.config, config);
    }

    #[test]
    fn startup() {
        setup();
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

        let mut config = generate_configuration(
            1234,
            8,
            1920,
            1080,
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
                    .display_size(Dimension::new(800, 600))
                    .decoder_areas(vec![])
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
