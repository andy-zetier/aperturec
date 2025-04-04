use crate::frame::*;
use crate::gtk3::{self, image::Image, ClientSideItcChannels, GtkUi, ItcChannels, LockState};

use aperturec_channel::{self as channel, Receiver as _, Sender as _, Unified};
use aperturec_graphics::{
    display::{self, Display},
    euclid_collections::EuclidSet,
    geometry::*,
};
use aperturec_protocol::common::*;
use aperturec_protocol::control::{
    self as cm, client_to_server as cm_c2s, server_to_client as cm_s2c, Architecture, Bitness,
    ClientGoodbye, ClientGoodbyeBuilder, ClientGoodbyeReason, ClientInfo, ClientInfoBuilder,
    ClientInit, ClientInitBuilder, Endianness, Os,
};
use aperturec_protocol::event::{
    self, button, client_to_server as em_c2s, server_to_client as em_s2c, Button,
    ButtonStateBuilder, DisplayEvent, KeyEvent, MappedButton, PointerEvent, PointerEventBuilder,
};
use aperturec_protocol::tunnel;
use aperturec_utils::log::*;

use anyhow::{anyhow, bail, ensure, Error, Result};
use crossbeam::channel::{bounded, never, select, unbounded, Receiver, RecvTimeoutError, Sender};
use derive_builder::Builder;
use gtk::glib;
use openssl::x509::X509;
use secrecy::{zeroize::Zeroize, ExposeSecret, SecretString};
use std::collections::{BTreeMap, HashSet};
use std::env::consts;
use std::io::{prelude::*, ErrorKind};
use std::iter;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::num::NonZeroUsize;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use std::{assert, thread};
use sysinfo::{CpuRefreshKind, MemoryRefreshKind, RefreshKind, System};
use tracing::*;

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

#[derive(Debug)]
pub enum UiMessage {
    Quit(String),
    CursorImage {
        id: usize,
        cursor_data: CursorData,
    },
    CursorChange {
        id: usize,
    },
    DisplayChange {
        display_config: display::DisplayConfiguration,
    },
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
                let display_config: display::DisplayConfiguration = dc.try_into()?;
                Ok(UiMessage::DisplayChange { display_config })
            }
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

fn generate_tunnel_requests(
    client_bound_requests: &[PortForwardArg],
    server_bound_requests: &[PortForwardArg],
) -> BTreeMap<u64, tunnel::Description> {
    let arg2desc = |pf_arg: &PortForwardArg, side: tunnel::Side| tunnel::Description {
        side: side.into(),
        protocol: tunnel::Protocol::Tcp.into(),
        bind_address: pf_arg.bind_addr.clone().unwrap_or_default(),
        bind_port: pf_arg.bind_port as u32,
        forward_address: pf_arg.clone().forward_addr,
        forward_port: pf_arg.forward_port as u32,
    };
    let client_bound_requests = client_bound_requests
        .iter()
        .map(|pf_arg| arg2desc(pf_arg, tunnel::Side::Client));
    let server_bound_requests = server_bound_requests
        .iter()
        .map(|pf_arg| arg2desc(pf_arg, tunnel::Side::Server));
    client_bound_requests
        .chain(server_bound_requests)
        .enumerate()
        .map(|(idx, desc)| (idx as u64, desc))
        .collect()
}

#[derive(Clone, Debug, PartialEq)]
pub struct PortForwardArg {
    bind_addr: Option<String>,
    bind_port: u16,
    forward_addr: String,
    forward_port: u16,
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

impl FromStr for PortForwardArg {
    type Err = Error;

    fn from_str(s: &str) -> Result<PortForwardArg> {
        // Split the input into parts by colon
        let parts: Vec<&str> = s.split(':').collect();

        match parts.len() {
            // Format: [bind_address:]bind_port:forward_addr:forward_port
            3 | 4 => {
                let bind_addr = if parts.len() == 4 {
                    Some(parts[0].to_string())
                } else {
                    None
                };
                let bind_port = parts[parts.len() - 3]
                    .parse::<u16>()
                    .map_err(|_| anyhow!("Invalid bind port"))?;
                let forward_addr = parts[parts.len() - 2].to_string();
                let forward_port = parts[parts.len() - 1]
                    .parse::<u16>()
                    .map_err(|_| anyhow!("Invalid forward port"))?;

                Ok(PortForwardArg {
                    bind_addr,
                    bind_port,
                    forward_addr,
                    forward_port,
                })
            }
            _ => Err(anyhow!(
                "Invalid format: expected '[bind_address:]port:host:hostport'"
            )),
        }
    }
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
#[derive(Builder, Clone, Debug)]
pub struct Configuration {
    pub name: String,
    pub auth_token: SecretString,
    pub decoder_max: NonZeroUsize,
    pub server_addr: String,
    pub initial_display_mode: DisplayMode,
    #[builder(setter(strip_option), default)]
    pub program_cmdline: Option<String>,
    #[builder(setter(name = "additional_tls_certificate", custom), default)]
    pub additional_tls_certificates: Vec<X509>,
    #[builder(default)]
    pub allow_insecure_connection: bool,
    #[builder(default)]
    pub client_bound_tunnel_reqs: Vec<PortForwardArg>,
    #[builder(default)]
    pub server_bound_tunnel_reqs: Vec<PortForwardArg>,
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

#[derive(Debug, Clone)]
pub enum MonitorGeometry {
    Multi { origin: Rect, other: EuclidSet },
    Single { size: Size },
}

impl<'mg> IntoIterator for &'mg MonitorGeometry {
    type Item = Rect;
    type IntoIter = Box<dyn Iterator<Item = Rect> + 'mg>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl MonitorGeometry {
    pub fn extent(&self) -> Rect {
        match self {
            MonitorGeometry::Multi { origin, other } => {
                iter::once(*origin).chain(other).extent().unwrap()
            }
            MonitorGeometry::Single { size } => Rect::from_size(*size),
        }
    }

    pub fn is_single(&self) -> bool {
        matches!(self, MonitorGeometry::Single { .. })
    }

    pub fn is_multi(&self) -> bool {
        matches!(self, MonitorGeometry::Multi { .. })
    }

    pub fn iter(&self) -> Box<dyn Iterator<Item = Rect> + '_> {
        match self {
            MonitorGeometry::Multi { origin, other } => Box::new(iter::once(*origin).chain(other)),
            MonitorGeometry::Single { size } => Box::new(iter::once(Rect::from_size(*size))),
        }
    }

    pub fn single_monitor_size(&self) -> Size {
        match self {
            MonitorGeometry::Multi { origin, .. } => origin.size,
            MonitorGeometry::Single { size } => *size,
        }
    }

    pub fn num_monitors(&self) -> usize {
        match self {
            MonitorGeometry::Multi { other, .. } => other.len() + 1,
            MonitorGeometry::Single { .. } => 1,
        }
    }

    pub fn identify_display_mode(
        &self,
        server_display_config: &display::DisplayConfiguration,
        is_fullscreened: bool,
        is_multimonitored: bool,
    ) -> Result<DisplayMode> {
        ensure!(
            server_display_config.display_decoder_infos.len() >= 1,
            "Server display configuration must have at least one display decoder info"
        );
        trace!(?is_fullscreened, ?is_multimonitored, ?server_display_config);

        let dc_rects = server_display_config
            .display_decoder_infos
            .iter()
            .filter(|ddi| ddi.display.is_enabled)
            .map(|ddi| ddi.display.area)
            .collect::<HashSet<_>>();
        let first_dc_rect = dc_rects.iter().next().unwrap();

        let windowed = DisplayMode::Windowed {
            size: first_dc_rect.size,
        };

        match (self, dc_rects.len(), is_fullscreened, is_multimonitored) {
            (MonitorGeometry::Single { size }, 1, true, _) => {
                if first_dc_rect.size.width > size.width || first_dc_rect.size.height > size.height
                {
                    warn!(
                        "Server provided display configuration with display size {}x{}, but the monitor is only {}x{}. There may be visual artifacts, including cut-off parts of the display.",
                        first_dc_rect.size.width,
                        first_dc_rect.size.height,
                        size.width,
                        size.height
                    );
                }
                Ok(DisplayMode::SingleFullscreen)
            }
            (MonitorGeometry::Single { .. }, _, true, _) => {
                bail!("Server provided multi-display configuration, but only one monitor is configured.");
            }
            (MonitorGeometry::Multi { .. }, 1, true, true) => {
                warn!("Server provided single-display configuration with multi-monitor geometry and multi-monitor mode. Falling back to single-monitor mode.");
                Ok(DisplayMode::SingleFullscreen)
            }
            (MonitorGeometry::Multi { origin, other }, _, true, true) => {
                let num_server_displays = server_display_config.display_decoder_infos.len();
                if num_server_displays < self.num_monitors() {
                    warn!("Server supports only {} displays, but client has {} monitors. Falling back single-monitor mode", num_server_displays, self.num_monitors());
                    return Ok(DisplayMode::SingleFullscreen);
                }
                let mg_rects = iter::once(*origin).chain(other).collect::<HashSet<_>>();
                if mg_rects != dc_rects {
                    warn!("Server provided display configuration does not match client monitor geometry. Falling back to windowed mode.");
                    return Ok(windowed);
                }
                Ok(DisplayMode::MultiFullscreen)
            }
            (MonitorGeometry::Multi { origin, other }, 1, true, false) => {
                let mg_sizes = iter::once(*origin)
                    .chain(other)
                    .map(|r| r.size)
                    .collect::<HashSet<_>>();
                if !mg_sizes.contains(&first_dc_rect.size) {
                    warn!(
                        "Server provided single-display configuration ({}x{}), but it does not match any monitor. Falling back to windowed mode.",
                        first_dc_rect.size.width,
                        first_dc_rect.size.height,
                    );
                    return Ok(windowed);
                }
                Ok(DisplayMode::SingleFullscreen)
            }
            (MonitorGeometry::Multi { .. }, _, true, false) => {
                bail!("Server provided multi-display configuration in single-monitor mode");
            }
            (_, 1, false, _) => Ok(windowed),
            (_, _, false, _) => {
                bail!("Server provided multi-display configuration in windowed mode");
            }
        }
    }
}

pub struct Client {
    config: Configuration,
    monitor_geometry: Option<MonitorGeometry>,
    id: Option<u64>,
    server_name: Option<String>,
    control_jh: Option<thread::JoinHandle<()>>,
    should_stop: Arc<AtomicBool>,
    display_config: Option<display::DisplayConfiguration>,
    local_addr: Option<SocketAddr>,
    requested_tunnels: BTreeMap<u64, tunnel::Description>,
    allocated_tunnels: BTreeMap<u64, tunnel::Response>,
}

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

        this.monitor_geometry = Some(gtk3::get_monitor_geometry()?);
        debug!(monitor_geometry=?this.monitor_geometry);

        let (cc, ec, mc, tc) = this.setup_unified_channel()?;
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
                DisplayMode::MultiFullscreen => match self.monitor_geometry.as_ref().unwrap() {
                    MonitorGeometry::Multi { origin, other } => iter::once(*origin)
                        .chain(other)
                        .map(|rect| DisplayInfo {
                            area: Some(Rectangle::try_from(rect).expect("Failed to generate area")),
                            is_enabled: true,
                        })
                        .collect(),
                    MonitorGeometry::Single { size } => {
                        vec![DisplayInfo {
                            area: Some(
                                Rectangle::try_from_size_at_origin(*size)
                                    .expect("Failed to generate area"),
                            ),
                            is_enabled: true,
                        }]
                    }
                },
                DisplayMode::SingleFullscreen => {
                    let size = match self.monitor_geometry.as_ref().unwrap() {
                        MonitorGeometry::Multi { origin, .. } => origin.size,
                        MonitorGeometry::Single { size } => *size,
                    };
                    vec![DisplayInfo {
                        area: Some(
                            Rectangle::try_from_size_at_origin(size)
                                .expect("Failed to generate area"),
                        ),
                        is_enabled: true,
                    }]
                }
            }
        };

        ClientInfoBuilder::default()
            .version(SemVer::from_cargo().expect("extract version from cargo"))
            .os(match consts::OS {
                "linux" => Os::Linux,
                "windows" => Os::Windows,
                "macos" => Os::Mac,
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
                arch => panic!("Unsupported architcture {}", arch),
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

    fn setup_tunnel_channel(&mut self, tc: channel::ClientTunnel) -> Result<()> {
        let (mut tc_rx, mut tc_tx) = tc.split();
        let mut tunnels = BTreeMap::new();

        let mut server_bound_tunnels = BTreeMap::new();
        let mut client_bound_tunnels = BTreeMap::new();
        for id in self.requested_tunnels.keys() {
            let response = self
                .allocated_tunnels
                .get(id)
                .ok_or(anyhow!("missing tunnel ID: {}", id))?;
            match response
                .message
                .as_ref()
                .ok_or(anyhow!("missing response message"))?
            {
                tunnel::response::Message::Success(desc) => {
                    let this_side = tunnel::Side::try_from(desc.side)?;
                    let other_side = if this_side == tunnel::Side::Client {
                        tunnel::Side::Server
                    } else {
                        tunnel::Side::Client
                    };
                    info_always!(
                        "allocated {:?} port {}:{} for forward to {}:{} on {:?}",
                        this_side,
                        desc.bind_address,
                        desc.bind_port,
                        desc.forward_address,
                        desc.forward_port,
                        other_side,
                    );

                    match this_side {
                        tunnel::Side::Server => server_bound_tunnels.insert(*id, desc.clone()),
                        tunnel::Side::Client => client_bound_tunnels.insert(*id, desc.clone()),
                    };
                }
                tunnel::response::Message::Failure(reason) => {
                    let desc = self.requested_tunnels.get(id).unwrap();
                    warn!(
                        "server failed binding: {}:{}: '{}'",
                        desc.bind_address, desc.bind_port, reason
                    );
                }
            }
        }
        debug!(?server_bound_tunnels);
        debug!(?client_bound_tunnels);

        let (new_tcp_streams_tx, new_tcp_streams_rx) = unbounded();
        let (stream_res_tx, stream_res_rx) = unbounded();
        let (data_to_server_tx, data_to_server_rx) = unbounded();
        let (data_from_server_tx, data_from_server_rx) = unbounded();
        let (closes_from_server_tx, closes_from_server_rx) = unbounded();
        let (closes_to_server_tx, closes_to_server_rx) = unbounded();
        let (opens_from_server_tx, opens_from_server_rx) = unbounded();

        for (tid, desc) in &client_bound_tunnels {
            let desc = desc.clone();
            debug!(?desc);
            let new_tcp_streams_tx = new_tcp_streams_tx.clone();
            let tid = *tid;

            thread::spawn::<_, Result<()>>(move || {
                let mut sid = 0;

                let bind_addr = if desc.bind_address.is_empty() {
                    // TODO: fix IPv6 behavior. Right now, our default is just to bind to IPv4,
                    // which may not always be right
                    IpAddr::V4(Ipv4Addr::UNSPECIFIED)
                } else {
                    desc.bind_address.parse::<IpAddr>()?
                };

                let listener = match TcpListener::bind((bind_addr, desc.bind_port.try_into()?)) {
                    Ok(listener) => listener,
                    Err(error) => {
                        warn!(%error, "client failed binding: {:?}:{}", bind_addr, desc.bind_port);
                        return Err(error.into());
                    }
                };

                loop {
                    let (stream, _) = listener.accept()?;
                    trace!(tid, sid, "accepted");
                    new_tcp_streams_tx.send((tid, sid, stream))?;
                    trace!(tid, sid, "forwarded to new stream handler");
                    sid += 1;
                }
            });
        }

        thread::spawn::<_, Result<()>>(move || {
            while let Ok(s2c) = tc_rx.receive() {
                match s2c.message {
                    Some(tunnel::message::Message::OpenTcp(_)) => {
                        opens_from_server_tx.send((s2c.tunnel_id, s2c.stream_id))?;
                    }
                    Some(tunnel::message::Message::CloseTcp(_)) => {
                        closes_from_server_tx.send((s2c.tunnel_id, s2c.stream_id))?;
                    }
                    Some(tunnel::message::Message::TcpData(tcp_data)) => {
                        data_from_server_tx.send((s2c.tunnel_id, s2c.stream_id, tcp_data.data))?;
                    }
                    None => warn!("empty tunnel message"),
                }
            }
            bail!("failed receiving tunnel message");
        });

        thread::spawn::<_, Result<()>>(move || loop {
            select! {
                recv(opens_from_server_rx) -> open_from_server_res => trace_span!("open-from-server").in_scope(|| {
                    let (tid, sid) = open_from_server_res?;
                    trace!(tid, sid);
                    let desc = match server_bound_tunnels.get(&tid) {
                        Some(desc) => desc,
                        None => {
                            warn!(tid, sid, "open tcp for non-server-bound tunnel");
                            closes_to_server_tx.send((tid, sid))?;
                            return Ok(());
                        }
                    };
                    if desc.side() == tunnel::Side::Client {
                        warn!(tid, sid, "open tcp for client-bound tunnel");
                        closes_to_server_tx.send((tid, sid))?;
                        return Ok(());
                    }
                    if tunnels.contains_key(&(tid, sid)) {
                        warn!(tid, sid, "open for existing tunnel/stream");
                        closes_to_server_tx.send((tid, sid))?;
                        return Ok(());
                    }

                    let (forward_addr, forward_port) = (desc.forward_address.clone(), desc.forward_port as u16);
                    let new_tcp_streams_tx = new_tcp_streams_tx.clone();

                    let closes_to_server_tx = closes_to_server_tx.clone();
                    thread::spawn::<_, Result<()>>(move || {
                        let mut stream = (forward_addr.as_str(), forward_port)
                            .to_socket_addrs()
                            .unwrap_or(Vec::default().into_iter())
                            .map(TcpStream::connect)
                            .filter_map(Result::ok);

                        if let Some(stream) = stream.next() {
                            new_tcp_streams_tx.send((tid, sid, stream))?;
                        } else {
                            closes_to_server_tx.send((tid, sid))?;
                        }

                        Ok(())
                    });

                    tunnels.insert((tid, sid), TunnelState::Opening { queued: vec![], should_terminate: Arc::new(AtomicBool::new(false)) });
                    Ok::<_, Error>(())
                })?,
                recv(new_tcp_streams_rx) -> new_tcp_stream_res => trace_span!("new-tcp-stream").in_scope(|| {
                    let (tid, sid, tcp_stream) = new_tcp_stream_res?;
                    trace!(tid, sid, ?tcp_stream);

                    let (queued, should_terminate) = match tunnels.remove(&(tid, sid)) {
                        Some(TunnelState::Opening { queued, should_terminate }) => (queued, should_terminate),
                        Some(TunnelState::Opened { should_terminate, .. }) => {
                            warn!("already open, ignoring");
                            should_terminate.store(true, Ordering::Relaxed);
                            return Ok(());
                        }
                        Some(TunnelState::HalfClosed) => {
                            warn!("half closed");
                            return Ok(());
                        }
                        Some(TunnelState::FullyClosed) => {
                            warn!("fully closed");
                            return Ok(());
                        }
                        None => {
                            if client_bound_tunnels.contains_key(&tid) {
                                let msg = tunnel::MessageBuilder::default()
                                    .tunnel_id(tid)
                                    .stream_id(sid)
                                    .message(tunnel::OpenTcpStream::new())
                                    .build()
                                    .expect("build tunnel message");
                                tc_tx.send(msg)?;
                                (vec![], Arc::new(AtomicBool::new(false)))
                            } else {
                                warn!("non-existent");
                                return Ok(());
                            }
                        }
                    };

                    const IO_TIMEOUT: Duration = Duration::from_millis(1);
                    tcp_stream.set_nonblocking(false)?;
                    tcp_stream.set_read_timeout(IO_TIMEOUT.into())?;
                    let mut rh = tcp_stream.try_clone()?;
                    let mut wh = tcp_stream;
                    let (to_tx, to_tx_rx) = bounded::<Vec<u8>>(1);

                    let stream_res_tx_wh = stream_res_tx.clone();
                    let should_terminate_wh = should_terminate.clone();
                    thread::spawn(move || {
                        let _s = trace_span!("tunnel-stream-write-thread", tid, sid).entered();
                        if let Err(e) = wh.write_all(&queued) {
                            stream_res_tx_wh.send((tid, sid, Err::<(), _>(e.into()))).expect("stream_res_tx_wh");
                            return;
                        }
                        let res = loop {
                            if should_terminate_wh.load(Ordering::Relaxed) {
                                break Ok(());
                            }
                            let data = match to_tx_rx.recv_timeout(IO_TIMEOUT) {
                                Ok(data) => data,
                                Err(RecvTimeoutError::Timeout) => continue,
                                Err(RecvTimeoutError::Disconnected) => break Err(anyhow!("disconnected")),
                            };
                            if let Err(e) = wh.write_all(&data) {
                                break Err(e.into());
                            }
                        };
                        stream_res_tx_wh.send((tid, sid, res)).expect("stream_res_tx_wh");
                        trace!(tid, sid, "terminating write half");
                    });

                    let stream_res_tx_rh = stream_res_tx.clone();
                    let data_to_server_tx = data_to_server_tx.clone();
                    let should_terminate_rh = should_terminate.clone();
                    thread::spawn(move || {
                        let _s = trace_span!("tunnel-stream-read-thread", tid, sid).entered();
                        let res: Result<_> = loop {
                            if should_terminate_rh.load(Ordering::Relaxed) {
                                break Ok(());
                            }
                            let mut data = vec![0_u8; 0x1000];
                            let nbytes = match rh.read(&mut data) {
                                Ok(nbytes_read) => nbytes_read,
                                Err(ref e) if e.kind() == ErrorKind::WouldBlock => continue,
                                Err(e) => break Err(e.into()),
                            };
                            if nbytes == 0 {
                                break Ok(());
                            } else {
                                data.truncate(nbytes);
                                if let Err(e) = data_to_server_tx.send((tid, sid, data)) {
                                    break Err(e.into());
                                }
                            }
                        };
                        stream_res_tx_rh.send((tid, sid, res)).expect("stream_res_tx_rh");
                        trace!(tid, sid, "terminating read half");
                    });

                    tunnels.insert((tid, sid), TunnelState::Opened { to_tx, should_terminate });
                    Ok::<_, Error>(())
                })?,
                recv(stream_res_rx) -> stream_res => trace_span!("stream-thread-terminated").in_scope(|| {
                    let (tid, sid, res) = stream_res?;
                    trace!(tid, sid, ?res);
                    match tunnels.remove(&(tid, sid)) {
                        Some(TunnelState::Opening { queued, .. }) => warn!(?queued, "closing opening tunnel"),
                        Some(TunnelState::Opened { should_terminate, .. }) => {
                            trace!("closing first half");
                            should_terminate.store(true, Ordering::Relaxed);
                            tunnels.insert((tid, sid), TunnelState::HalfClosed);
                        },
                        Some(TunnelState::HalfClosed) => {
                            trace!("closing second half");
                            tunnels.insert((tid, sid), TunnelState::FullyClosed);
                        },
                        Some(TunnelState::FullyClosed) => {
                            closes_to_server_tx.send((tid, sid))?;
                        }
                        None => warn!("non-existent"),
                    }
                    Ok::<_, Error>(())
                })?,
                recv(data_to_server_rx) -> data_to_server_res => trace_span!("data-to-server").in_scope(|| {
                    let (tid, sid, data) = data_to_server_res?;
                    trace!(tid, sid, ?data);
                    let msg = tunnel::MessageBuilder::default()
                        .tunnel_id(tid)
                        .stream_id(sid)
                        .message(tunnel::TcpData::new(data))
                        .build()
                        .expect("build tunnel message");
                    tc_tx.send(msg)?;
                    Ok::<_, Error>(())
                })?,
                recv(data_from_server_rx) -> data_from_server_res => trace_span!("data-from-server").in_scope(|| {
                    let (tid, sid, data) = data_from_server_res?;
                    trace!(tid, sid, ?data);
                    match tunnels.get_mut(&(tid, sid)) {
                        Some(TunnelState::Opened { to_tx, .. }) => {
                            trace!("opened");
                            to_tx.send(data)?;
                        },
                        Some(TunnelState::Opening { queued, .. }) => {
                            trace!("opening");
                            queued.extend(data);
                        },
                        Some(TunnelState::HalfClosed) => warn!("half closed"),
                        Some(TunnelState::FullyClosed) => warn!("fully closed"),
                        None => warn!(tid, sid, "non-existent tunnel/stream"),
                    }
                    Ok::<_, Error>(())
                })?,
                recv(closes_to_server_rx) -> close_to_server_res => trace_span!("close-to-server").in_scope(|| {
                    let (tid, sid) = close_to_server_res?;
                    trace!(tid, sid);
                    let msg = tunnel::MessageBuilder::default()
                        .tunnel_id(tid)
                        .stream_id(sid)
                        .message(tunnel::CloseTcpStream::new())
                        .build()
                        .expect("build tunnel message");
                    tc_tx.send(msg)?;
                    Ok::<_, Error>(())
                })?,
                recv(closes_from_server_rx) -> close_from_server_res => trace_span!("close-from-server").in_scope(|| {
                    let (tid, sid) = close_from_server_res?;
                    trace!(tid, sid);
                    match tunnels.remove(&(tid, sid)) {
                        Some(TunnelState::Opening { should_terminate, .. }) |
                        Some(TunnelState::Opened { should_terminate, .. }) => {
                            should_terminate.store(true, Ordering::Relaxed);
                            trace!(tid, sid, "marking half closed");
                            tunnels.insert((tid, sid), TunnelState::HalfClosed);
                        },
                        Some(TunnelState::HalfClosed) => {
                            trace!(tid, sid, "marking fully closed");
                            tunnels.insert((tid, sid), TunnelState::FullyClosed);
                        },
                        Some(TunnelState::FullyClosed) => (),
                        None => warn!(tid, sid, "Close from server for tunnel/stream which does not exist"),
                    }
                    Ok::<_, Error>(())
                })?,
            }
        });

        Ok(())
    }

    fn setup_media_channel(
        &mut self,
        mut mc: channel::ClientMedia,
        notify_media_rx: Receiver<display::DisplayConfiguration>,
        img_tx: glib::Sender<Image>,
        img_rx: Receiver<Image>,
    ) -> Result<()> {
        let (mm_rx_tx, mm_rx_rx) = unbounded();
        thread::spawn::<_, Result<()>>(move || loop {
            let msg = mc.receive()?;
            mm_rx_tx.send(msg)?;
        });

        let mut framer = Framer::new(self.display_config.as_ref().unwrap().clone());

        thread::spawn::<_, Result<()>>(move || loop {
            let draw_recv = if framer.has_draws() {
                Some(&img_rx)
            } else {
                None
            };

            select! {
                recv(mm_rx_rx) -> msg_res => {
                    let Some(msg) = msg_res?.message else {
                        warn!("media message with empty body");
                        continue;
                    };
                    if let Err(e) = framer.report_mm(msg) {
                        warn!("Error processing media message: {}", e)
                    }
                },
                recv(notify_media_rx) -> msg_res => {
                    let new_config = msg_res?;
                    if new_config.id > framer.display_config.id {
                        framer = Framer::new(new_config);
                    }
                },
                recv(draw_recv.unwrap_or(&never())) -> img_res => {
                    let mut img = img_res?;
                    if img.display_config_id < framer.display_config.id {
                        img.update_display_config(&framer.display_config);
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

        thread::spawn(move || {
            loop {
                match client_cc_read.receive() {
                    Ok(cm_s2c) => {
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
                        error!(
                            "Failed to receive with error: {}. Server may already be gone",
                            err
                        );
                        control_rx_tx
                            .send(Err(err))
                            .unwrap_or_else(|e| warn!("failed to forward error to Tx half: {}", e));
                        break;
                    }
                }
            }
            trace!("Control channel rx exiting");
        });

        thread::spawn(move || {
            while let Ok(cm_c2s) = control_tx_rx.recv() {
                let should_exit = matches!(cm_c2s, cm_c2s::Message::ClientGoodbye(_));
                if let Err(err) = client_cc_write.send(cm_c2s) {
                    error!("Failed to send: {}", err);
                    break;
                }

                if should_exit {
                    break;
                }
            }
            trace!("Control channel tx exiting");
        });

        //
        // Setup SIGINT handler
        //
        ctrlc::set_handler(move || {
            info!("Received Ctrl-C, exiting");
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
                        Ok(Ok(cm_s2c::Message::ServerGoodbye(gb))) => {
                            if let Err(err) =
                                ui_tx.send(UiMessage::Quit(String::from("Goodbye!")))
                                {
                                    warn!("Failed to send QuitMessage: {}", err);
                                }
                            if gb.reason() == cm::ServerGoodbyeReason::ShuttingDown {
                                warn!("Server is shutting down");
                            } else if gb.reason() == cm::ServerGoodbyeReason::OtherLogin {
                                info!("Server was logged into elsewhere. Exiting");
                            } else if gb.reason() == cm::ServerGoodbyeReason::InactiveTimeout {
                                info!("Client has been inactive for too long. Exiting");
                            } else {
                                error!("Server sent goodbye with reason: {}", gb.reason().as_str_name());
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
        notify_media_tx: Sender<display::DisplayConfiguration>,
        ui_tx: glib::Sender<UiMessage>,
    ) -> Result<()> {
        let (mut ec_rx, mut ec_tx) = client_ec.split();
        let should_stop = self.should_stop.clone();

        thread::spawn::<_, Result<()>>(move || {
            debug!("Event channel Rx started");
            loop {
                let msg = ec_rx.receive()?;
                trace!(?msg);

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
        channel::ClientTunnel,
    )> {
        let server_input = self.config.server_addr.as_str();
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
        for cert in &self.config.additional_tls_certificates {
            debug!("Adding cert: {:?}", cert);
            channel_client_builder = channel_client_builder
                .additional_tls_pem_certificate(&String::from_utf8_lossy(&cert.to_pem()?));
        }
        if self.config.allow_insecure_connection {
            channel_client_builder = channel_client_builder.allow_insecure_connection();
        }
        let mut channel_client = channel_client_builder.build_sync()?;
        let channel_session = channel_client.connect(&server_addr, server_port)?;
        self.local_addr = Some(channel_session.local_addr()?);
        Ok(channel_session.split())
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
        config.initial_display_mode,
        client.monitor_geometry.as_ref().unwrap().clone(),
        client.display_config.as_ref().unwrap().clone(),
    );

    client.shutdown();

    Ok(())
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::gtk3;
    use aperturec_protocol::control::ServerInitBuilder;
    use test_log::test;

    fn generate_configuration(
        auth_token: SecretString,
        dec_max: NonZeroUsize,
        width: usize,
        height: usize,
        server_port: u16,
    ) -> Configuration {
        ConfigurationBuilder::default()
            .auth_token(auth_token)
            .decoder_max(dec_max)
            .name(String::from("test_client"))
            .server_addr(format!("127.0.0.1:{}", server_port))
            .initial_display_mode(DisplayMode::Windowed {
                size: (width, height).into(),
            })
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

        let (width, height) = gtk3::get_fullscreen_dims().expect("fullscreen dims");

        let mut config = generate_configuration(
            SecretString::default(),
            8.try_into().unwrap(),
            width.try_into().unwrap(),
            height.try_into().unwrap(),
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
                            .display_decoder_infos(vec![DisplayDecoderInfoBuilder::default()
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
                                .decoder_areas(vec![RectangleBuilder::default()
                                    .location(Location::new(0, 0))
                                    .dimension(Dimension::new(800_u64, 600_u64))
                                    .build()
                                    .expect("Rectangle build")])
                                .build()
                                .expect("DisplayDecoderInfo build")])
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
