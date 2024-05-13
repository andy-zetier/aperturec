use crate::gtk3::{GtkUi, ItcChannels};
use anyhow::{bail, Result};
use aperturec_channel::{
    self as channel, client::states as channel_states, Receiver, Sender, UnifiedClient,
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
use aperturec_protocol::media::{self, server_to_client as mm_s2c};
use aperturec_state_machine::*;
use aperturec_trace::log;
use crossbeam_channel::{select, unbounded};
use derive_builder::Builder;
use openssl::x509::X509;
use std::collections::{BTreeMap, BTreeSet};
use std::env::consts;
use std::io::ErrorKind;
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
    UpdateReceivedMessage {
        decoder_id: u32,
        sequence: u64,
        missing_sequences: BTreeSet<u64>,
    },
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
    pub keepalive_timeout: Duration,
    pub win_width: u64,
    pub win_height: u64,
    #[builder(setter(strip_option), default)]
    pub program_cmdline: Option<String>,
    #[builder(setter(name = "additional_tls_certificate", custom), default)]
    pub additional_tls_certificates: Vec<X509>,
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

#[derive(Clone, Debug, Default)]
pub struct ClientDecoder {
    pub id: u32,
    pub origin: Point,
    pub width: u32,
    pub height: u32,
}

impl ClientDecoder {
    fn new(id: u32) -> Self {
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
    id: Option<u64>,
    server_name: Option<String>,
    heartbeat_interval: Duration,
    heartbeat_response_interval: Duration,
    control_jh: Option<thread::JoinHandle<()>>,
    should_stop: Arc<AtomicBool>,
    decoders: Vec<ClientDecoder>,
}

impl Client {
    fn new(config: &Configuration) -> Self {
        const HEARTBEAT_RESPONSE_INTERVAL: Duration = Duration::from_secs(10);
        Self {
            config: config.clone(),
            should_stop: Arc::new(AtomicBool::new(false)),

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
            heartbeat_response_interval: HEARTBEAT_RESPONSE_INTERVAL,

            id: None,
            server_name: None,
            control_jh: None,
            decoders: vec![],
        }
    }

    pub fn startup(config: &Configuration, itc: &ItcChannels) -> Result<Self> {
        let mut this = Client::new(config);

        let (cc, ec, mc) = this.setup_unified_channel()?;
        this.setup_control_channel(cc, itc)?;
        this.setup_event_channel(ec, itc)?;
        this.setup_media_channel(mc, itc)?;

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
            .temp_id(self.config.temp_id)
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
        itc: &ItcChannels,
    ) -> Result<()> {
        assert!(
            itc.img_to_decoder_rxs.len() == self.config.decoder_max.into(),
            "img_to_decoder_rxs.len() != decoder_max ({} != {})",
            itc.img_to_decoder_rxs.len(),
            self.config.decoder_max
        );

        let mut decoder_channels_tx = BTreeMap::new();
        let mut decoder_channels_rx = BTreeMap::new();
        for decoder in &self.decoders {
            let (tx, rx) = unbounded();
            decoder_channels_tx.insert(decoder.id, tx);
            decoder_channels_rx.insert(decoder.id, rx);
        }
        thread::spawn::<_, Result<()>>(move || loop {
            let mm_s2c::Message::FramebufferUpdate(fbu) = mc.receive()?;
            aperturec_metrics::builtins::packet_sent(1);
            log::debug!(
                "[decoder {}] received sequence {}",
                fbu.decoder_id,
                fbu.sequence
            );

            match decoder_channels_tx.get_mut(&fbu.decoder_id) {
                Some(channel) => channel.send(fbu)?,
                None => log::warn!(
                    "Received media message for decoder ID {} which does not exist",
                    fbu.decoder_id
                ),
            }
        });

        for decoder in &self.decoders {
            let img_tx = itc.img_from_decoder_tx.clone();
            let img_rx = itc.img_to_decoder_rxs[decoder.id as usize].take().unwrap();
            let control_tx = itc.notify_control_tx.clone();
            let should_stop = self.should_stop.clone();
            let decoder_id = decoder.id;

            let fbu_rx = decoder_channels_rx
                .remove(&decoder.id)
                .expect("decoder channel");

            thread::spawn(move || {
                let mut current_seq_id = None;

                //
                // Receive an initial Image from the UI thread
                //
                let mut image = match img_rx.recv() {
                    Ok(img) => {
                        assert!(
                            img.decoder_id == decoder_id as usize,
                            "[decoder {}] Image decoder_id != decoder_id ({} != {})",
                            decoder_id,
                            img.decoder_id,
                            decoder_id
                        );
                        img
                    }
                    Err(err) => {
                        log::error!(
                            "[decoder {}] Fatal error receiving Image from UI: {}",
                            decoder_id,
                            err
                        );
                        log::debug!("[decoder {}] exiting", decoder_id);
                        bail!(err);
                    }
                };

                let loop_res = loop {
                    //
                    // Listen for FramebufferUpdates from the server
                    //
                    let fbu = fbu_rx.recv()?;

                    if should_stop.load(Ordering::Relaxed) {
                        break Ok::<_, anyhow::Error>(());
                    }

                    // FramebufferUpdate logging is *very* noisy
                    // log::trace!(
                    //     "[decoder {}] FramebufferUpdate [{}]",
                    //     decoder_id,
                    //     fbu.sequence
                    // );

                    aperturec_metrics::builtins::packet_received(1);

                    let mut missing_sequences: BTreeSet<u64> = BTreeSet::new();

                    //
                    // Handle out-of-sequence RectangleUpdates
                    //
                    // TODO: I'm not sure we should simply drop out of order sequence numbers.
                    // Its possible out-of-sequence updates represent completely different
                    // decoder areas, in which case they should not be dropped. However, if
                    // they contain stale data, we don't want to include it and should drop
                    // them. Perhaps the safe thing to do here is drop them and add them to
                    // missing_sequences so the latest data is re-sent? In any case, this doesn't
                    // seem to happen very often in my ad-hoc testing, so I'll leave them
                    // dropped for now with a warning.
                    //
                    if current_seq_id.map_or(false, |csi: u64| csi >= fbu.sequence) {
                        log::warn!(
                            "[decoder {}] Dropping Rectangle {}, already received {}",
                            decoder_id,
                            fbu.sequence,
                            current_seq_id.unwrap()
                        );

                        continue;
                    } else if current_seq_id.is_none() && fbu.sequence != 0 {
                        missing_sequences.extend(0..fbu.sequence)
                    } else if let Some(current_seq_id) = current_seq_id {
                        if current_seq_id < fbu.sequence {
                            missing_sequences.extend(current_seq_id + 1..fbu.sequence);
                        }
                    }

                    current_seq_id = Some(fbu.sequence);

                    if !missing_sequences.is_empty() {
                        let missing_count = missing_sequences.len();
                        aperturec_metrics::builtins::packet_lost(missing_count);

                        // If these packets were lost, they must have been sent
                        aperturec_metrics::builtins::packet_sent(missing_count);
                    }
                    //
                    // Notify Control thread of last SequenceId and missing SequenceIds
                    //
                    match control_tx.send(ControlMessage::UpdateReceivedMessage {
                        decoder_id,
                        sequence: fbu.sequence,
                        missing_sequences,
                    }) {
                        Ok(_) => (),
                        Err(err) => {
                            if !should_stop.load(Ordering::Relaxed) {
                                log::error!("[decoder {}] Fatal error sending UpdateReceivedMessage to control thread: {}", decoder_id, err);
                            }
                            break Err(err.into());
                        }
                    };

                    //
                    // Write rectangle data to the Image with the appropriate Codec
                    //
                    if let media::FramebufferUpdate {
                        codec,
                        location: Some(location),
                        dimension: Some(dimension),
                        data,
                        ..
                    } = &fbu
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
                                        decoder_id,
                                        err
                                    );
                                    log::debug!("[decoder {}] {:#?}", decoder_id, &fbu);
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
                                        decoder_id,
                                        err
                                    );
                                    log::debug!("[decoder {}] {:#?}", decoder_id, &fbu);
                                }
                            },
                            _ => {
                                log::warn!(
                                    "[decoder {}] Dropping Rectangle {}, unsupported codec",
                                    decoder_id,
                                    current_seq_id.unwrap()
                                );
                                continue;
                            }
                        }
                    } else {
                        log::warn!(
                            "[decoder {}] Dropping rectangle {}, malformed",
                            decoder_id,
                            current_seq_id.unwrap()
                        );
                        continue;
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
                                    decoder_id,
                                    err
                                );
                                break Err(err.into());
                            }
                        },
                        Err(err) => {
                            log::error!(
                                "[decoder {}] Fatal error sending Image to UI: {}",
                                decoder_id,
                                err
                            );
                            break Err(err.into());
                        }
                    };
                }; // loop

                log::debug!("[decoder {}] Media channel exiting", decoder_id);
                loop_res
            }); // thread::spawn
        } // for self.decoder

        Ok(())
    }

    fn setup_control_channel(
        &mut self,
        client_cc: channel::ClientControl,
        itc: &ItcChannels,
    ) -> Result<()> {
        let control_to_ui_tx = itc.control_to_ui_tx.clone();

        let ci = self.generate_client_init();
        log::debug!("{:#?}", &ci);

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
        self.id = Some(si.client_id);
        self.server_name = Some(si.server_name);
        let display_size = si.display_size.expect("No display size provided");
        self.config.win_height = display_size.height;
        self.config.win_width = display_size.width;

        for decoder_area in si.decoder_areas.into_iter() {
            if let DecoderArea {
                decoder_id,
                location: Some(location),
                dimension: Some(dimension),
            } = decoder_area
            {
                let mut d = ClientDecoder::new(decoder_id);
                d.set_origin(location);
                d.set_dims(dimension);
                self.decoders.push(d);
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
            &self.id.unwrap()
        );

        //
        // Setup Control Channel TX/RX threads
        //
        // The QUIC tx/rx threads read/write network control channel messages from/to appropriate
        // ITC channels. This allows the core Control Channel thread to select!() on unbounded ITC
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
                if let aperturec_protocol::control::client_to_server::Message::MissedFrameReport(
                    aperturec_protocol::control::MissedFrameReport {
                        decoder_id,
                        frame_sequence_ids,
                    },
                ) = &cm_c2s
                {
                    log::debug!(
                        "[decoder {}] Sending MFR {:?}",
                        decoder_id,
                        frame_sequence_ids
                    );
                }
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
        let client_id = self.id.unwrap();
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
                                    .map(|(d, s)| DecoderSequencePair::new(*d, *s))
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
                        Ok(ControlMessage::UpdateReceivedMessage { decoder_id, sequence, missing_sequences }) => {
                            decoder_seq.insert(decoder_id, sequence);

                            if !missing_sequences.is_empty() {
                                let mfr = MissedFrameReportBuilder::default()
                                    .decoder_id(decoder_id)
                                    .frame_sequence_ids(missing_sequences.into_iter().collect::<Vec<_>>())
                                    .build()
                                    .expect("Failed to build MissedFrameReport!");

                                if let Err(err) = control_tx_tx.send(mfr.into()) {
                                    log::warn!("Failed to send MissedFrameReport: {}", err);
                                }
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

    fn setup_event_channel(
        &mut self,
        mut client_ec: channel::ClientEvent,
        itc: &ItcChannels,
    ) -> Result<()> {
        let event_rx = itc.event_from_ui_rx.take().unwrap();

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

    fn setup_unified_channel(
        &mut self,
    ) -> Result<(
        channel::ClientControl,
        channel::ClientEvent,
        channel::ClientMedia,
    )> {
        let (server_addr, server_port) = match self.config.server_addr.rsplit_once(":") {
            Some((addr, port)) => (addr, Some(port.parse()?)),
            None => (&*self.config.server_addr, None),
        };

        let mut channel_builder = channel::client::Builder::default().server_addr(server_addr);
        if let Some(port) = server_port {
            channel_builder = channel_builder.server_port(port);
        }
        for cert in &self.config.additional_tls_certificates {
            log::debug!("Adding cert: {:?}", cert);
            channel_builder = channel_builder.additional_tls_certificate(&cert.to_der()?);
        }
        let channel = channel_builder.build_sync()?;
        let channel = try_transition!(channel, channel_states::Connected).map_err(|r| r.error)?;
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
        &client.decoders,
    );

    client.shutdown();

    Ok(())
}

#[cfg(test)]
mod test {
    use super::*;

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
            .keepalive_timeout(Duration::from_secs(1))
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
        let der_material: channel::tls::DerMaterial =
            material.clone().try_into().expect("convert to DER");
        let qserver = channel::server::Builder::default()
            .bind_addr("0.0.0.0:0")
            .tls_certificate(&der_material.certificate)
            .tls_private_key(&der_material.pkey)
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
        let sthread = thread::spawn(move || {
            let qserver = try_transition!(qserver).expect("server listen");
            try_transition!(qserver).expect("server ready");
        });

        let itc = ItcChannels::new(&config);
        let client = Client::startup(&config, &itc);
    }
}
