use crate::backend::{Backend, DamageStream, Event, FramebufferUpdate};
use crate::task::encoder::*;

use anyhow::{anyhow, bail, Result};
use aperturec_protocol::common::*;
use aperturec_protocol::event as em;
use aperturec_trace::log;
use aperturec_trace::queue::{self, deq, enq, trace_queue};
use async_trait::async_trait;
use futures::{future, stream, Stream, StreamExt};
use ndarray::{Array2, ShapeBuilder};
use rand::distributions::{Alphanumeric, DistString};
use shlex::Shlex;
use std::fmt::{self, Debug, Formatter};
use std::iter::Iterator;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio::time;
use tokio_stream::wrappers::BroadcastStream;
use xcb::damage::{self, Damage};
use xcb::x::{self, Drawable, GetImage, ImageFormat, ScreenBuf};
use xcb::{randr, xtest, BaseEvent, Connection, Extension};
use xcb::{CookieWithReplyChecked, Request, RequestWithReply, RequestWithoutReply};

const X_DAMAGE_QUEUE: queue::Queue = trace_queue!("x:damage");

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
    root_drawable: Drawable,
    _xvfb_proc: Child,
    _initial_program_proc: Child,
}

#[async_trait]
impl Backend for X {
    async fn capture_area(&self, area: Rect) -> Result<FramebufferUpdate> {
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
        let pixels: Vec<RawPixel> = get_image_reply
            .data()
            .chunks(RAW_BYTES_PER_PIXEL)
            .map(|chunk| RawPixel {
                blue: chunk[0],
                green: chunk[1],
                red: chunk[2],
                alpha: chunk[3],
            })
            .collect();
        Ok(FramebufferUpdate {
            rect: area,
            data: Array2::from_shape_vec((area.size.width, area.size.height).f(), pixels)?,
        })
    }

    async fn damage_stream(&self) -> Result<DamageStream> {
        let size = self.resolution().await?;
        let rect = Rect {
            origin: Point::new(0, 0),
            size,
        };
        Ok(stream::iter([rect])
            .chain(
                self.connection
                    .event_stream()?
                    .filter_map(|event| match &event.0 {
                        xcb::Event::Damage(damage::Event::Notify(notify_event)) => {
                            future::ready(Some(notify_event.area()))
                        }
                        other_event => {
                            log::warn!("Some unexpected event ignored: {:#?}", other_event);
                            future::ready(None)
                        }
                    })
                    .map(|xcb_rect| Rect {
                        origin: Point::new(xcb_rect.x as usize, xcb_rect.y as usize),
                        size: Size::new(xcb_rect.width as usize, xcb_rect.height as usize),
                    }),
            )
            .boxed())
    }

    async fn initialize<N, S>(initial_program: S, max_width: N, max_height: N) -> Result<Self>
    where
        N: Into<Option<usize>> + Send,
        S: AsRef<str> + Send,
    {
        const CHILD_STARTUP_WAIT_TIME_MS: Duration = Duration::from_millis(1000);
        let max_width = max_width.into().unwrap_or(Self::DEFAULT_MAX_WIDTH);
        let max_height = max_height.into().unwrap_or(Self::DEFAULT_MAX_HEIGHT);

        log::debug!(
            "Starting X Server with max resolution {}x{}",
            max_width,
            max_height
        );

        let mut xvfb_cmd = Command::new("Xvfb");
        xvfb_cmd
            .arg("-screen")
            .arg("0")
            .arg(format!("{}x{}x{}", max_width, max_height, 24))
            .arg("-displayfd")
            .arg("1") // stdout
            .stdout(Stdio::piped())
            .kill_on_drop(true);
        log::debug!("Starting Xvfb with command `{:?}`", xvfb_cmd);
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
        log::trace!("Started X server with name \"{}\"", display_name);

        let mut initial_program_args = Shlex::new(initial_program.as_ref());
        let initial_program_exe = initial_program_args
            .next()
            .ok_or(anyhow!("Empty initial program provided"))?;
        let mut initial_program_cmd = Command::new::<&str>(initial_program_exe.as_ref());
        initial_program_cmd
            .args(initial_program_args)
            .env("DISPLAY", &display_name)
            .env("XDG_SESSION_TYPE", "x11")
            .kill_on_drop(true);
        log::debug!(
            "Starting {} with command `{:?}`",
            initial_program.as_ref(),
            initial_program_cmd
        );
        let mut initial_program_proc = initial_program_cmd.spawn()?;
        time::sleep(CHILD_STARTUP_WAIT_TIME_MS).await;
        if let Ok(Some(status)) = initial_program_proc.try_wait() {
            bail!("{} exited with status {}", initial_program.as_ref(), status);
        }
        log::trace!("Launched {}", initial_program.as_ref());

        log::trace!("Creating new XConnection");
        let connection = XConnection::new(&*display_name)?;
        log::trace!("Querying for damage extension version");
        X::query_damage_version(&connection).await?;

        log::trace!("Creating root objects");
        let (root_damage, root_drawable) = X::create_root_objects(&connection);
        log::trace!("Setting up damage monitor");
        X::setup_damage_monitor(&connection, &root_damage, &root_drawable).await?;
        log::trace!("Setting up xtest extension");
        X::setup_xtest_extension(&connection).await?;

        log::trace!("X initialized");
        Ok(X {
            display_name,
            max_width,
            max_height,
            connection,
            root_drawable,
            _xvfb_proc: xvfb_proc,
            _initial_program_proc: initial_program_proc,
        })
    }

    async fn notify_event(&mut self, event: Event) -> Result<()> {
        match event {
            Event::Key { key, is_depressed } => {
                let keycode = key as x::Keycode;
                if keycode > self.connection.max_keycode || keycode < self.connection.min_keycode {
                    log::warn!(
                        "Received key event for key outside range ({},{}): {}. Dropping the event.",
                        self.connection.min_keycode,
                        self.connection.max_keycode,
                        keycode
                    );
                    return Ok(());
                }
                let event_type_num = if is_depressed {
                    x::KeyPressEvent::NUMBER
                } else {
                    // hard-coded for same reasons as pointer event
                    3
                } as u8;
                log::info!("key_event.key: {}", key);
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
                    log::debug!("event_type_num: {}", event_type_num);
                    let req = xtest::FakeInput {
                        r#type: event_type_num,
                        detail: u8_for_button(&button) + 1,
                        time: x::CURRENT_TIME,
                        root: self.root_window()?,
                        root_x: location.x_position as i16,
                        root_y: location.y_position as i16,
                        deviceid: 0,
                    };

                    log::trace!("Sending event {:#?}", req);
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
                log::trace!("Sending event {:#?}", req);
                self.connection.checked_void_request(req).await?;
            }
            Event::Display { size } => {
                let size = Size::new(size.width as usize, size.height as usize);
                self.set_resolution(&size).await?;
            }
        }

        Ok(())
    }

    async fn cursor_bitmaps(&self) -> Result<Vec<CursorBitmap>> {
        // TODO actually implement
        Ok(vec![])
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

        log::trace!("{:#?}", screen_resources);
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
        Ok(())
    }

    async fn resolution(&self) -> Result<Size> {
        let screen_resources = self.screen_resources().await?;
        log::trace!("crtcs: {:#?}", screen_resources.crtcs());
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
        log::trace!("crtc_info: {:?}", crtc_info);

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
        log::trace!("querying damage version");
        let reply = connection
            .checked_request_with_reply(damage::QueryVersion {
                client_major_version: damage::MAJOR_VERSION,
                client_minor_version: damage::MINOR_VERSION,
            })
            .await?;

        log::trace!("received reply");
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

    fn create_root_objects(connection: &XConnection) -> (Damage, Drawable) {
        let root_damage = connection.inner.generate_id();
        let root_drawable = Drawable::Window(connection.preferred_screen.root());
        (root_damage, root_drawable)
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
}

struct XConnection {
    inner: Arc<Connection>,
    preferred_screen: ScreenBuf,
    min_keycode: x::Keycode,
    max_keycode: x::Keycode,
    _event_task: JoinHandle<Result<()>>,
    event_broadcast_rx: broadcast::Receiver<Arc<XEvent>>,
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

type XEventStream = Pin<Box<dyn Stream<Item = Arc<XEvent>> + Send>>;

impl XConnection {
    fn event_stream(&self) -> Result<XEventStream> {
        Ok(BroadcastStream::new(self.event_broadcast_rx.resubscribe())
            .filter_map(|res| match res {
                Ok(event) => future::ready(Some(event)),
                Err(bcast_err) => {
                    log::warn!("event bcast error: {}", bcast_err);
                    future::ready(None)
                }
            })
            .boxed())
    }

    async fn checked_request_with_reply<R>(
        &self,
        req: R,
    ) -> Result<<<R as Request>::Cookie as CookieWithReplyChecked>::Reply>
    where
        R: RequestWithReply + Debug,
        <R as xcb::Request>::Cookie: CookieWithReplyChecked,
    {
        let inner = self.inner.clone();
        let response = tokio::task::block_in_place(move || {
            let cookie = inner.send_request(&req);
            let reply = inner.wait_for_reply(cookie);
            if let Err(e) = &reply {
                log::error!("X returned an error for request {:?}: {}", req, e);
            }
            reply
        })?;

        Ok(response)
    }

    async fn checked_void_request<R>(&self, req: R) -> Result<()>
    where
        R: RequestWithoutReply + Debug,
    {
        let inner = self.inner.clone();
        Ok(tokio::task::block_in_place(move || {
            let reply = inner.send_and_check_request(&req);
            if let Err(e) = &reply {
                log::error!("X returned an error for request {:?}: {}", req, e);
            }
            reply
        })?)
    }

    /// Create a new X with an optional name of the server to connect to.  If the `name` is `None`,
    /// this will use the `DISPLAY` environment variable.
    fn new<'s>(name: impl Into<Option<&'s str>>) -> Result<Self> {
        let name = name.into();
        log::trace!("Connecting to display {:?}", name);
        let (conn, preferred_screen_index) =
            Connection::connect_with_extensions(name, &[Extension::Damage, Extension::Test], &[])?;
        log::trace!("Created X connection");
        let conn_arc = Arc::new(conn);
        let preferred_screen = conn_arc
            .get_setup()
            .roots()
            .nth(preferred_screen_index as usize)
            .ok_or(anyhow!("No preferred screen"))?
            .to_owned();
        let min_keycode = conn_arc.get_setup().min_keycode();
        let max_keycode = conn_arc.get_setup().max_keycode();
        let (event_broadcast_tx, event_broadcast_rx) = broadcast::channel(256);
        let event_task_inner = conn_arc.clone();

        Ok(XConnection {
            inner: conn_arc,
            preferred_screen,
            min_keycode,
            max_keycode,
            _event_task: tokio::task::spawn_blocking(move || loop {
                let event = event_task_inner.wait_for_event()?;
                event_broadcast_tx.send(Arc::new(XEvent(event)))?;
                enq!(X_DAMAGE_QUEUE);
            }),
            event_broadcast_rx,
        })
    }
}

#[derive(Debug)]
pub struct XEvent(xcb::Event);

impl Drop for XEvent {
    fn drop(&mut self) {
        deq!(X_DAMAGE_QUEUE);
    }
}

#[cfg(test)]
mod test {
    use super::*;

    use serial_test::serial;
    use std::sync::Once;

    static INIT: Once = Once::new();

    fn setup() {
        INIT.call_once(|| {
            aperturec_trace::Configuration::new("test")
                .initialize()
                .expect("trace init");
        });
    }

    #[tokio::test(flavor = "multi_thread")]
    #[serial]
    async fn retrieve_damage_from_stream() {
        const NUM_UPDATES: usize = 20;
        let x = X::initialize("glxgears", 1920, 1080)
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

    #[tokio::test(flavor = "multi_thread")]
    #[serial]
    async fn get_resolution() {
        setup();
        let x = X::initialize("glxgears", 1920, 1080)
            .await
            .expect("x initialize");
        let res = x.resolution().await.expect("resolution");
        assert_eq!(res.width, 1920);
        assert_eq!(res.height, 1080);
    }

    #[tokio::test(flavor = "multi_thread")]
    #[serial]
    async fn set_resolution() {
        let mut x = X::initialize("glxgears", 1920, 1080)
            .await
            .expect("x initialize");
        let new = Size::new(1024, 768);
        x.set_resolution(&new).await.expect("set resolution");
        assert_eq!(new, x.resolution().await.expect("get resolution"));
        let res = x.resolution().await.expect("resolution");
        assert_eq!(res.width, 1024);
        assert_eq!(res.height, 768);
    }
}
