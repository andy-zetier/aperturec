use crate::gtk3::{GtkUi, ItcChannels};

use aperturec_channel::reliable::tcp;
use aperturec_channel::unreliable::udp;
use aperturec_channel::{
    ClientControlChannel, ClientEventChannel, ClientMediaChannel, Receiver, Sender,
};
use aperturec_protocol::common_types::*;
use aperturec_protocol::control_messages::{
    Architecture, Bitness, ClientGoodbye, ClientGoodbyeBuilder, ClientGoodbyeReason, ClientInfo,
    ClientInfoBuilder, ClientInit, ClientInitBuilder, ClientToServerMessage as CM_C2S, Decoder,
    DecoderSequencePair, Endianness, HeartbeatResponseBuilder, MissedFrameReportBuilder, Os,
    ServerToClientMessage as CM_S2C,
};
use aperturec_protocol::media_messages::ServerToClientMessage as MM_S2C;

use aperturec_protocol::event_messages::{
    Button, ButtonStateBuilder, ClientToServerMessage as EM_C2S, KeyEvent, KeyEventBuilder,
    MappedButton, PointerEvent, PointerEventBuilder,
};
use aperturec_state_machine::TryTransitionable;

use anyhow::Result;
use derive_builder::Builder;
use std::collections::{BTreeMap, BTreeSet};
use std::env::consts;
use std::io::ErrorKind;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
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
        LocationBuilder::default()
            .x_position(self.0.into())
            .y_position(self.1.into())
            .build()
            .expect("Failed to convert Point -> Location")
    }

    pub fn from_location(loc: &Location) -> Self {
        Self(
            loc.x_position
                .try_into()
                .expect("Failed to convert x coordinate"),
            loc.y_position
                .try_into()
                .expect("Failed to convert y coordinate"),
        )
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
        match button {
            1 => Button::MappedButton(MappedButton::Left),
            2 => Button::MappedButton(MappedButton::Right),
            3 => Button::MappedButton(MappedButton::Middle),
            4 => Button::MappedButton(MappedButton::WheelScrollUp),
            5 => Button::MappedButton(MappedButton::WheelScrollDown),
            6 => Button::MappedButton(MappedButton::WheelScrollLeft),
            7 => Button::MappedButton(MappedButton::WheelScrollRight),
            8 => Button::MappedButton(MappedButton::Back),
            9 => Button::MappedButton(MappedButton::Forward),
            _ => Button::UnmappedButton(
                button
                    .try_into()
                    .unwrap_or_else(|_| panic!("Failed to create UnmappedButton ({})", button)),
            ),
        }
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
        KeyEventBuilder::default()
            .key(self.key.into())
            .down(self.is_pressed)
            .build()
            .expect("Failed to build KeyEvent")
    }
}

#[derive(Builder, Clone, Default)]
pub struct GoodbyeMessage {
    reason: String,
}

impl GoodbyeMessage {
    const NETWORK_ERROR: &str = "Network Error";
    const TERMINATING: &str = "Terminating";

    pub fn to_client_goodbye(&self, client_id: ClientId) -> ClientGoodbye {
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
}

//
// Async utility functions
//
// The Client is currently synchronous mostly due to GTK's limitation on threads and is unable to
// directly call async functions such as the Server and Client constructors from the
// aperturec_channels crate. To get around this, we can wrap these async calls in a managed tokio
// runtime (see `async_rt`) and return the results to synchrnous code.
//
async fn new_async_udp_server(addr: IpAddr, port: u16) -> udp::Server<udp::Listening> {
    udp::Server::new_blocking(SocketAddr::new(addr, port))
        .try_transition()
        .await
        .expect("Failed to Listen")
}

async fn new_async_tcp_client_retry(
    addr: SocketAddr,
    retry: Duration,
) -> tcp::Client<tcp::Connected> {
    loop {
        let client = tcp::Client::new(addr).try_transition().await;
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
    pub win_width: u64,
    pub win_height: u64,
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
    id: ClientId,
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
        let id = ClientId::new(config.id);
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

        this.setup_decoders(itc)?;
        this.setup_control_channel(itc)?;
        this.setup_event_channel(itc)?;

        Ok(this)
    }

    pub fn shutdown(mut self) {
        if self.control_jh.is_some() {
            let _ = self.control_jh.take().unwrap().join();
        }
    }

    fn generate_client_caps(&self) -> ClientCaps {
        ClientCapsBuilder::default()
            .supported_codecs(vec![Codec::Raw, Codec::Zlib])
            .build()
            .expect("Failed to generate ClientCaps!")
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
            .endianness(match cpu_endian::working() {
                cpu_endian::Endian::Little => Endianness::Little,
                cpu_endian::Endian::Big => Endianness::Big,
                _ => panic!("Unsupported endianness"),
            })
            .architecture(match consts::ARCH {
                "x86" => Architecture::X86,
                "x86_64" => Architecture::X86,
                "arm" => Architecture::Arm,
                _ => panic!("Unsupported architcture"),
            })
            .cpu_id(sys.cpus()[0].brand().to_string())
            .number_of_cores(sys.cpus().len().try_into().unwrap())
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
            .temp_id(ClientId(self.id.0))
            .client_info(self.generate_client_info())
            .client_caps(self.generate_client_caps())
            .client_heartbeat_interval(DurationMs(
                self.heartbeat_interval
                    .as_millis()
                    .try_into()
                    .expect("Failed to convert Duration"),
            ))
            .client_heartbeat_response_interval(DurationMs(
                self.heartbeat_response_interval
                    .as_millis()
                    .try_into()
                    .expect("Failed to convert Duration"),
            ))
            .decoders(self.decoders.keys().map(|p| Decoder::new(*p)).collect())
            .build()
            .expect("Failed to generate ClientInit!")
    }

    fn setup_decoders(&mut self, itc: &ItcChannels) -> Result<()> {
        let port_start = self.config.bind_address.port();

        assert!(
            itc.img_to_decoder_rxs.len() == self.config.decoder_max.into(),
            "img_to_decover_rxs.len() != decoder_max ({} != {})",
            itc.img_to_decoder_rxs.len(),
            self.config.decoder_max
        );

        for decoder_id in 0..self.config.decoder_max {
            let port = if port_start == 0 {
                0
            } else {
                port_start + decoder_id
            };
            let udp_server = self
                .async_rt
                .block_on(
                    self.async_rt
                        .spawn(new_async_udp_server(self.config.bind_address.ip(), port)),
                )
                .expect("Failed to create UDP server");

            let port = udp_server.local_addr().port();

            log::debug!("[decoder {}] {:?}", port, &udp_server);

            self.decoders.insert(port, ClientDecoder::new(decoder_id));

            let mut client_mc = ClientMediaChannel::new(udp_server);

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
                            port,
                            img.decoder_id,
                            decoder_id
                        );
                        img
                    }
                    Err(err) => {
                        log::error!(
                            "[decoder {}] Fatal error receiving Image from UI: {}",
                            port,
                            err
                        );
                        log::debug!("[decoder {}] exiting", port);
                        return;
                    }
                };

                log::debug!("[decoder {}] Media channel started", port);
                loop {
                    //
                    // Listen for FramebufferUpdates from the server
                    //
                    let fbu = match client_mc.receive() {
                        Ok(m) => match m {
                            MM_S2C::FramebufferUpdate(fbu) => fbu,
                            _ => {
                                log::warn!("[decoder {}] Received unexpected message", port);
                                continue;
                            }
                        },
                        Err(err) => match err.downcast::<std::io::Error>() {
                            Ok(ioe) => match ioe.kind() {
                                ErrorKind::WouldBlock => {
                                    // receive() should be blocking
                                    log::warn!("[decoder {}] EAGAIN", port);
                                    continue;
                                }
                                _ => {
                                    log::warn!("[decoder {}] IO Error: {:?}", port, ioe);
                                    continue;
                                }
                            },
                            Err(other) => {
                                log::warn!(
                                    "[decoder {}] Failed to process media channel message: {}",
                                    port,
                                    other
                                );
                                continue;
                            }
                        },
                    };

                    if should_stop.load(Ordering::Relaxed) {
                        break;
                    }

                    if fbu.rectangle_updates.is_empty() {
                        log::warn!(
                            "[decoder {}] Ignoring FramebufferUpdate with {} rectangle updates",
                            port,
                            fbu.rectangle_updates.len()
                        );
                        continue;
                    }

                    // FramebufferUpdate logging is *very* noisy
                    /*log::trace!(
                        "[decoder {}] FramebufferUpdate [{}..{}]",
                        port,
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
                        if current_seq_id.map_or(false, |csi: u64| csi >= ru.sequence_id.0) {
                            log::warn!(
                                "[decoder {}] Dropping Rectangle {}, already received {}",
                                port,
                                ru.sequence_id.0,
                                current_seq_id.unwrap()
                            );

                            continue;
                        } else if current_seq_id.is_none() && ru.sequence_id.0 != 0 {
                            missing_seq.extend(0..ru.sequence_id.0)
                        } else if let Some(current_seq_id) = current_seq_id {
                            if current_seq_id < ru.sequence_id.0 {
                                missing_seq.extend(current_seq_id + 1..ru.sequence_id.0);
                            }
                        }

                        current_seq_id = Some(ru.sequence_id.0);

                        //
                        // Write rectangle data to the Image with the appropriate Codec
                        //
                        match ru.rectangle.codec {
                            Codec::Raw => match image.draw_raw(
                                &ru.rectangle.data,
                                ru.location.x_position,
                                ru.location.y_position,
                                ru.rectangle
                                    .dimension
                                    .as_ref()
                                    .expect("Width is required")
                                    .width,
                                ru.rectangle
                                    .dimension
                                    .as_ref()
                                    .expect("Height is required")
                                    .height,
                            ) {
                                Ok(_) => (),
                                Err(err) => {
                                    log::warn!("[decoder {}] Codec::Raw failed: {}", port, err);
                                    log::debug!("[decoder {}] {:#?}", port, &ru);
                                }
                            },
                            Codec::Zlib => match image.draw_raw_zlib(
                                &ru.rectangle.data,
                                ru.location.x_position,
                                ru.location.y_position,
                                ru.rectangle
                                    .dimension
                                    .as_ref()
                                    .expect("Width is required")
                                    .width,
                                ru.rectangle
                                    .dimension
                                    .as_ref()
                                    .expect("Height is required")
                                    .height,
                            ) {
                                Ok(_) => (),
                                Err(err) => {
                                    log::warn!("[decoder {}] Codec::Zlib failed: {}", port, err);
                                    log::debug!("[decoder {}] {:#?}", port, &ru);
                                }
                            },
                            _ => {
                                log::warn!(
                                    "[decoder {}] Dropping Rectangle {}, unsupported codec",
                                    port,
                                    current_seq_id.unwrap()
                                );
                            }
                        }
                    } // for rectangle_updates

                    //
                    // Swap Image with the UI thread
                    //
                    image = match img_tx.send(image) {
                        Ok(_) => match img_rx.recv() {
                            Ok(img) => img,
                            Err(err) => {
                                log::error!(
                                    "[decoder {}] Fatal error receiving Image from UI: {}",
                                    port,
                                    err
                                );
                                break;
                            }
                        },
                        Err(err) => {
                            log::error!(
                                "[decoder {}] Fatal error sending Image to UI: {}",
                                port,
                                err
                            );
                            break;
                        }
                    };

                    if !missing_seq.is_empty() {
                        log::debug!("[decoder {}] MFRs generated for {:?}", port, missing_seq);
                        aperturec_metrics::builtins::packet_lost(missing_seq.len());
                    }
                    //
                    // Notify Control thread of last SequenceId and missing SequenceIds
                    //
                    match control_tx.send(ControlMessage::UpdateReceivedMessage(
                        port,
                        current_seq_id.unwrap(),
                        missing_seq.into_iter().collect(),
                    )) {
                        Ok(_) => (),
                        Err(err) => {
                            if !should_stop.load(Ordering::Relaxed) {
                                log::error!("[decoder {}] Fatal error sending UpdateReceivedMessage to control thread: {}", port, err);
                            }
                            break;
                        }
                    };
                } // loop receive FramebufferUpdate

                log::debug!("[decoder {}] Media channel exiting", port);
            }); // thread::spawn
        } // for decoder_id

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

        let mut client_cc = ClientControlChannel::new(tcp_client);
        client_cc.send(CM_C2S::ClientInit(ci))?;

        log::debug!("Client Init sent, waiting for ServerInit...");
        let si = loop {
            match client_cc.receive() {
                Ok(CM_S2C::ServerInit(si)) => break si,
                Ok(_) => panic!("Unexpected message received, expected ServerInit"),
                Err(err) => match err.downcast::<std::io::Error>() {
                    Ok(ioe) => match ioe.kind() {
                        ErrorKind::WouldBlock => {
                            // EAGAIN is expected while wating for the ServerInit
                            continue;
                        }
                        _ => panic!("Failed to read ServerInit: {:?}", ioe),
                    },
                    Err(other) => panic!("Failed to read ServerInit: {:?}", other),
                },
            }
        };

        log::debug!("{:#?}", si);

        //
        // Update client config with info from Server
        //
        self.id = si.client_id;
        self.server_name = Some(si.server_name);
        self.event_port = Some(si.event_port);
        self.config.win_height = si.display_size.height;
        self.config.win_width = si.display_size.width;

        for decoder in si.decoder_areas {
            let d = self
                .decoders
                .get_mut(&decoder.decoder.port)
                .expect("Unknown decoder");
            d.set_origin(decoder.location);
            d.set_dims(decoder.dimension);
        }

        log::info!(
            "Connected to server @ {} ({}) as client {}!",
            &self.config.server_addr,
            &self.server_name.as_ref().unwrap(),
            &self.id.0
        );

        //
        // The control channel thread needs to monitor both the TCP connection with the server and
        // the ITC channel with the UI and other internal components. Ideally this would be
        // performed with a select() on the TCP socket and the ITC channel fds, however, it does
        // not seem to be possible to select() across a std::net::TcpStream and a std::sync::mpsc.
        // Tokio has implemented such a select!() construct for async code, but there does not
        // seem to be an equivalent for synchronous code.
        //
        // The workaround here is to perform a non-blocking read on the TCP connection followed by
        // a blocking read on the ITC channel. To do this, we need to come up with a reasonable
        // timeout value for the ITC read. We must break out of the ITC receive in time to make
        // sure we are responding to HeartbeatRequets in a timely manner. Therefore, wait for ITC
        // messages at most 75% of the shortest heartbeat interval.
        //
        let seq_update_timeout =
            (std::cmp::min(self.heartbeat_response_interval, self.heartbeat_interval) * 75) / 100;
        log::debug!("ITC control message timeout: {:?}", seq_update_timeout);

        let client_id = self.id.clone();
        let should_stop = self.should_stop.clone();
        let control_rx = itc.notify_control_rx.take().unwrap();

        self.control_jh = Some(thread::spawn(move || {
            // Track the last received sequence id for each decoder, this is needed for HBRs
            let mut decoder_seq = BTreeMap::new();

            log::debug!("Control channel started");
            loop {
                //
                // Check for Control channel messages from the server, this is non-blocking
                //
                match client_cc.receive() {
                    Ok(CM_S2C::HeartbeatRequest(hr)) => {
                        let hbr = HeartbeatResponseBuilder::default()
                            .heartbeat_id(HeartbeatId::new(hr.request_id.0))
                            .last_sequence_ids(
                                decoder_seq
                                    .iter()
                                    .map(|(d, s)| {
                                        DecoderSequencePair::new(
                                            Decoder::new(*d),
                                            SequenceId::new(*s),
                                        )
                                    })
                                    .collect(),
                            )
                            .build()
                            .expect("Failed to generate HeartbeatResponse!");
                        match client_cc.send(CM_C2S::new_heartbeat_response(hbr)) {
                            Err(err) => log::warn!(
                                "Failed to send HeartbeatResponse {:?}: {}",
                                hr.request_id.0,
                                err
                            ),
                            _ => log::trace!("Sent HeartbeatResponse {}", hr.request_id.0),
                        };
                    }
                    Ok(CM_S2C::ServerGoodbye(_)) => {
                        if let Err(err) =
                            control_to_ui_tx.send(UiMessage::QuitMessage(String::from("Gooebyde!")))
                        {
                            log::warn!("Failed to send QuitMessage: {}", err);
                        }
                        break;
                    }
                    Ok(_) => {
                        log::warn!("Unexpected message received on control channel");
                    }
                    Err(err) => match err.downcast::<std::io::Error>() {
                        Ok(ioe) => match ioe.kind() {
                            ErrorKind::WouldBlock | ErrorKind::Interrupted => (),
                            _ => {
                                let _ = control_to_ui_tx
                                    .send(UiMessage::QuitMessage(format!("{:?}", ioe)));
                                let _ = client_cc.send(CM_C2S::new_client_goodbye(
                                    ClientGoodbye::new(client_id, ClientGoodbyeReason::Terminating),
                                ));
                                log::trace!("Sent ClientGoodbye: Terminating");
                                log::error!("Fatal I/O error reading control message: {:?}", ioe);
                                break;
                            }
                        },
                        Err(other) => {
                            let _ = control_to_ui_tx
                                .send(UiMessage::QuitMessage(format!("{:?}", other)));
                            let _ = client_cc.send(CM_C2S::new_client_goodbye(ClientGoodbye::new(
                                client_id,
                                ClientGoodbyeReason::Terminating,
                            )));
                            log::trace!("Sent ClientGoodbye: Terminating");
                            log::error!("Fatal error reading control message: {:?}", other);
                            break;
                        }
                    },
                } // client_cc.receive()

                //
                // Check the ITC control channel for messages from internal components, this is
                // blocking for seq_update_timeout
                //
                let (dec, seq, missing) = match control_rx.recv_timeout(seq_update_timeout) {
                    Ok(ControlMessage::UpdateReceivedMessage(d, s, m)) => (d, s, m),
                    Ok(ControlMessage::UiClosed(gm)) => {
                        let _ = client_cc
                            .send(CM_C2S::new_client_goodbye(gm.to_client_goodbye(client_id)));
                        log::trace!("Sent ClientGoodbye: User Requested");
                        log::info!("Disconnecting from server, sent ClientGoodbye");
                        break;
                    }
                    Ok(ControlMessage::EventChannelDied(gm)) => {
                        let _ = control_to_ui_tx
                            .send(UiMessage::QuitMessage("Event Channel Died".to_string()));
                        let _ = client_cc
                            .send(CM_C2S::new_client_goodbye(gm.to_client_goodbye(client_id)));
                        log::trace!("Sent ClientGoodbye: Network Error");
                        break;
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        log::error!("Decoder ITC channel disconnected!");
                        break;
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
                };

                decoder_seq.insert(dec, seq);

                if !missing.is_empty() {
                    let mfr = MissedFrameReportBuilder::default()
                        .decoder(Decoder::new(dec))
                        .frames(missing.iter().map(|s| SequenceId::new(*s)).collect())
                        .build()
                        .expect("Failed to build MissedFrameReport!");

                    match client_cc.send(CM_C2S::new_missed_frame_report(mfr)) {
                        Err(err) => log::warn!("Failed to send MissedFrameReport: {}", err),
                        _ => {
                            log::trace!(
                                "Sent MissedFrameReport for Decoder {}, missed {}",
                                dec,
                                missing.len()
                            );
                        }
                    };
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
                        EM_C2S::PointerEvent(mbem.to_pointer_event())
                    }
                    EventMessage::PointerEventMessage(pem) => {
                        EM_C2S::PointerEvent(pem.to_pointer_event())
                    }
                    EventMessage::KeyEventMessage(kem) => EM_C2S::KeyEvent(kem.to_key_event()),
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

    use crate::client::{Client, Configuration, ConfigurationBuilder};
    use crate::gtk3::ItcChannels;
    use aperturec_channel::reliable::tcp;
    use aperturec_channel::{Receiver, Sender, ServerControlChannel, ServerEventChannel};
    use aperturec_protocol::common_types::*;
    use aperturec_protocol::control_messages::*;
    use aperturec_protocol::control_messages::{
        ClientToServerMessage as CM_C2S, ServerToClientMessage as CM_S2C,
    };
    use aperturec_state_machine::TryTransitionable;
    use simple_logger::SimpleLogger;
    use std::net::SocketAddr;
    use std::net::UdpSocket;
    use std::sync::Once;
    use std::time::Duration;
    use tokio::runtime::Builder;

    static INIT: Once = Once::new();

    fn setup() {
        INIT.call_once(|| {
            SimpleLogger::new()
                .env()
                .init()
                .expect("Failed to initialize logging");
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
        let config = generate_default_configuration();
        let itc = ItcChannels::new(&config);
        let mut client = Client::new(&config);

        client
            .setup_decoders(&itc)
            .expect("Failed to setup decoders");

        assert_eq!(client.decoders.len(), config.decoder_max.into());

        for port in client.decoders.keys() {
            assert_eq!(is_udp_port_open(*port), false, "Port {} is open!", port);
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
                    Ok(CM_C2S::ClientInit(ci)) => break ci,
                    Ok(_) => panic!("Unexpected client message!"),
                    Err(err) => panic!("{}", err),
                }
            };

            server_cc
                .send(CM_S2C::ServerInit(
                    ServerInitBuilder::default()
                        .client_id(ClientId(7890))
                        .server_name(String::from("test_server"))
                        .cursor_bitmaps(None)
                        .event_port(1234)
                        .display_size(Dimension::new(800, 600))
                        .decoder_areas(vec![])
                        .build()
                        .unwrap(),
                ))
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
