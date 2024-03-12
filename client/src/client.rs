use crate::gtk3::{GtkUi, ItcChannels};
use anyhow::Result;
use aperturec_channel::reliable::tcp;
use aperturec_channel::unreliable::udp;
use aperturec_channel::{
    ClientControlChannel, ClientEventChannel, ClientMediaChannel, Receiver, Sender,
};
use aperturec_protocol::common::*;
use aperturec_protocol::control::{
    server_to_client as cm_s2c, Architecture, Bitness, ClientGoodbye, ClientGoodbyeBuilder,
    ClientGoodbyeReason, ClientInfo, ClientInfoBuilder, ClientInit, ClientInitBuilder, DecoderArea,
    DecoderSequencePair, Endianness, HeartbeatResponseBuilder, MissedFrameReportBuilder, Os,
};
use aperturec_protocol::event::{
    button, client_to_server as em_c2s, Button, ButtonStateBuilder, KeyEvent, MappedButton,
    PointerEvent, PointerEventBuilder,
};
use aperturec_protocol::media::{
    client_to_server as mm_c2s, server_to_client as mm_s2c, MediaKeepalive, Rectangle,
    RectangleUpdate,
};
use aperturec_state_machine::TryTransitionable;
use aperturec_trace::log;
use crossbeam_channel::{select, unbounded};
use derive_builder::Builder;
use std::collections::{BTreeMap, BTreeSet};
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
// Internal position representation
//
#[derive(Clone, Copy, Debug, Default)]
pub struct Point(pub u32, pub u32);

impl Point {
    pub fn to_location(&self) -> Location {
        Location {
            x_position: self.0 as u64,
            y_position: self.1 as u64,
        }
    }

    pub fn from_location(loc: &Location) -> Self {
        Self(loc.x_position as u32, loc.y_position as u32)
    }
}

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
            .location(self.pos.to_location())
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
            .location(self.pos.to_location())
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
    QuitMessage(String),
}

pub enum ControlMessage {
    UpdateReceivedMessage(u16, u64, Vec<u64>),
    UiClosed(GoodbyeMessage),
    EventChannelDied(GoodbyeMessage),
    SigintReceived,
}

//
// Async utility functions
//
// The Client is currently synchronous mostly due to GTK's limitation on threads and is unable to
// directly call async functions such as the Server and Client constructors from the
// aperturec_channels crate. To get around this, we can wrap these async calls in a managed tokio
// runtime (see `async_rt`) and return the results to synchrnous code.
//
async fn new_async_udp_client(
    remote_addr: SocketAddr,
    bind_addr: SocketAddr,
) -> udp::Client<udp::Connected> {
    udp::Client::new_blocking(remote_addr, Some(bind_addr))
        .try_transition()
        .await
        .expect("Failed to Connect")
}

async fn new_async_tcp_client_retry(
    addr: SocketAddr,
    retry: Duration,
) -> tcp::Client<tcp::Connected> {
    loop {
        let client = tcp::Client::new_blocking(addr).try_transition().await;
        match client {
            Ok(client) => return client,
            Err(err) => log::warn!(
                "Failed to connect to {}: {:?}, retrying in {:?}...",
                addr,
                err.error,
                retry
            ),
        }
        thread::sleep(retry);
    }
}

//
// Client structs
//
#[derive(Builder, Clone, Debug, PartialEq)]
pub struct Configuration {
    pub decoder_max: u16,
    pub name: String,
    pub server_addr: SocketAddr,
    pub bind_address: SocketAddr,
    pub id: u64,
    pub max_fps: Duration,
    pub keepalive_timeout: Duration,
    pub win_width: u64,
    pub win_height: u64,
    pub root_program: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct ClientDecoder {
    pub id: u16,
    pub origin: Point,
    pub width: u32,
    pub height: u32,
}

impl ClientDecoder {
    fn new(id: u16) -> Self {
        Self {
            id,
            ..Default::default()
        }
    }

    fn set_origin(&mut self, loc: Location) {
        self.origin = Point::from_location(&loc);
    }

    fn set_dims(&mut self, dims: Dimension) {
        self.width = dims.width.try_into().unwrap();
        self.height = dims.height.try_into().unwrap();
    }
}

pub struct Client {
    config: Configuration,
    decoders: BTreeMap<u16, ClientDecoder>,
    event_port: Option<u16>,
    async_rt: tokio::runtime::Runtime,
    id: u64,
    server_name: Option<String>,
    tcp_retry_interval: Duration,
    heartbeat_interval: Duration,
    heartbeat_response_interval: Duration,
    control_jh: Option<thread::JoinHandle<()>>,
    should_stop: Arc<AtomicBool>,
}

impl Client {
    fn new(config: &Configuration) -> Self {
        let decoders = BTreeMap::new();
        let id = config.id;
        let async_rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("Failed to build tokio Runtime");

        Self {
            config: config.clone(),
            decoders,
            event_port: None,
            async_rt,
            id,
            server_name: None,

            //
            // How often to retry if we can't establish the initial connection with the server
            //
            tcp_retry_interval: Duration::from_millis(5 * 1000),

            //
            // TODO: I'm undecided on initial hearbeat resonse (HBR) timing. Basing HBR interval on
            // FPS makes sense at first since this may recover missed frame data before the user
            // notices. However, this is only useful if we've missed the "last" FramebufferUpdate
            // in a series of updates as we already detect sequence number jumps in a series of
            // FramebufferUpdates. In all other cases, we're just sending a u64 back and forth at
            // FPS. This is unnecessary overhead if the visual data is mostly static or if we've
            // successfully received all outstanding FramebufferUpdates.
            //
            // Setting the interval to 10x FPS and hardcoding the response time to 10 until we come
            // up with something better.
            //
            heartbeat_interval: config.max_fps * 10,
            heartbeat_response_interval: Duration::from_millis(10 * 1000),

            control_jh: None,
            should_stop: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn startup(config: &Configuration, itc: &ItcChannels) -> Result<Self> {
        let mut this = Client::new(config);

        this.setup_control_channel(itc)?;
        this.setup_decoders(itc)?;
        this.setup_event_channel(itc)?;

        Ok(this)
    }

    pub fn shutdown(mut self) {
        if self.control_jh.is_some() {
            let _ = self.control_jh.take().unwrap().join();
        }
    }

    fn generate_client_caps(&self) -> ClientCaps {
        ClientCaps {
            supported_codecs: vec![Codec::Raw.into(), Codec::Zlib.into()],
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
        ClientInitBuilder::default()
            .temp_id(self.id)
            .client_info(self.generate_client_info())
            .client_caps(self.generate_client_caps())
            .client_heartbeat_interval::<prost_types::Duration>(
                self.heartbeat_interval
                    .try_into()
                    .expect("Failed to convert Duration"),
            )
            .client_heartbeat_response_interval::<prost_types::Duration>(
                self.heartbeat_response_interval
                    .try_into()
                    .expect("Failed to convert Duration"),
            )
            .max_decoder_count::<u32>(self.config.decoder_max.into())
            .root_program(self.config.root_program.clone().unwrap_or(String::from("")))
            .build()
            .expect("Failed to generate ClientInit!")
    }

    fn setup_decoders(&mut self, itc: &ItcChannels) -> Result<()> {
        let port_start = self.config.bind_address.port();

        assert!(
            itc.img_to_decoder_rxs.len() == self.config.decoder_max.into(),
            "img_to_decoder_rxs.len() != decoder_max ({} != {})",
            itc.img_to_decoder_rxs.len(),
            self.config.decoder_max
        );

        for (rem_port, decoder) in &self.decoders {
            let decoder_id = decoder.id;
            let remote_port = *rem_port;
            let local_port = if port_start == 0 {
                0
            } else {
                port_start + decoder_id
            };

            let mut decoder_addr = self.config.server_addr;
            decoder_addr.set_port(remote_port);

            let udp_client = self
                .async_rt
                .block_on(self.async_rt.spawn(new_async_udp_client(
                    decoder_addr,
                    SocketAddr::new(self.config.bind_address.ip(), local_port),
                )))
                .expect("Failed to create UDP client");

            let local_port = udp_client.local_addr().port();

            log::debug!("[decoder {}] {:?}", local_port, &udp_client);

            let (mut client_mc_rx, mut client_mc_tx) = ClientMediaChannel::new(udp_client).split();
            let is_data_recv: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
            let should_stop_ka = self.should_stop.clone();
            let is_data_recv_ka = is_data_recv.clone();
            let keepalive_timeout = self.config.keepalive_timeout;

            thread::spawn(move || {
                let keepalive: mm_c2s::Message =
                    MediaKeepalive::new(Decoder::new(remote_port as u32).into()).into();

                loop {
                    log::trace!("[decoder {}] Sending {:?}", local_port, &keepalive);
                    if let Err(err) = client_mc_tx.send(keepalive.clone()) {
                        if is_data_recv_ka.load(Ordering::Relaxed) {
                            log::warn!(
                                "[decoder {}] Failed to send keepalive to {}: {}",
                                local_port,
                                decoder_addr,
                                err
                            );
                        }
                    }

                    if should_stop_ka.load(Ordering::Relaxed) {
                        break;
                    }

                    if is_data_recv_ka.load(Ordering::Relaxed) {
                        thread::sleep(keepalive_timeout);
                    } else {
                        thread::sleep(Duration::from_millis(250));
                    }
                }
            });

            let img_tx = itc.img_from_decoder_tx.clone();
            let img_rx = itc.img_to_decoder_rxs[decoder_id as usize].take().unwrap();
            let control_tx = itc.notify_control_tx.clone();
            let should_stop = self.should_stop.clone();

            thread::spawn(move || {
                let mut current_seq_id = None;

                //
                // Receive an initial Image from the UI thread
                //
                let mut image = match img_rx.recv() {
                    Ok(img) => {
                        assert!(
                            img.decoder_id == decoder_id.into(),
                            "[decoder {}] Image decoder_id != decoder_id ({} != {})",
                            local_port,
                            img.decoder_id,
                            decoder_id
                        );
                        img
                    }
                    Err(err) => {
                        log::error!(
                            "[decoder {}] Fatal error receiving Image from UI: {}",
                            local_port,
                            err
                        );
                        log::debug!("[decoder {}] exiting", local_port);
                        return;
                    }
                };

                log::debug!("[decoder {}] Media channel started", local_port);
                loop {
                    //
                    // Listen for FramebufferUpdates from the server
                    //
                    let fbu = match client_mc_rx.receive() {
                        Ok(mm_s2c::Message::FramebufferUpdate(fbu)) => fbu,
                        Err(err) => match err.downcast::<std::io::Error>() {
                            Ok(ioe) => match ioe.kind() {
                                ErrorKind::WouldBlock => {
                                    // receive() should be blocking
                                    log::warn!("[decoder {}] EAGAIN", local_port);
                                    continue;
                                }
                                _ => {
                                    log::warn!("[decoder {}] IO Error: {:?}", local_port, ioe);
                                    continue;
                                }
                            },
                            Err(other) => {
                                log::warn!(
                                    "[decoder {}] Failed to process media channel message: {}",
                                    local_port,
                                    other
                                );
                                continue;
                            }
                        },
                    };

                    if should_stop.load(Ordering::Relaxed) {
                        break;
                    }

                    is_data_recv.swap(true, Ordering::Relaxed);

                    if fbu.rectangle_updates.is_empty() {
                        log::warn!(
                            "[decoder {}] Ignoring FramebufferUpdate with {} rectangle updates",
                            local_port,
                            fbu.rectangle_updates.len()
                        );
                        continue;
                    }

                    // FramebufferUpdate logging is *very* noisy
                    /*log::trace!(
                        "[decoder {}] FramebufferUpdate [{}..{}]",
                        local_port,
                        fbu.rectangle_updates.first().unwrap().sequence_id.0,
                        fbu.rectangle_updates.last().unwrap().sequence_id.0
                    );*/

                    aperturec_metrics::builtins::packet_sent(fbu.rectangle_updates.len());

                    let mut missing_seq: BTreeSet<u64> = BTreeSet::new();
                    for ru in fbu.rectangle_updates {
                        //
                        // Handle out-of-sequence RectangleUpdates
                        //
                        // TODO: I'm not sure we should simply drop out of order sequence numbers.
                        // Its possible out-of-sequence updates represent completely different
                        // decoder areas, in which case they should not be dropped. However, if
                        // they contain stale data, we don't want to include it and should drop
                        // them. Perhaps the safe thing to do here is drop them and add them to
                        // missing_seq so the latest data is re-sent? In any case, this doesn't
                        // seem to happen very often in my ad-hoc testing, so I'll leave them
                        // dropped for now with a warning.
                        //
                        if current_seq_id.map_or(false, |csi: u64| csi >= ru.sequence_id) {
                            log::warn!(
                                "[decoder {}] Dropping Rectangle {}, already received {}",
                                local_port,
                                ru.sequence_id,
                                current_seq_id.unwrap()
                            );

                            continue;
                        } else if current_seq_id.is_none() && ru.sequence_id != 0 {
                            missing_seq.extend(0..ru.sequence_id)
                        } else if let Some(current_seq_id) = current_seq_id {
                            if current_seq_id < ru.sequence_id {
                                missing_seq.extend(current_seq_id + 1..ru.sequence_id);
                            }
                        }

                        current_seq_id = Some(ru.sequence_id);

                        //
                        // Write rectangle data to the Image with the appropriate Codec
                        //
                        if let RectangleUpdate {
                            location: Some(location),
                            rectangle:
                                Some(Rectangle {
                                    codec,
                                    data,
                                    dimension: Some(dimension),
                                }),
                            ..
                        } = &ru
                        {
                            match Codec::try_from(*codec) {
                                Ok(Codec::Raw) => match image.draw_raw(
                                    data,
                                    location.x_position.into(),
                                    location.y_position.into(),
                                    dimension.width,
                                    dimension.height,
                                ) {
                                    Ok(_) => (),
                                    Err(err) => {
                                        log::warn!(
                                            "[decoder {}] Codec::Raw failed: {}",
                                            local_port,
                                            err
                                        );
                                        log::debug!("[decoder {}] {:#?}", local_port, &ru);
                                    }
                                },
                                Ok(Codec::Zlib) => match image.draw_raw_zlib(
                                    data,
                                    location.x_position.into(),
                                    location.y_position.into(),
                                    dimension.width,
                                    dimension.height,
                                ) {
                                    Ok(_) => (),
                                    Err(err) => {
                                        log::warn!(
                                            "[decoder {}] Codec::Zlib failed: {}",
                                            local_port,
                                            err
                                        );
                                        log::debug!("[decoder {}] {:#?}", local_port, &ru);
                                    }
                                },
                                _ => {
                                    log::warn!(
                                        "[decoder {}] Dropping Rectangle {}, unsupported codec",
                                        local_port,
                                        current_seq_id.unwrap()
                                    );
                                }
                            }
                        } else {
                            log::warn!(
                                "[decoder {}] Dropping rectangle {}, malformed",
                                local_port,
                                current_seq_id.unwrap()
                            );
                        }
                    }

                    //
                    // Swap Image with the UI thread
                    //
                    image = match img_tx.send(image) {
                        Ok(_) => match img_rx.recv() {
                            Ok(img) => img,
                            Err(err) => {
                                log::error!(
                                    "[decoder {}] Fatal error receiving Image from UI: {}",
                                    local_port,
                                    err
                                );
                                break;
                            }
                        },
                        Err(err) => {
                            log::error!(
                                "[decoder {}] Fatal error sending Image to UI: {}",
                                local_port,
                                err
                            );
                            break;
                        }
                    };

                    if !missing_seq.is_empty() {
                        log::debug!(
                            "[decoder {}] MFRs generated for {:?}",
                            local_port,
                            missing_seq
                        );
                        aperturec_metrics::builtins::packet_lost(missing_seq.len());
                    }
                    //
                    // Notify Control thread of last SequenceId and missing SequenceIds
                    //
                    match control_tx.send(ControlMessage::UpdateReceivedMessage(
                        remote_port,
                        current_seq_id.unwrap(),
                        missing_seq.into_iter().collect(),
                    )) {
                        Ok(_) => (),
                        Err(err) => {
                            if !should_stop.load(Ordering::Relaxed) {
                                log::error!("[decoder {}] Fatal error sending UpdateReceivedMessage to control thread: {}", local_port, err);
                            }
                            break;
                        }
                    };
                } // loop receive FramebufferUpdate

                log::debug!("[decoder {}] Media channel exiting", local_port);
            }); // thread::spawn
        } // for self.decoder

        Ok(())
    }

    fn setup_control_channel(&mut self, itc: &ItcChannels) -> Result<()> {
        let control_to_ui_tx = itc.control_to_ui_tx.clone();

        log::info!("Connecting to {}...", &self.config.server_addr);
        let tcp_client = self
            .async_rt
            .block_on(self.async_rt.spawn(new_async_tcp_client_retry(
                self.config.server_addr,
                self.tcp_retry_interval,
            )))
            .expect("Failed to create control channel TCP connection");

        let ci = self.generate_client_init();
        log::debug!("{:#?}", &ci);

        let client_cc = ClientControlChannel::new(tcp_client);
        let (mut client_cc_read, mut client_cc_write) = client_cc.split();
        client_cc_write.send(ci.into())?;

        log::debug!("Client Init sent, waiting for ServerInit...");
        let si = match client_cc_read.receive() {
            Ok(cm_s2c::Message::ServerInit(si)) => si,
            Ok(cm_s2c::Message::ServerGoodbye(gb)) => panic!("Server sent goodbye: {:?}", gb),
            Ok(_) => panic!("Unexpected message received, expected ServerInit"),
            Err(other) => panic!("Failed to read ServerInit: {:?}", other),
        };

        log::debug!("{:#?}", si);

        //
        // Update client config with info from Server
        //
        self.id = si.client_id;
        self.server_name = Some(si.server_name);
        self.event_port = Some(si.event_port.try_into().expect("Invalid event port"));
        let display_size = si.display_size.expect("No display size provided");
        self.config.win_height = display_size.height;
        self.config.win_width = display_size.width;

        for (decoder_id, decoder_area) in si.decoder_areas.into_iter().enumerate() {
            if let DecoderArea {
                decoder: Some(decoder),
                location: Some(location),
                dimension: Some(dimension),
            } = decoder_area
            {
                if let Ok(port) = decoder.port.try_into() {
                    let mut d = ClientDecoder::new(decoder_id.try_into().unwrap());
                    d.set_origin(location);
                    d.set_dims(dimension);
                    self.decoders.insert(port, d);
                } else {
                    panic!("Invalid port received from server {}", decoder.port);
                }
            } else {
                panic!("Invalid decoder area: {:?}", decoder_area);
            }
        }

        assert!(
            self.decoders.len() <= self.config.decoder_max.into(),
            "Server returned {} decoders, but our max is {}",
            self.decoders.len(),
            self.config.decoder_max
        );

        log::info!(
            "Connected to server @ {} ({}) as client {}!",
            &self.config.server_addr,
            &self.server_name.as_ref().unwrap(),
            &self.id
        );

        //
        // Setup Control Channel TX/RX threads
        //
        // The TCP tx/rx threads read/write network control channel messages from/to appropriate
        // ITC channels. This allows the core Control Channel thread to select!() on unbounded ITC
        // channels to drive execution and avoid mixing TCP and ITC reads.
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
        let control_tx = itc.notify_control_tx.clone();
        ctrlc::set_handler(move || {
            log::warn!("SIGINT received, exiting");
            control_tx
                .send(ControlMessage::SigintReceived)
                .expect("Failed to notify control channel of SIGINT");
        })
        .unwrap_or_else(|e| log::error!("Unable to install SIGINT handler: '{}'", e));

        //
        // Setup core Control Channel thread
        //
        let client_id = self.id;
        let should_stop = self.should_stop.clone();
        let control_rx = itc.notify_control_rx.take().unwrap();

        self.control_jh = Some(thread::spawn(move || {
            // Track the last received sequence id for each decoder, this is needed for HBRs
            let mut decoder_seq = BTreeMap::new();

            log::debug!("Control channel started");
            loop {
                select! {
                    recv(control_rx_rx) -> msg => match msg {
                        Ok(Ok(cm_s2c::Message::HeartbeatRequest(hr))) => {
                            log::trace!("Recv HeartbeatRequest  {}", hr.request_id);
                            let hbr = HeartbeatResponseBuilder::default()
                                .heartbeat_id(hr.request_id)
                                .last_sequence_ids(
                                    decoder_seq
                                    .iter()
                                    .map(|(d, s)| {
                                        DecoderSequencePair::new(
                                            Decoder::new(*d).into(),
                                            *s,
                                            )
                                    })
                                    .collect::<Vec<_>>(),
                                    )
                                .build()
                                .expect("Failed to generate HeartbeatResponse!");
                            match control_tx_tx.send(hbr.into()) {
                                Err(err) => log::warn!(
                                    "Failed to send HeartbeatResponse {:?}: {}",
                                    hr.request_id,
                                    err
                                    ),
                                _ => log::trace!("Sent HeartbeatResponse {}", hr.request_id),
                            };
                        },
                        Ok(Ok(cm_s2c::Message::ServerGoodbye(_))) => {
                            if let Err(err) =
                                control_to_ui_tx.send(UiMessage::QuitMessage(String::from("Goodbye!")))
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
                                    let _ = control_to_ui_tx
                                        .send(UiMessage::QuitMessage(format!("{:?}", ioe)));
                                    let _ = control_tx_tx.send(ClientGoodbye::new(client_id, ClientGoodbyeReason::Terminating.into()).into());
                                    log::trace!("Sent ClientGoodbye: Terminating");
                                    log::error!("Fatal I/O error reading control message: {:?}", ioe);
                                    break;
                                }
                            },
                            Err(other) => {
                                let _ = control_to_ui_tx
                                    .send(UiMessage::QuitMessage(format!("{:?}", other)));
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
                    recv(control_rx) -> msg => match msg {
                        Ok(ControlMessage::UpdateReceivedMessage(dec, seq, missing)) => {
                            decoder_seq.insert(dec.into(), seq);

                            let num_frames = missing.len();
                            if !missing.is_empty() {
                                let mfr = MissedFrameReportBuilder::default()
                                    .decoder(dec)
                                    .frame_sequence_ids(missing)
                                    .build()
                                    .expect("Failed to build MissedFrameReport!");

                                match control_tx_tx.send(mfr.into()) {
                                    Err(err) => log::warn!("Failed to send MissedFrameReport: {}", err),
                                    _ => {
                                        log::trace!(
                                            "Sent MissedFrameReport for Decoder {}, missed {}",
                                            dec,
                                            num_frames);
                                    }
                                };
                            }

                        },
                        Ok(ControlMessage::UiClosed(gm)) => {
                            control_tx_tx.send(gm.to_client_goodbye(client_id).into())
                                .unwrap_or_else(|e| log::error!("Failed to send client goodbye to control channel: {}", e));
                            log::trace!("Sent ClientGoodbye: User Requested");
                            log::info!("Disconnecting from server, sent ClientGoodbye");
                            break;
                        },
                        Ok(ControlMessage::EventChannelDied(gm)) => {
                            control_to_ui_tx.send(UiMessage::QuitMessage("Event Channel Died".to_string()))
                                .unwrap_or_else(|e| log::error!("Failed to send QuitMessage to UI: {}", e));
                            control_tx_tx.send(gm.to_client_goodbye(client_id).into())
                                .unwrap_or_else(|e| log::error!("Failed to send client goodbye to control channel: {}", e));
                            log::trace!("Sent ClientGoodbye: Network Error");
                            break;
                        },
                        Ok(ControlMessage::SigintReceived) => {
                            control_to_ui_tx.send(UiMessage::QuitMessage("SIGINT".to_string()))
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
                    }

                }
            } // loop

            should_stop.store(true, Ordering::Relaxed);
            log::debug!("Control channel exiting");
        })); // thread::spawn

        Ok(())
    }

    fn setup_event_channel(&self, itc: &ItcChannels) -> Result<()> {
        let event_rx = itc.event_from_ui_rx.take().unwrap();
        let mut event_addr = self.config.server_addr;
        event_addr.set_port(*self.event_port.as_ref().unwrap());
        let tcp_client = self
            .async_rt
            .block_on(self.async_rt.spawn(new_async_tcp_client_retry(
                event_addr,
                self.tcp_retry_interval,
            )))
            .expect("Failed to create event channel TCP connection");

        let mut client_ec = ClientEventChannel::new(tcp_client);

        log::info!("Connected event channel @ {}", &event_addr);

        let control_tx = itc.notify_control_tx.clone();
        let should_stop = self.should_stop.clone();

        thread::spawn(move || {
            log::debug!("Event channel started");
            for event_msg in event_rx.iter() {
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

                if let Err(err) = client_ec.send(msg) {
                    let _ = control_tx.send(ControlMessage::EventChannelDied(
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

    pub fn get_height(&self) -> i32 {
        self.config.win_height.try_into().unwrap()
    }

    pub fn get_width(&self) -> i32 {
        self.config.win_width.try_into().unwrap()
    }

    pub fn get_fps(&self) -> Duration {
        self.config.max_fps
    }

    pub fn get_decoders_as_vec(&self) -> Vec<ClientDecoder> {
        let mut dec_vec: Vec<ClientDecoder> = self.decoders.clone().into_values().collect();
        dec_vec.sort_by_key(|d| d.id);
        dec_vec
    }
}

pub fn run_client(config: Configuration) -> Result<()> {
    //
    // Create ITC channels
    //
    let itc = ItcChannels::new(&config);

    //
    // Create Client and start up channels
    //
    let client = Client::startup(&config, &itc)?;

    //
    // Start up the UI on main thread
    //
    GtkUi::run_ui(
        itc,
        client.get_width(),
        client.get_height(),
        client.get_fps(),
        client.get_decoders_as_vec().as_slice(),
    );

    client.shutdown();

    Ok(())
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::client::{Client, Configuration, ConfigurationBuilder};
    use crate::gtk3::ItcChannels;
    use aperturec_channel::reliable::tcp;
    use aperturec_channel::{Receiver, Sender, ServerControlChannel, ServerEventChannel};
    use aperturec_protocol::control::client_to_server as cm_c2s;
    use aperturec_protocol::control::*;
    use aperturec_state_machine::TryTransitionable;
    use std::net::SocketAddr;
    use std::net::UdpSocket;
    use std::sync::Once;
    use std::time::Duration;
    use tokio::runtime::Builder;

    static INIT: Once = Once::new();

    fn setup() {
        INIT.call_once(|| {
            aperturec_trace::Configuration::new("test")
                .initialize()
                .expect("trace init");
        });
    }

    fn generate_configuration(dec_max: u16, width: u64, height: u64) -> Configuration {
        ConfigurationBuilder::default()
            .decoder_max(dec_max)
            .name(String::from("test_client"))
            .server_addr("127.0.0.1:8765".parse().unwrap())
            .bind_address("127.0.0.1:0".parse().unwrap())
            .win_width(width)
            .win_height(height)
            .id(1234)
            .max_fps(Duration::from_secs((1 / 30u16).into()))
            .keepalive_timeout(Duration::from_secs(1))
            .root_program(Some("glxgears".into()))
            .build()
            .expect("Failed to build Configuration!")
    }

    fn generate_default_configuration() -> Configuration {
        generate_configuration(4, 800, 600)
    }

    fn is_udp_port_open(port: u16) -> bool {
        match UdpSocket::bind(("127.0.0.1", port)) {
            Ok(_) => true,
            Err(_) => false,
        }
    }

    async fn new_async_tcp_server(addr: String) -> tcp::Server<tcp::Accepted> {
        let addr: SocketAddr = addr.parse().expect("Failed to parse SocketAddr");
        tcp::Server::new_blocking(addr)
            .try_transition()
            .await
            .expect("Failed to Listen")
            .try_transition()
            .await
            .expect("Failed to Accept")
    }

    #[test]
    fn client_initialization() {
        setup();
        let config = generate_default_configuration();
        let client = Client::new(&config);

        assert_eq!(client.config, config);
    }

    #[test]
    fn setup_decoders() {
        setup();
        let begin_port = 8653;
        let mut config = generate_default_configuration();
        config.bind_address.set_port(begin_port);
        let itc = ItcChannels::new(&config);
        let mut client = Client::new(&config);

        for i in 0..config.decoder_max {
            client.decoders.insert(1234 + i, ClientDecoder::new(i));
        }

        client
            .setup_decoders(&itc)
            .expect("Failed to setup decoders");

        for (i, _remote_port) in client.decoders.keys().into_iter().enumerate() {
            let local_port = begin_port + i as u16;
            assert_eq!(
                is_udp_port_open(local_port),
                false,
                "Port {} is open!",
                local_port
            );
        }
    }

    #[test]
    fn setup_control_channel() {
        setup();
        let config = generate_default_configuration();
        let itc = ItcChannels::new(&config);
        let mut client = Client::new(&config);

        // Use a short retry interval for the test
        client.tcp_retry_interval = std::time::Duration::from_millis(100);

        //
        // spawn fake server
        //
        std::thread::spawn(move || {
            let async_rt = Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("Failed to build tokio Runtime");

            let tcp_server = async_rt
                .block_on(
                    async_rt.spawn(new_async_tcp_server(client.config.server_addr.to_string())),
                )
                .expect("Failed to create TCP server");
            let mut server_cc = ServerControlChannel::new(tcp_server);
            let _ci = loop {
                match server_cc.receive() {
                    Ok(cm_c2s::Message::ClientInit(ci)) => break ci,
                    Ok(_) => panic!("Unexpected client message!"),
                    Err(err) => panic!("{}", err),
                }
            };

            server_cc
                .send(
                    ServerInitBuilder::default()
                        .client_id(7890_u64)
                        .server_name(String::from("test_server"))
                        .event_port(1234_u32)
                        .display_size(Dimension::new(800, 600))
                        .decoder_areas(vec![])
                        .cursor_bitmaps(vec![])
                        .build()
                        .unwrap()
                        .into(),
                )
                .unwrap();
        });

        client
            .setup_control_channel(&itc)
            .expect("Failed to startup Control Channel");
        assert_eq!(client.server_name, Some(String::from("test_server")));
        assert_eq!(client.event_port, Some(1234));
    }

    #[test]
    fn setup_event_channel() {
        setup();
        let config = generate_default_configuration();
        let itc = ItcChannels::new(&config);
        let mut client = Client::new(&config);

        // Use a short retry interval for the test
        client.tcp_retry_interval = std::time::Duration::from_millis(100);

        //
        // Spawn fake server
        //
        std::thread::spawn(move || {
            let async_rt = Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("Failed to build tokio Runtime");

            let mut event_addr = client.config.server_addr.clone();
            event_addr.set_port(8764);

            let tcp_server = async_rt
                .block_on(async_rt.spawn(new_async_tcp_server(event_addr.to_string())))
                .expect("Failed to create TCP server");
            let mut server_cc = ServerEventChannel::new(tcp_server);
            let _event = loop {
                match server_cc.receive() {
                    Ok(_) => break,
                    Err(err) => panic!("{}", err),
                }
            };
        });

        client.event_port = Some(8764);
        client
            .setup_event_channel(&itc)
            .expect("Failed to setup Event Channel");
    }
}
