use crate::backend::{Backend, CursorChange, CursorImage, Event, LockState};
use crate::metrics::{BackendEvent, DisplayHeight, DisplayWidth};
use crate::process_utils;

use aperturec_graphics::prelude::*;
use aperturec_protocol::event as em;

use anyhow::{anyhow, bail, Result};
use futures::{future, stream, Stream, StreamExt, TryFutureExt};
use ndarray::{prelude::*, AssignElem};
use rand::distributions::{Alphanumeric, DistString};
use std::fmt::{self, Debug, Formatter};
use std::iter::Iterator;
use std::ops::{Deref, DerefMut};
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tracing::*;
use xcb::damage::{self, Damage};
use xcb::x::{self, Drawable, GetImage, ImageFormat, ScreenBuf, Window};
use xcb::{randr, xfixes, xtest, BaseEvent, Connection, Extension};
use xcb::{CookieWithReplyChecked, Request, RequestWithReply, RequestWithoutReply};

const CHILD_STARTUP_WAIT_TIME_MS: Duration = Duration::from_millis(1000);

pub const X_BYTES_PER_PIXEL: usize = 4;

fn u8_for_button(button: &em::Button) -> u8 {
    match button.kind {
        Some(em::button::Kind::MappedButton(idx)) => idx as u8 - 1,
        Some(em::button::Kind::UnmappedButton(idx)) => idx as u8,
        None => panic!("Button with no interior mapping"),
    }
}

#[derive(Debug)]
pub struct X {
    pub display_name: String,
    max_width: usize,
    max_height: usize,
    connection: XConnection,
    last_created_mode: Option<randr::Mode>,
    root_drawable: Drawable,
    root_process: Option<Child>,
    root_process_exited: bool,
    _xvfb_proc: Child,
}

async fn do_exec_command(display_name: &str, command: &mut Command) -> Result<Child> {
    command.env("DISPLAY", display_name);
    command.env("XDG_SESSION_TYPE", "x11");
    command.stdout(process_utils::StdoutTracer::new(Level::TRACE));
    command.stderr(process_utils::StderrTracer::new(Level::TRACE));
    command.process_group(0);
    debug!("Starting command `{:?}`", command);
    let process = command.spawn()?;
    debug!("Launched {:?}", process);
    Ok(process)
}

impl From<xfixes::GetCursorImageReply> for CursorImage {
    fn from(reply: xfixes::GetCursorImageReply) -> Self {
        CursorImage {
            serial: reply.cursor_serial(),
            width: reply.width(),
            height: reply.height(),
            x_hot: reply.xhot(),
            y_hot: reply.yhot(),

            data: reply
                .cursor_image()
                .to_vec()
                .iter()
                .flat_map(|&x| x.to_le_bytes())
                .collect(),
        }
    }
}

// B-G-R-A format
#[derive(Clone, Copy)]
pub struct XPixel([u8; X_BYTES_PER_PIXEL]);

impl XPixel {
    fn blue(&self) -> u8 {
        self[0]
    }

    fn green(&self) -> u8 {
        self[1]
    }

    fn red(&self) -> u8 {
        self[2]
    }

    fn alpha(&self) -> u8 {
        self[3]
    }
}

impl Deref for XPixel {
    type Target = [u8; X_BYTES_PER_PIXEL];

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for XPixel {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl PartialEq<Pixel24> for XPixel {
    fn eq(&self, rhs: &Pixel24) -> bool {
        self.alpha() == u8::MAX
            && self.blue() == rhs.blue
            && self.green() == rhs.green
            && self.red() == rhs.red
    }
}

impl AssignElem<XPixel> for &mut Pixel24 {
    fn assign_elem(self, x: XPixel) {
        self.blue = x.blue();
        self.green = x.green();
        self.red = x.red();
    }
}

pub struct XPixelMap {
    reply: x::GetImageReply,
    size: Size,
}

impl XPixelMap {
    fn new(area: &Rect, reply: x::GetImageReply) -> Self {
        XPixelMap {
            reply,
            size: area.size,
        }
    }
}

impl From<&xfixes::CursorNotifyEvent> for CursorChange {
    fn from(cn: &xfixes::CursorNotifyEvent) -> Self {
        CursorChange {
            serial: cn.cursor_serial(),
        }
    }
}

impl PixelMap for XPixelMap {
    type Pixel = XPixel;

    fn as_ndarray(&self) -> ArrayView2<Self::Pixel> {
        // SAFETY: self.reply.data() returns a slice with a lifetime of self and the
        // returned ArrayView is guaranteed only live as long as self is borrowed
        unsafe {
            ArrayView2::from_shape_ptr(
                (self.size.height, self.size.width),
                self.reply.data() as *const [u8] as *const XPixel,
            )
        }
    }
}

impl Backend for X {
    type PixelMap = XPixelMap;

    fn root_process_exited(&self) -> bool {
        self.root_process_exited
    }

    async fn wait_root_process(&mut self) -> Result<ExitStatus> {
        match self.root_process {
            None => future::pending().await,
            Some(ref mut rp) => {
                let es = rp.wait().await?;
                self.root_process_exited = true;
                debug!("Root program exited with result {}", es);
                Ok(es)
            }
        }
    }

    fn start_kill_root_process(&mut self) -> Result<()> {
        if let Some(root_process) = self.root_process.as_mut() {
            Ok(root_process.start_kill()?)
        } else {
            anyhow::bail!("No root process to kill");
        }
    }

    async fn exec_command(&mut self, command: &mut Command) -> Result<Child> {
        do_exec_command(&self.display_name, command).await
    }

    async fn capture_area(&self, area: Rect) -> Result<Self::PixelMap> {
        let get_image_reply = self
            .connection
            .checked_request_with_reply(GetImage {
                format: ImageFormat::ZPixmap,
                drawable: self.root_drawable,
                x: area.origin.x as i16,
                y: area.origin.y as i16,
                width: area.size.width as u16,
                height: area.size.height as u16,
                plane_mask: u32::MAX,
            })
            .await?;
        Ok(XPixelMap::new(&area, get_image_reply))
    }

    async fn damage_stream(&self) -> Result<impl Stream<Item = Rect> + Send + Unpin + 'static> {
        let size = self.resolution().await?;
        let rect = Rect {
            origin: Point::new(0, 0),
            size,
        };
        Ok(stream::iter([rect]).chain(
            self.connection
                .event_stream()
                .filter_map(|event| match &event.0 {
                    xcb::Event::Damage(damage::Event::Notify(notify_event)) => {
                        future::ready(Some(notify_event.area()))
                    }
                    _ => future::ready(None),
                })
                .map(|xcb_rect| Rect {
                    origin: Point::new(xcb_rect.x as usize, xcb_rect.y as usize),
                    size: Size::new(xcb_rect.width as usize, xcb_rect.height as usize),
                }),
        ))
    }

    async fn capture_cursor(&self) -> Result<CursorImage> {
        let reply = self
            .connection
            .checked_request_with_reply(xfixes::GetCursorImage {})
            .await?;

        Ok(reply.into())
    }

    async fn cursor_stream(
        &self,
    ) -> Result<impl Stream<Item = CursorChange> + Send + Unpin + 'static> {
        Ok(self
            .connection
            .event_stream()
            .filter_map(|event| match &event.0 {
                xcb::Event::XFixes(xfixes::Event::CursorNotify(cursor_notify_event)) => {
                    future::ready(Some(cursor_notify_event.into()))
                }
                _ => future::ready(None),
            }))
    }

    async fn initialize<N>(
        max_width: N,
        max_height: N,
        root_process_cmd: Option<&mut Command>,
    ) -> Result<Self>
    where
        N: Into<Option<usize>> + Send + Sync,
    {
        let max_width = max_width.into().unwrap_or(Self::DEFAULT_MAX_WIDTH);
        let max_height = max_height.into().unwrap_or(Self::DEFAULT_MAX_HEIGHT);

        debug!(
            "Starting X Server with max resolution {}x{}",
            max_width, max_height
        );

        let mut xvfb_cmd = Command::new("Xvfb");
        xvfb_cmd
            .arg("-screen")
            .arg("0")
            .arg(format!("{}x{}x{}", max_width, max_height, 24))
            .arg("-displayfd")
            .arg("1") // stdout
            .stdout(Stdio::piped())
            .stderr(process_utils::StderrTracer::new(Level::DEBUG))
            .kill_on_drop(true);
        debug!("Starting Xvfb with command `{:?}`", xvfb_cmd);
        let mut xvfb_proc = xvfb_cmd.spawn()?;
        time::sleep(CHILD_STARTUP_WAIT_TIME_MS).await;
        if let Ok(Some(status)) = xvfb_proc.try_wait() {
            bail!("Xvfb exited with status {}", status);
        }
        let mut xvfb_stdout =
            BufReader::new(xvfb_proc.stdout.as_mut().ok_or(anyhow!("No Xvfb stdout"))?);
        let mut display_name = String::new();
        xvfb_stdout.read_line(&mut display_name).await?;
        display_name = format!(":{}", display_name.trim().trim_end());
        trace!("Started X server with name \"{}\"", display_name);

        trace!("Creating new XConnection");
        let connection = XConnection::new(&*display_name)?;
        trace!("Querying for damage extension version");
        X::query_damage_version(&connection).await?;
        trace!("Querying for xfixes extension version");
        X::query_xfixes_version(&connection).await?;
        trace!("Creating root objects");
        let (root_damage, root_window) = X::create_root_objects(&connection);
        let root_drawable = Drawable::Window(root_window);
        trace!("Setting up damage monitor");
        X::setup_damage_monitor(&connection, &root_damage, &root_drawable).await?;
        trace!("Setting up xtest extension");
        X::setup_xtest_extension(&connection).await?;
        trace!("Setting up xfixes monitor");
        X::setup_xfixes_monitor(&connection, &root_window).await?;

        let root_process = match root_process_cmd {
            None => None,
            Some(cmd) => Some(do_exec_command(&display_name, cmd).await?),
        };

        trace!("X initialized");
        Ok(X {
            display_name,
            max_width,
            max_height,
            connection,
            last_created_mode: None,
            root_drawable,
            root_process,
            root_process_exited: false,
            _xvfb_proc: xvfb_proc,
        })
    }

    async fn notify_event(&mut self, event: Event) -> Result<()> {
        match event {
            Event::Key { key, is_depressed } => {
                let keycode = key as x::Keycode;
                if keycode > self.connection.max_keycode || keycode < self.connection.min_keycode {
                    warn!(
                        "Received key event for key outside range ({},{}): {}. Dropping the event.",
                        self.connection.min_keycode, self.connection.max_keycode, keycode
                    );
                    return Ok(());
                }
                let event_type_num = if is_depressed {
                    x::KeyPressEvent::NUMBER
                } else {
                    // hard-coded for same reasons as pointer event
                    3
                } as u8;
                trace!(?key);
                let req = xtest::FakeInput {
                    r#type: event_type_num,
                    detail: key as u8,
                    time: x::CURRENT_TIME,
                    root: self.root_window()?,
                    root_x: 0,
                    root_y: 0,
                    deviceid: 0,
                };
                self.connection.checked_void_request(req).await?;
            }
            Event::Pointer {
                location,
                button_states,
                cursor: _,
            } => {
                for (button, is_depressed) in button_states {
                    let event_type_num = if is_depressed {
                        x::ButtonPressEvent::NUMBER
                    } else {
                        // We should just be able to use x::ButtonReleaseEvent::NUMBER here, but
                        // due to some of the xcb rust implementation
                        // (https://github.com/rust-x-bindings/rust-xcb/issues/238), we hard code
                        // this according to the C API values
                        // (https://xcb.freedesktop.org/manual/group__XCB____API.html)
                        5
                    } as u8;
                    debug!("event_type_num: {}", event_type_num);
                    let req = xtest::FakeInput {
                        r#type: event_type_num,
                        detail: u8_for_button(&button) + 1,
                        time: x::CURRENT_TIME,
                        root: self.root_window()?,
                        root_x: location.x_position as i16,
                        root_y: location.y_position as i16,
                        deviceid: 0,
                    };

                    trace!("Sending event {:#?}", req);
                    self.connection.checked_void_request(req).await?;
                }
                let req = xtest::FakeInput {
                    r#type: x::MotionNotifyEvent::NUMBER as u8,
                    // 0 == false: "For motion events, if this field is True , then rootX and rootY
                    // are relative distances from the current pointer location; if this field is
                    // False, then they are absolute positions."
                    // (https://www.x.org/releases/X11R7.7/doc/xextproto/xtest.html#Requests)
                    detail: 0,
                    time: x::CURRENT_TIME,
                    root: self.root_window()?,
                    root_x: location.x_position as i16,
                    root_y: location.y_position as i16,
                    deviceid: 0,
                };
                trace!("Sending event {:#?}", req);
                self.connection.checked_void_request(req).await?;
            }
            Event::Display { size } => {
                let size = Size::new(size.width as usize, size.height as usize);
                self.set_resolution(&size).await?;
            }
        }

        Ok(())
    }

    async fn set_lock_state(&self, lock_state: LockState) -> Result<()> {
        struct Lock {
            end_state: Option<bool>,
            keybutmask: x::KeyButMask,
            map_index: x::MapIndex,
        }

        debug!("Setting lock_state: {:?}", lock_state);

        let mut locks = vec![
            Lock {
                end_state: lock_state.is_caps_locked,
                map_index: x::MapIndex::Lock,
                keybutmask: x::KeyButMask::LOCK,
            },
            Lock {
                end_state: lock_state.is_num_locked,
                map_index: x::MapIndex::N2, // MOD2
                keybutmask: x::KeyButMask::MOD2,
            },
            Lock {
                end_state: lock_state.is_scroll_locked,
                map_index: x::MapIndex::N3, // MOD3
                keybutmask: x::KeyButMask::MOD3,
            },
        ];

        locks.retain(|l| l.end_state.is_some());

        let reply = self
            .connection
            .checked_request_with_reply(x::QueryPointer {
                window: self.root_window()?,
            })
            .await?;

        let mask = reply.mask();
        trace!("Current KeyButMask: {:?}", mask);

        locks.retain(|l| l.end_state.unwrap() != mask.contains(l.keybutmask));
        if locks.is_empty() {
            trace!("All locks correct, nothing to do!");
            return Ok(());
        }

        let reply = self
            .connection
            .checked_request_with_reply(x::GetModifierMapping {})
            .await?;

        let keycodes_per_modifier = reply.keycodes_per_modifier();

        for l in &mut locks {
            match reply
                .keycodes()
                .chunks(keycodes_per_modifier.into())
                .nth(l.map_index as usize)
                .and_then(|codes| codes.first().cloned())
            {
                Some(kc)
                    if kc > self.connection.min_keycode && kc < self.connection.max_keycode =>
                {
                    debug!("Using keycode {:?} for {:?}", kc, l.map_index);
                    let req = xtest::FakeInput {
                        r#type: x::KeyPressEvent::NUMBER as u8,
                        detail: kc,
                        time: x::CURRENT_TIME,
                        root: self.root_window()?,
                        root_x: 0,
                        root_y: 0,
                        deviceid: 0,
                    };
                    self.connection.checked_void_request(req).await?;

                    let req = xtest::FakeInput {
                        r#type: 3_u8,
                        detail: kc,
                        time: x::CURRENT_TIME,
                        root: self.root_window()?,
                        root_x: 0,
                        root_y: 0,
                        deviceid: 0,
                    };
                    self.connection.checked_void_request(req).await?;
                }
                _ => {
                    warn!(
                        "No keycode available for {:?}, modifier disabled",
                        l.map_index
                    );
                }
            };
        }

        trace!(
            "Final KeyButMask: {:?}",
            self.connection
                .checked_request_with_reply(x::QueryPointer {
                    window: self.root_window()?,
                })
                .await?
                .mask()
        );

        Ok(())
    }

    async fn set_resolution(&mut self, resolution: &Size) -> Result<()> {
        anyhow::ensure!(
            resolution.width <= self.max_width
                && resolution.height <= self.max_height
                && resolution.width >= Self::MIN_WIDTH
                && resolution.height >= Self::MIN_HEIGHT,
            "Resolution {}x{} is out of bounds {}x{}-{}x{}",
            resolution.width,
            resolution.height,
            Self::MIN_WIDTH,
            Self::MIN_HEIGHT,
            self.max_width,
            self.max_height
        );
        if resolution == &self.resolution().await? {
            return Ok(());
        }

        let window = self.root_window()?;
        let screen_resources = self.screen_resources().await?;
        if screen_resources.outputs().len() != 1 {
            bail!(
                "num outputs != 1 ({}), only one screen supported",
                screen_resources.outputs().len()
            );
        }
        if screen_resources.crtcs().len() != 1 {
            bail!(
                "num crtcs != 1 ({}), only one screen supported",
                screen_resources.crtcs().len()
            );
        }
        if screen_resources.modes().is_empty() {
            bail!("no modes for any output");
        }

        trace!(?screen_resources);
        let curr_output = screen_resources.outputs()[0];
        let curr_output_info = self
            .connection
            .checked_request_with_reply(randr::GetOutputInfo {
                config_timestamp: x::CURRENT_TIME,
                output: curr_output,
            })
            .await?;
        if curr_output_info.modes().is_empty() {
            bail!("no modes for current output");
        }
        let curr_crtc = curr_output_info.crtcs()[0];

        let name = format!(
            "aperturec_{}x{}_{}",
            resolution.width,
            resolution.height,
            Alphanumeric.sample_string(&mut rand::thread_rng(), 16)
        );
        let mut new_mode_info = screen_resources.modes()[0];
        new_mode_info.width = resolution.width as u16;
        new_mode_info.height = resolution.height as u16;
        new_mode_info.name_len = name.len() as u16;

        let create_mode_reply = self
            .connection
            .checked_request_with_reply(randr::CreateMode {
                window,
                mode_info: new_mode_info,
                name: name.as_bytes(),
            })
            .await?;

        self.connection
            .checked_void_request(randr::AddOutputMode {
                output: screen_resources.outputs()[0],
                mode: create_mode_reply.mode(),
            })
            .await?;

        self.connection
            .checked_request_with_reply(randr::SetCrtcConfig {
                crtc: curr_crtc,
                timestamp: x::CURRENT_TIME,
                config_timestamp: x::CURRENT_TIME,
                x: 0,
                y: 0,
                mode: create_mode_reply.mode(),
                rotation: randr::Rotation::ROTATE_0,
                outputs: &[curr_output],
            })
            .await?;

        if let Some(mode) = self.last_created_mode.replace(create_mode_reply.mode()) {
            if let Err(err) = self
                .connection
                .checked_void_request(randr::DeleteOutputMode {
                    output: screen_resources.outputs()[0],
                    mode,
                })
                .and_then(|_| {
                    self.connection
                        .checked_void_request(randr::DestroyMode { mode })
                })
                .await
            {
                warn!(?err);
            }
        }

        DisplayWidth::update(resolution.width as f64);
        DisplayHeight::update(resolution.height as f64);

        Ok(())
    }

    async fn resolution(&self) -> Result<Size> {
        let screen_resources = self.screen_resources().await?;
        trace!("crtcs: {:#?}", screen_resources.crtcs());
        if screen_resources.crtcs().len() != 1 {
            bail!(
                "Only support single monitor, {} monitors identified",
                screen_resources.crtcs().len()
            );
        }
        let crtc = screen_resources.crtcs()[0];
        let crtc_info = self
            .connection
            .checked_request_with_reply(randr::GetCrtcInfo {
                crtc,
                config_timestamp: x::CURRENT_TIME,
            })
            .await?;
        trace!("crtc_info: {:?}", crtc_info);

        Ok(Size::new(
            crtc_info.width() as usize,
            crtc_info.height() as usize,
        ))
    }
}

impl X {
    pub const DEFAULT_MAX_WIDTH: usize = 3840;
    pub const DEFAULT_MAX_HEIGHT: usize = 2160;
    pub const MIN_WIDTH: usize = 800;
    pub const MIN_HEIGHT: usize = 600;

    async fn screen_resources(&self) -> Result<randr::GetScreenResourcesReply> {
        self.connection
            .checked_request_with_reply(randr::GetScreenResources {
                window: self.root_window()?,
            })
            .await
    }

    fn root_window(&self) -> Result<x::Window> {
        match self.root_drawable {
            Drawable::Window(window) => Ok(window),
            _ => bail!("non-window root drawable"),
        }
    }

    async fn query_damage_version(connection: &XConnection) -> Result<()> {
        trace!("querying damage version");
        let reply = connection
            .checked_request_with_reply(damage::QueryVersion {
                client_major_version: damage::MAJOR_VERSION,
                client_minor_version: damage::MINOR_VERSION,
            })
            .await?;

        trace!("received reply");
        if reply.major_version() != damage::MAJOR_VERSION
            || reply.minor_version() != damage::MINOR_VERSION
        {
            bail!(
                "X Server Damage {}.{} != X Client Damage {}.{}",
                reply.major_version(),
                reply.minor_version(),
                damage::MAJOR_VERSION,
                damage::MINOR_VERSION
            );
        }

        Ok(())
    }

    async fn query_xfixes_version(connection: &XConnection) -> Result<()> {
        trace!("querying xfixes version");
        let reply = connection
            .checked_request_with_reply(xfixes::QueryVersion {
                client_major_version: xfixes::MAJOR_VERSION,
                client_minor_version: xfixes::MINOR_VERSION,
            })
            .await?;

        if reply.major_version() != xfixes::MAJOR_VERSION
            || reply.minor_version() != xfixes::MINOR_VERSION
        {
            bail!(
                "X Server XFixes {}.{} != X Client XFixes {}.{}",
                reply.major_version(),
                reply.minor_version(),
                xfixes::MAJOR_VERSION,
                xfixes::MINOR_VERSION,
            );
        }

        Ok(())
    }

    fn create_root_objects(connection: &XConnection) -> (Damage, Window) {
        let root_damage = connection.inner.generate_id();
        let root_window = connection.preferred_screen.root();
        (root_damage, root_window)
    }

    async fn setup_damage_monitor(
        connection: &XConnection,
        damage: &Damage,
        drawable: &Drawable,
    ) -> Result<()> {
        connection
            .checked_void_request(damage::Create {
                damage: *damage,
                drawable: *drawable,
                level: damage::ReportLevel::RawRectangles,
            })
            .await
    }

    async fn setup_xtest_extension(connection: &XConnection) -> Result<()> {
        let reply = connection
            .checked_request_with_reply(xtest::GetVersion {
                major_version: xtest::MAJOR_VERSION as u8,
                minor_version: xtest::MINOR_VERSION as u16,
            })
            .await?;

        if reply.major_version() != xtest::MAJOR_VERSION as u8
            || reply.minor_version() != xtest::MINOR_VERSION as u16
        {
            bail!(
                "X Server XTest {}.{} != X Client XTest {}.{}",
                reply.major_version(),
                reply.minor_version(),
                xtest::MAJOR_VERSION,
                xtest::MINOR_VERSION
            );
        }

        connection
            .checked_void_request(xtest::GrabControl { impervious: true })
            .await?;

        Ok(())
    }

    async fn setup_xfixes_monitor(connection: &XConnection, window: &Window) -> Result<()> {
        connection
            .checked_void_request(xfixes::SelectCursorInput {
                window: *window,
                event_mask: xfixes::CursorNotifyMask::all(),
            })
            .await
    }
}

struct XEventStreamMux {
    txs: Arc<Mutex<Vec<mpsc::UnboundedSender<Arc<XEvent>>>>>,
    _task: JoinHandle<Result<()>>,
}

impl XEventStreamMux {
    fn new(conn: Arc<xcb::Connection>) -> XEventStreamMux {
        let conn = conn.clone();
        let txs: Arc<Mutex<_>> = Arc::default();
        let tx_task_txs = txs.clone();

        XEventStreamMux {
            txs,
            _task: tokio::task::spawn(async move {
                loop {
                    let event = loop {
                        if let Some(event) = conn.poll_for_event()? {
                            break event;
                        }
                        time::sleep(Duration::from_micros(1)).await;
                    };
                    BackendEvent::inc();
                    let arcd = Arc::new(XEvent(event));

                    let mut txs = tx_task_txs.lock().expect("lock X mux txs");
                    txs.retain(|tx| tx.send(arcd.clone()).is_ok());
                }
            }),
        }
    }

    fn get_stream(&self) -> impl Stream<Item = Arc<XEvent>> + Send + Sync + Unpin {
        let mut txs = self.txs.lock().expect("lock X mux txs");
        let (tx, rx) = mpsc::unbounded_channel();
        txs.push(tx);
        UnboundedReceiverStream::new(rx)
    }
}

struct XConnection {
    inner: Arc<Connection>,
    preferred_screen: ScreenBuf,
    min_keycode: x::Keycode,
    max_keycode: x::Keycode,
    event_stream_mux: XEventStreamMux,
}

impl Debug for XConnection {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::result::Result<(), fmt::Error> {
        f.debug_struct("XConnection")
            .field("inner", &self.inner.get_raw_conn())
            .field("preferred_screen", &self.preferred_screen)
            .field("min_keycode", &self.min_keycode)
            .field("max_keycode", &self.max_keycode)
            .finish()
    }
}

impl XConnection {
    fn event_stream(&self) -> impl Stream<Item = Arc<XEvent>> + Send + Sync + Unpin {
        self.event_stream_mux.get_stream()
    }

    async fn checked_request_with_reply<R>(
        &self,
        req: R,
    ) -> Result<<<R as Request>::Cookie as CookieWithReplyChecked>::Reply>
    where
        R: RequestWithReply + Debug,
        <R as xcb::Request>::Cookie: CookieWithReplyChecked,
    {
        let cookie = self.inner.send_request(&req);
        self.inner.flush()?;
        loop {
            if let Some(response) = self.inner.poll_for_reply(&cookie).transpose()? {
                return Ok(response);
            }
            time::sleep(Duration::from_micros(1)).await;
        }
    }

    async fn checked_void_request<R>(&self, req: R) -> Result<()>
    where
        R: RequestWithoutReply + Debug,
    {
        let inner = self.inner.clone();
        Ok(tokio::task::block_in_place(move || {
            let reply = inner.send_and_check_request(&req);
            if let Err(e) = &reply {
                error!("X returned an error for request {:?}: {}", req, e);
            }
            reply
        })?)
    }

    /// Create a new X with an optional name of the server to connect to.  If the `name` is `None`,
    /// this will use the `DISPLAY` environment variable.
    fn new<'s>(name: impl Into<Option<&'s str>>) -> Result<Self> {
        let name = name.into();
        trace!("Connecting to display {:?}", name);
        let (conn, preferred_screen_index) = Connection::connect_with_extensions(
            name,
            &[Extension::Damage, Extension::Test, Extension::XFixes],
            &[],
        )?;
        trace!("Created X connection");
        let conn_arc = Arc::new(conn);
        let preferred_screen = conn_arc
            .get_setup()
            .roots()
            .nth(preferred_screen_index as usize)
            .ok_or(anyhow!("No preferred screen"))?
            .to_owned();
        let min_keycode = conn_arc.get_setup().min_keycode();
        let max_keycode = conn_arc.get_setup().max_keycode();

        Ok(XConnection {
            inner: conn_arc.clone(),
            preferred_screen,
            min_keycode,
            max_keycode,
            event_stream_mux: XEventStreamMux::new(conn_arc),
        })
    }
}

#[derive(Debug)]
pub struct XEvent(xcb::Event);

#[cfg(test)]
mod test {
    use super::*;

    use serial_test::serial;
    use test_log::test;

    #[test(tokio::test(flavor = "multi_thread"))]
    #[serial]
    async fn retrieve_damage_from_stream() {
        const NUM_UPDATES: usize = 20;
        let x = X::initialize(1920, 1080, Some(&mut Command::new("glxgears")))
            .await
            .expect("x initialize");
        let damage_stream = x.damage_stream().await.expect("damage stream");
        let mut first = damage_stream.take(NUM_UPDATES);
        let mut count: usize = 0;

        while let Some(_damage) = first.next().await {
            count += 1;
        }

        assert_eq!(count, NUM_UPDATES);
    }

    #[test(tokio::test(flavor = "multi_thread"))]
    #[serial]
    async fn get_resolution() {
        let x = X::initialize(1920, 1080, Some(&mut Command::new("glxgears")))
            .await
            .expect("x initialize");
        let res = x.resolution().await.expect("resolution");
        assert_eq!(res.width, 1920);
        assert_eq!(res.height, 1080);
    }

    #[test(tokio::test(flavor = "multi_thread"))]
    #[serial]
    async fn set_resolution() {
        let mut x = X::initialize(1920, 1080, Some(&mut Command::new("glxgears")))
            .await
            .expect("x initialize");

        let resolutions = vec![
            Size::new(1024, 768),
            Size::new(800, 600),
            Size::new(1440, 900),
            Size::new(1280, 1024),
            Size::new(823, 688),
            Size::new(800, 600),
        ];

        for new in resolutions {
            x.set_resolution(&new).await.expect("set resolution");
            assert_eq!(new, x.resolution().await.expect("get resolution"));
            let res = x.resolution().await.expect("resolution");
            assert_eq!(res.width, new.width);
            assert_eq!(res.height, new.height);
        }

        // Ensure only the initial mode and the current mode still exist
        assert_eq!(
            x.screen_resources()
                .await
                .expect("screen resources")
                .modes()
                .len(),
            2
        );
    }
}
