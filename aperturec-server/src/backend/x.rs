use crate::backend::{Backend, CursorChange, CursorImage, Event, LockState, SetDisplaysSuccess};
use crate::metrics::{BackendEvent, DisplayHeight, DisplayWidth};
use crate::process_utils::{self, DisplayableExitStatus};

use aperturec_graphics::{display::*, geometry::*, prelude::*};
use aperturec_protocol::event as em;

use anyhow::{Result, anyhow, bail, ensure};
use futures::{Future, Stream, StreamExt, TryFutureExt, future, stream};
use ndarray::prelude::*;
use scan_fmt::scan_fmt;
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::env;
use std::fmt::{self, Debug, Formatter};
use std::iter::Iterator;
use std::mem;
use std::pin::Pin;
use std::process::{ExitStatus, Stdio};
use std::ptr::{self, NonNull};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader, unix::AsyncFd};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tracing::*;
use xcb::{
    BaseEvent, CookieWithReplyChecked, Request, RequestWithReply, RequestWithoutReply, Xid, XidNew,
    composite, damage, randr, x, xfixes, xtest,
};
use zerocopy::byteorder::{BE, U16, U32};

const CHILD_STARTUP_WAIT_TIME_MS: Duration = Duration::from_millis(1000);

const XPIXMAP_DEPTH: u32 = 24;
const XWD_FILE_VERSION: u32 = 7;
pub const X_BYTES_PER_PIXEL: usize = 4;

fn u8_for_button(button: &em::Button) -> u8 {
    match button.kind {
        Some(em::button::Kind::MappedButton(idx)) => idx as u8 - 1,
        Some(em::button::Kind::UnmappedButton(idx)) => idx as u8,
        None => panic!("Button with no interior mapping"),
    }
}

#[derive(Debug)]
struct XDisplay {
    crtc: randr::Crtc,
    output: randr::Output,
}

#[derive(Debug)]
pub struct X {
    pub display_name: String,
    connection: XConnection,
    default_mode_id: u32,
    displays: BTreeMap<usize, XDisplay>,
    max_display_count: usize,
    max_height: usize,
    max_width: usize,
    framebuffer: Arc<XFrameBuffer>,
    root_window: x::Window,
    root_process: Option<Child>,
    xvfb_process: Child,
}

async fn do_exec_command(display_name: &str, command: &mut Command) -> Result<Child> {
    command.env("DISPLAY", display_name);
    command.env("XDG_SESSION_TYPE", "x11");
    command.stdout(process_utils::StdoutTracer::new(Level::TRACE));
    command.stderr(process_utils::StderrTracer::new(Level::TRACE));
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

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct XWDFileHeader {
    header_size: U32<BE>,
    file_version: U32<BE>,
    pixmap_format: U32<BE>,
    pixmap_depth: U32<BE>,
    pixmap_width: U32<BE>,
    pixmap_height: U32<BE>,
    xoffset: U32<BE>,
    byte_order: U32<BE>,
    bitmap_unit: U32<BE>,
    bitmap_bit_order: U32<BE>,
    bitmap_pad: U32<BE>,
    bits_per_pixel: U32<BE>,
    bytes_per_line: U32<BE>,
    visual_class: U32<BE>,
    red_mask: U32<BE>,
    green_mask: U32<BE>,
    blue_mask: U32<BE>,
    bits_per_rgb: U32<BE>,
    colormap_entries: U32<BE>,
    ncolors: U32<BE>,
    window_width: U32<BE>,
    window_height: U32<BE>,
    window_x: U32<BE>,
    window_y: U32<BE>,
    window_bdrwidth: U32<BE>,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct XWDColor {
    pixel: U32<BE>,
    red: U16<BE>,
    green: U16<BE>,
    blue: U16<BE>,
    flags: u8,
    pad: u8,
}

#[derive(Debug)]
struct XFrameBuffer {
    shmaddr: NonNull<u8>,
}

/// SAFETY: XFrameBuffer owns a raw pointer to a shared memory segment that is mapped read-only.
/// The underlying memory region is never mutated by XFrameBuffer, so concurrent access from multiple
/// threads is safe. Detaching the segment in Drop is also thread-safe.
/// No interior mutability or synchronization primitives are required.
unsafe impl Send for XFrameBuffer {}
unsafe impl Sync for XFrameBuffer {}

impl XFrameBuffer {
    fn header(&self) -> &XWDFileHeader {
        // SAFETY: The shared memory segment begins with a valid XWDFileHeader at its start.
        unsafe { &*(self.shmaddr.as_ptr() as *const XWDFileHeader) }
    }

    fn color_offset(&self) -> usize {
        self.header().header_size.get() as usize
            + self.header().ncolors.get() as usize * mem::size_of::<XWDColor>()
    }

    fn new(shmid: i32) -> Result<Self> {
        // SAFETY: Attaching to the shared memory segment shmid returns a valid pointer or error.
        let shmaddr = unsafe { libc::shmat(shmid, ptr::null_mut(), libc::SHM_RDONLY) };
        if shmaddr == (-1isize) as *mut libc::c_void {
            bail!("shmat failed");
        }
        let shmaddr = NonNull::new(shmaddr).ok_or(anyhow!("null shmaddr"))?;

        let mut ds = mem::MaybeUninit::zeroed();
        // SAFETY: shmctl with IPC_STAT initializes the ds structure pointed to by ds.as_mut_ptr().
        if unsafe { libc::shmctl(shmid, libc::IPC_STAT, ds.as_mut_ptr()) } != 0 {
            // SAFETY: Detach the shared memory on error cleanup; shmaddr is from successful shmat.
            unsafe { libc::shmdt(shmaddr.as_ptr() as *const _) };
            bail!("shmctl IPC_STAT failed");
        }
        // SAFETY: ds has been initialized by the successful shmctl call above.
        let ds = unsafe { ds.assume_init() };

        // SAFETY: The shared memory region mapped by shmaddr is at least header_size bytes long.
        let header = unsafe { &*(shmaddr.as_ptr() as *const XWDFileHeader) };
        debug!(xwd_header=?header);
        ensure!(
            header.file_version == XWD_FILE_VERSION,
            "unknown XWD file version: {}",
            header.file_version
        );
        ensure!(
            ds.shm_segsz as u32
                >= (header.header_size + header.window_height * header.bytes_per_line).get(),
            "shmem segment too small"
        );
        ensure!(
            header.pixmap_depth == XPIXMAP_DEPTH,
            "invalid X pixmap depth: {}",
            header.pixmap_depth,
        );
        ensure!(
            header.bits_per_pixel == (X_BYTES_PER_PIXEL * 8) as u32,
            "invalid bits per pixel: {}",
            header.bits_per_pixel
        );

        Ok(XFrameBuffer {
            // SAFETY: shmaddr is non-null and correctly aligned for u8 pointer, safe to wrap.
            shmaddr: shmaddr.cast(),
        })
    }
}

impl Drop for XFrameBuffer {
    fn drop(&mut self) {
        // SAFETY: Detaching the shared memory pointer previously attached with shmat.
        unsafe {
            libc::shmdt(self.shmaddr.as_ptr() as *const _);
        }
    }
}

/// B-G-R-A format
pub type XPixel = Pixel32;

pub struct XPixelMap {
    framebuffer: Arc<XFrameBuffer>,
    area: Rect,
}

impl XPixelMap {
    fn new(framebuffer: Arc<XFrameBuffer>, area: Rect) -> Self {
        XPixelMap { framebuffer, area }
    }
}

impl PixelMap for XPixelMap {
    type Pixel = XPixel;

    fn as_ndarray(&self) -> ArrayView2<Self::Pixel> {
        let xwd_header = self.framebuffer.header();
        let pixel_offset = self.framebuffer.color_offset()
            + self.area.origin.y * xwd_header.bytes_per_line.get() as usize
            + self.area.origin.x * mem::size_of::<XPixel>();
        let base = self.framebuffer.shmaddr.as_ptr();
        // SAFETY: `base` is valid for the entire framebuffer, and `pixel_offset` is in bounds
        let pixel_ptr = unsafe { base.add(pixel_offset) as *const XPixel };
        let row_stride = xwd_header.bytes_per_line.get() as usize / mem::size_of::<XPixel>();
        // SAFETY: The shape and strides correspond exactly to a contiguous region of XPixel values.
        unsafe {
            ArrayView2::from_shape_ptr(
                (self.area.size.height, self.area.size.width).strides((row_stride, 1)),
                pixel_ptr,
            )
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

impl Backend for X {
    type PixelMap = XPixelMap;

    async fn wait_root_process(&mut self) -> Result<ExitStatus> {
        static WARN_ONCE: OnceLock<()> = OnceLock::new();
        static INFO_ONCE: OnceLock<()> = OnceLock::new();
        let root_proc_fut: Pin<Box<dyn Future<Output = _> + Send + Sync>> =
            if let Some(child) = self.root_process.as_mut() {
                Box::pin(child.wait())
            } else {
                Box::pin(future::pending())
            };

        tokio::select! {
            biased;
            xvfb_es_res = self.xvfb_process.wait() => {
                let xvfb_es = xvfb_es_res?;
                WARN_ONCE.get_or_init(|| warn!(exit_status=%xvfb_es.display(), "Xvfb exiting"));
                Ok(xvfb_es)
            },
            root_es_res = root_proc_fut => {
                let root_es = root_es_res?;
                if root_es.success() {
                    INFO_ONCE.get_or_init(|| {
                        info!(exit_status = %root_es.display(), "Root process exited cleanly");
                    });
                } else {
                    WARN_ONCE.get_or_init(|| warn!(exit_status = %root_es.display(), "Root process exiting"));
                }
                Ok(root_es)
            }
        }
    }

    async fn exec_command(&mut self, command: &mut Command) -> Result<Child> {
        do_exec_command(&self.display_name, command).await
    }

    async fn capture_area(&self, area: Rect) -> Result<Self::PixelMap> {
        Ok(XPixelMap::new(self.framebuffer.clone(), area))
    }

    async fn damage_stream(&self) -> Result<impl Stream<Item = Rect> + Send + Unpin + 'static> {
        let size = Size::new(self.max_width, self.max_height);
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
        max_display_count: N,
        root_process_cmd: Option<&mut Command>,
    ) -> Result<Self>
    where
        N: Into<Option<usize>> + Send + Sync,
    {
        let max_width = max_width.into().unwrap_or(Self::DEFAULT_MAX_WIDTH);
        let max_height = max_height.into().unwrap_or(Self::DEFAULT_MAX_HEIGHT);
        let mut max_display_count = max_display_count
            .into()
            .unwrap_or(Self::DEFAULT_MAX_DISPLAY_COUNT);

        debug!(
            "Starting X Server with screen size {}x{}, max displays {}",
            max_width, max_height, max_display_count
        );

        let xvfb_path = env::var("XVFB").unwrap_or_else(|_| "Xvfb".to_string());
        if let Err(error) = which::which(&xvfb_path) {
            bail!("Xvfb executable not found: {}", error);
        }

        let xvfb_help = Command::new(&xvfb_path).arg("-help").output().await?.stderr;

        //
        // Separate handling for "-crtcs n" and "-crtcs n@WxH" can be removed once the later is
        // merged upstream in xserver
        //
        let xvfb_has_crtc_size = String::from_utf8_lossy(&xvfb_help).contains("-crtcs n[@WxH]");
        if !xvfb_has_crtc_size && !String::from_utf8_lossy(&xvfb_help).contains("-crtcs n") {
            warn!(
                "{} does not support the -crtcs option. Multi-display is disabled",
                xvfb_path
            );

            max_display_count = 1;
        }

        let mut xvfb_cmd = Command::new(xvfb_path);
        xvfb_cmd
            .arg("-screen")
            .arg("0")
            .arg(format!("{max_width}x{max_height}x{XPIXMAP_DEPTH}"))
            .arg("-displayfd")
            .arg("1") // stdout
            .arg("-shmem")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        if xvfb_has_crtc_size {
            xvfb_cmd.arg("-crtcs").arg(format!(
                "{}@{}x{}",
                max_display_count,
                X::MIN_WIDTH,
                X::MIN_HEIGHT
            ));
        } else if max_display_count > 1 {
            xvfb_cmd.arg("-crtcs").arg(max_display_count.to_string());
        }

        debug!("Starting Xvfb with command `{:?}`", xvfb_cmd);
        let mut xvfb_process = xvfb_cmd.spawn()?;
        time::sleep(CHILD_STARTUP_WAIT_TIME_MS).await;
        if let Ok(Some(status)) = xvfb_process.try_wait() {
            bail!("Xvfb exited with status {}", status.display());
        }
        let mut xvfb_stdout = BufReader::new(
            xvfb_process
                .stdout
                .as_mut()
                .ok_or(anyhow!("No Xvfb stdout"))?,
        );
        let mut display_name = String::new();
        xvfb_stdout.read_line(&mut display_name).await?;
        display_name = format!(":{}", display_name.trim().trim_end());

        let mut xvfb_stderr = BufReader::new(
            xvfb_process
                .stderr
                .as_mut()
                .ok_or(anyhow!("No Xvfb stderr"))?,
        );
        let shmem_id = loop {
            let mut line = String::new();
            xvfb_stderr
                .read_line(&mut line)
                .await
                .map_err(|e| anyhow!("Failed reading Xvfb stderr: {}", e))?;
            if let Ok((_, id)) = scan_fmt!(line.trim(), "screen {d} shmid {d}", usize, usize) {
                break id;
            }
        };
        trace!(server = %display_name, %shmem_id, "Started X server");

        trace!("Creating new XConnection");
        let connection = XConnection::new(&*display_name)?;
        trace!("Querying for composite extension version");
        X::query_composite_version(&connection).await?;
        trace!("Querying for damage extension version");
        X::query_damage_version(&connection).await?;
        trace!("Querying for xfixes extension version");
        X::query_xfixes_version(&connection).await?;
        trace!("Querying for randr extension version");
        X::query_randr_version(&connection).await?;
        let root_window = connection.preferred_screen.root();
        trace!("Setting up damage monitor");
        X::setup_damage_monitor(&connection, &root_window).await?;
        trace!("Setting up xtest extension");
        X::setup_xtest_extension(&connection).await?;
        trace!("Setting up xfixes monitor");
        X::setup_xfixes_monitor(&connection, &root_window).await?;
        trace!("Hiding cursor");
        X::hide_cursor(&connection, &root_window).await?;

        let root_process = match root_process_cmd {
            None => {
                println!("{}", &display_name);
                None
            }
            Some(cmd) => Some(do_exec_command(&display_name, cmd).await?),
        };

        let (displays, default_mode_id) = X::setup_displays(&connection, &root_window).await?;

        let framebuffer = Arc::new(XFrameBuffer::new(shmem_id as i32)?);

        let mut x = X {
            connection,
            default_mode_id,
            display_name,
            displays,
            max_display_count,
            max_height,
            max_width,
            framebuffer,
            root_window,
            root_process,
            xvfb_process,
        };

        if !xvfb_has_crtc_size {
            x.set_displays(vec![Display::new(
                Rect::from_size(Size::new(X::MIN_WIDTH, X::MIN_HEIGHT)),
                true,
            )])
            .await
            .map_err(|_| anyhow!("Failed to set minimal Displays"))?;
        }

        trace!("X initialized");
        Ok(x)
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
                let req = xtest::FakeInput {
                    r#type: event_type_num,
                    detail: key as u8,
                    time: x::CURRENT_TIME,
                    root: self.root_window,
                    root_x: 0,
                    root_y: 0,
                    deviceid: 0,
                };
                trace!(key_event=?req);
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
                    let req = xtest::FakeInput {
                        r#type: event_type_num,
                        detail: u8_for_button(&button) + 1,
                        time: x::CURRENT_TIME,
                        root: self.root_window,
                        root_x: location.x_position as i16,
                        root_y: location.y_position as i16,
                        deviceid: 0,
                    };

                    trace!(mouse_event=?req);
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
                    root: self.root_window,
                    root_x: location.x_position as i16,
                    root_y: location.y_position as i16,
                    deviceid: 0,
                };
                trace!(motion_event=?req);
                self.connection.checked_void_request(req).await?;
            }
            Event::Display { displays: _ } => {
                // Display Events are handled in backend.rs directly and not sent via notify_event
                unreachable!("Unexpected Display Event");
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
                window: self.root_window,
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
                        root: self.root_window,
                        root_x: 0,
                        root_y: 0,
                        deviceid: 0,
                    };
                    self.connection.checked_void_request(req).await?;

                    let req = xtest::FakeInput {
                        r#type: 3_u8,
                        detail: kc,
                        time: x::CURRENT_TIME,
                        root: self.root_window,
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
                    window: self.root_window,
                })
                .await?
                .mask()
        );

        Ok(())
    }

    async fn set_displays(
        &mut self,
        mut requested_displays: Vec<Display>,
    ) -> Result<SetDisplaysSuccess, Vec<Display>> {
        let mut errors = false;
        let mut changed = false;

        if requested_displays.len() != self.max_display_count {
            if requested_displays.len() > self.max_display_count {
                warn!(
                    "Received {} display requests, truncating to {}",
                    requested_displays.len(),
                    self.max_display_count
                );
            } else {
                debug!(
                    "Received {} display requests, expanding to {}",
                    requested_displays.len(),
                    self.max_display_count
                );
            }
        }

        requested_displays.resize_with(self.max_display_count, Display::default);

        if let Err(e) = self
            .resize_screen_if(&requested_displays, Ordering::Greater)
            .await
        {
            warn!("Failed to expand screen size: {:?}", e);
        }

        let mut final_displays: Vec<Display> = Vec::with_capacity(requested_displays.len());
        for (id, display) in requested_displays.into_iter().enumerate() {
            match self.set_display(id, &display).await {
                Ok(kind) => {
                    if let SetDisplaySuccess::Updated = kind {
                        changed = true;
                    }
                    final_displays.push(display.clone())
                }
                Err(err) => {
                    errors = true;
                    match self.get_display(id).await {
                        Ok(current_display) => {
                            warn!(
                                "Failed to set display {}: {:?}. Current display is {:?}",
                                id, err, current_display
                            );
                            final_displays.push(current_display)
                        }
                        Err(err) => warn!("Failed to update display {}: {:?}. Ignoring", id, err),
                    }
                }
            }
        }

        if let Ok(extent) = final_displays.derive_extent() {
            DisplayWidth::update(extent.width as f64);
            DisplayHeight::update(extent.height as f64);
        }

        if changed {
            if let Err(e) = self.resize_screen_if(&final_displays, Ordering::Less).await {
                warn!("Failed to trim screen size: {:?}", e);
            }
        }

        if errors {
            Err(final_displays)
        } else {
            Ok(SetDisplaysSuccess {
                changed,
                displays: final_displays,
            })
        }
    }

    async fn clear_focus(&self) -> Result<()> {
        self.connection
            .checked_void_request(x::SetInputFocus {
                revert_to: x::InputFocus::None,
                focus: x::Window::none(),
                time: x::CURRENT_TIME,
            })
            .await
    }
}

enum SetDisplaySuccess {
    NoChange,
    Updated,
}

impl X {
    pub const DEFAULT_MAX_DISPLAY_COUNT: usize = 4;
    pub const DEFAULT_MAX_WIDTH: usize = Self::DEFAULT_MAX_DISPLAY_COUNT * 3840;
    pub const DEFAULT_MAX_HEIGHT: usize = Self::DEFAULT_MAX_DISPLAY_COUNT * 2160;
    pub const MIN_WIDTH: usize = 800;
    pub const MIN_HEIGHT: usize = 600;

    async fn screen_resources(&self) -> Result<randr::GetScreenResourcesCurrentReply> {
        self.connection
            .checked_request_with_reply(randr::GetScreenResourcesCurrent {
                window: self.root_window,
            })
            .await
    }

    async fn query_composite_version(connection: &XConnection) -> Result<()> {
        trace!("querying composite version");
        let reply = connection
            .checked_request_with_reply(composite::QueryVersion {
                client_major_version: composite::MAJOR_VERSION,
                client_minor_version: composite::MINOR_VERSION,
            })
            .await?;

        trace!("received reply");
        if reply.major_version() != composite::MAJOR_VERSION
            || reply.minor_version() != composite::MINOR_VERSION
        {
            bail!(
                "X Server Damage {}.{} != X Client Damage {}.{}",
                reply.major_version(),
                reply.minor_version(),
                composite::MAJOR_VERSION,
                composite::MINOR_VERSION
            );
        }

        Ok(())
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

    async fn query_randr_version(connection: &XConnection) -> Result<()> {
        trace!("querying randr version");
        let reply = connection
            .checked_request_with_reply(randr::QueryVersion {
                major_version: randr::MAJOR_VERSION,
                minor_version: randr::MINOR_VERSION,
            })
            .await?;

        if reply.major_version() != randr::MAJOR_VERSION
            || reply.minor_version() != randr::MINOR_VERSION
        {
            bail!(
                "X Server RandR {}.{} != X Client RandR {}.{}",
                reply.major_version(),
                reply.minor_version(),
                randr::MAJOR_VERSION,
                randr::MINOR_VERSION,
            );
        }

        Ok(())
    }

    async fn setup_damage_monitor(connection: &XConnection, root_window: &x::Window) -> Result<()> {
        connection
            .checked_void_request(composite::RedirectSubwindows {
                window: *root_window,
                update: composite::Redirect::Automatic,
            })
            .await?;
        let damage = connection.inner.generate_id();
        connection
            .checked_void_request(damage::Create {
                damage,
                drawable: x::Drawable::Window(*root_window),
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

    async fn setup_xfixes_monitor(connection: &XConnection, window: &x::Window) -> Result<()> {
        connection
            .checked_void_request(xfixes::SelectCursorInput {
                window: *window,
                event_mask: xfixes::CursorNotifyMask::all(),
            })
            .await
    }

    async fn hide_cursor(connection: &XConnection, window: &x::Window) -> Result<()> {
        connection
            .checked_void_request(xfixes::HideCursor { window: *window })
            .await
    }

    async fn setup_displays(
        connection: &XConnection,
        window: &x::Window,
    ) -> Result<(BTreeMap<usize, XDisplay>, u32)> {
        let screen_resources = connection
            .checked_request_with_reply(randr::GetScreenResources { window: *window })
            .await?;

        debug!(?screen_resources);

        let crtcs = screen_resources.crtcs();
        let outputs = screen_resources.outputs();
        let modes = screen_resources.modes();

        if crtcs.len() != outputs.len() {
            error!(?screen_resources);
            bail!("num crtcs {} != num outputs {}", crtcs.len(), outputs.len());
        }

        if modes.is_empty() {
            error!(?screen_resources);
            bail!("num modes is 0");
        }

        let displays: BTreeMap<usize, XDisplay> = crtcs
            .iter()
            .zip(outputs.iter())
            .enumerate()
            .map(|(key, (&crtc, &output))| (key, XDisplay { crtc, output }))
            .collect();

        Ok((displays, modes[0].id))
    }

    async fn resize_screen_if(
        &self,
        displays: impl IntoIterator<Item = &Display> + Clone,
        resize_if: Ordering,
    ) -> Result<()> {
        let requested = displays.clone().into_iter().derive_extent()?;
        let window = self.root_window;

        let current = self
            .connection
            .checked_request_with_reply(x::GetGeometry {
                drawable: x::Drawable::Window(self.root_window),
            })
            .await?;
        let current = Size::new(current.width() as _, current.height() as _);
        let combined = match resize_if {
            Ordering::Less => requested.min(current),
            Ordering::Greater => requested.max(current),
            Ordering::Equal => {
                unimplemented!("resize_if should be Ordering::Less or Ordering::Greater")
            }
        };
        debug!(?current, ?requested, ?combined);

        if combined.width.cmp(&current.width) != resize_if
            && combined.height.cmp(&current.height) != resize_if
        {
            return Ok(());
        }

        // From Xvfb's vfbRandRInit()
        let dpi = 96.0;
        let px_to_mm = 25.4 / dpi;

        let mm_width = (combined.width as f64 * px_to_mm).round() as u32;
        let mm_height = (combined.height as f64 * px_to_mm).round() as u32;

        debug!(new_screen_size=?(combined.width, combined.height, mm_width, mm_height));

        self.connection
            .checked_void_request(randr::SetScreenSize {
                window,
                width: combined.width as _,
                height: combined.height as _,
                mm_width,
                mm_height,
            })
            .await
    }

    async fn find_mode_by_name(&self, target_name: &str) -> Result<Option<randr::Mode>> {
        let sr = self.screen_resources().await?;

        let mut offset = 0;

        for mode in sr.modes() {
            let len = mode.name_len as usize;

            if offset + len <= sr.names().len() {
                let bytes = &sr.names()[offset..offset + len];

                if let Ok(name) = std::str::from_utf8(bytes) {
                    if name == target_name {
                        //
                        // Safety: XCB advises against calling new() directly, as it can create
                        // XIDs that have not been allocated by the X server. However, the
                        // `mode.id` used here is retrieved directly from the most recent
                        // `GetScreenResourcesCurrent` call. Since these mode IDs originate from
                        // X11 itself, they are guaranteed to be valid and not arbitrarily
                        // generated.
                        //
                        return Ok(Some(unsafe { randr::Mode::new(mode.id) }));
                    }
                }
            }

            offset += len;
        }

        Ok(None)
    }

    async fn get_display(&mut self, id: usize) -> Result<Display> {
        let crtc = self
            .displays
            .get(&id)
            .ok_or_else(|| anyhow!("Cannot find display id {}", id))?
            .crtc;

        let crtc_info = self
            .connection
            .checked_request_with_reply(randr::GetCrtcInfo {
                crtc,
                config_timestamp: x::CURRENT_TIME,
            })
            .await?;

        let (x, y, width, height, is_enabled) = (
            crtc_info.x() as usize,
            crtc_info.y() as usize,
            crtc_info.width() as usize,
            crtc_info.height() as usize,
            !crtc_info.outputs().is_empty(),
        );

        Ok(Display {
            area: Rect::new(Point::new(x, y), Size::new(width, height)),
            is_enabled,
        })
    }

    async fn set_display(&mut self, id: usize, display: &Display) -> Result<SetDisplaySuccess> {
        let xdisplay = self
            .displays
            .get(&id)
            .ok_or_else(|| anyhow!("Cannot find display id {}", id))?;

        let crtc_info = self
            .connection
            .checked_request_with_reply(randr::GetCrtcInfo {
                crtc: xdisplay.crtc,
                config_timestamp: x::CURRENT_TIME,
            })
            .await?;

        let is_crtc_enabled = !crtc_info.outputs().is_empty();

        if !display.is_enabled && !is_crtc_enabled {
            return Ok(SetDisplaySuccess::NoChange);
        }

        let width: u16 = display.area.size.width.try_into()?;
        let height: u16 = display.area.size.height.try_into()?;
        let x: i16 = display.origin().x.try_into()?;
        let y: i16 = display.origin().y.try_into()?;

        if is_crtc_enabled
            && display.is_enabled
            && crtc_info.width() == width
            && crtc_info.height() == height
            && crtc_info.x() == x
            && crtc_info.y() == y
        {
            return Ok(SetDisplaySuccess::NoChange);
        }

        // We assume that the screen size has been appropriately set when `set_display` is called
        // from `set_displays` and therefore we do not have to call `resize_screen_if` again

        let (mode, x, y, outputs) = if display.is_enabled {
            ensure!(
                Rect::from_size(Size::new(self.max_width, self.max_height))
                    .contains_rect(&display.area)
                    && display.area.size.width >= Self::MIN_WIDTH
                    && display.area.size.height >= Self::MIN_HEIGHT,
                "Display {}x{} @ ({}, {}) is out of bounds {}x{} - {}x{} {:?}",
                width,
                height,
                x,
                y,
                Self::MIN_WIDTH,
                Self::MIN_HEIGHT,
                self.max_width,
                self.max_height,
                display,
            );

            let name = format!("{width}x{height}");

            let mode = if let Some(mode) = self.find_mode_by_name(&name).await? {
                mode
            } else {
                //
                // New Mode must have refresh frequency components that evaluate to > 1
                //
                self.connection
                    .checked_request_with_reply(randr::CreateMode {
                        window: self.root_window,
                        name: name.as_bytes(),
                        mode_info: randr::ModeInfo {
                            id: 0,
                            width,
                            height,
                            dot_clock: 60,
                            hsync_start: 0,
                            hsync_end: 0,
                            htotal: 1,
                            hskew: 0,
                            vsync_start: 0,
                            vsync_end: 0,
                            vtotal: 1,
                            name_len: name.len() as u16,
                            mode_flags: randr::ModeFlag::empty(),
                        },
                    })
                    .await?
                    .mode()
            };

            self.connection
                .checked_void_request(randr::AddOutputMode {
                    output: xdisplay.output,
                    mode,
                })
                .await?;
            (mode, x, y, vec![xdisplay.output])
        } else {
            (randr::Mode::none(), 0, 0, vec![])
        };

        self.connection
            .checked_request_with_reply(randr::SetCrtcConfig {
                crtc: xdisplay.crtc,
                timestamp: x::CURRENT_TIME,
                config_timestamp: x::CURRENT_TIME,
                x,
                y,
                mode,
                rotation: randr::Rotation::ROTATE_0,
                outputs: &outputs,
            })
            .await?;

        if is_crtc_enabled
            && crtc_info.mode() != mode
            && crtc_info.mode().resource_id() != self.default_mode_id
        {
            //
            // Remove Modes no longer in use by the Output. This may fail if a Mode is currently in
            // use by another Output or the Mode was created by the display driver. Failures can be
            // safely ignored
            //
            let mode = crtc_info.mode();
            let _ = self
                .connection
                .checked_void_request(randr::DeleteOutputMode {
                    output: xdisplay.output,
                    mode,
                })
                .and_then(|_| {
                    self.connection
                        .checked_void_request(randr::DestroyMode { mode })
                })
                .await;
        }

        Ok(SetDisplaySuccess::Updated)
    }
}

struct XEventStreamMux {
    txs: Arc<Mutex<Vec<mpsc::UnboundedSender<Arc<XEvent>>>>>,
    _task: JoinHandle<Result<()>>,
}

impl XEventStreamMux {
    fn new(conn: Arc<xcb::Connection>) -> XEventStreamMux {
        let txs: Arc<Mutex<Vec<_>>> = Arc::default();
        let txs_clone = txs.clone();

        let _task = tokio::spawn(async move {
            let afd = AsyncFd::new(conn.clone())?;

            loop {
                // first drain any events already queued
                while let Some(ev) = conn.poll_for_event()? {
                    BackendEvent::inc();
                    let arc_ev = Arc::new(XEvent(ev));
                    txs_clone
                        .lock()
                        .expect("lock X mux txs")
                        .retain(|tx: &mpsc::UnboundedSender<_>| tx.send(arc_ev.clone()).is_ok());
                }

                // wait until the X socket becomes readable
                let mut guard = afd.readable().await?;
                guard.clear_ready();
            }
        });

        XEventStreamMux { txs, _task }
    }

    fn get_stream(&self) -> impl Stream<Item = Arc<XEvent>> + Send + Sync + Unpin + use<> {
        let mut txs = self.txs.lock().expect("lock X mux txs");
        let (tx, rx) = mpsc::unbounded_channel();
        txs.push(tx);
        UnboundedReceiverStream::new(rx)
    }
}

struct XConnection {
    inner: Arc<xcb::Connection>,
    preferred_screen: x::ScreenBuf,
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
    fn event_stream(&self) -> impl Stream<Item = Arc<XEvent>> + Send + Sync + Unpin + use<> {
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
            if let Some(response) = self.inner.poll_for_reply(&cookie) {
                if let Err(e) = response {
                    match e {
                        xcb::Error::Protocol(ref e) => {
                            debug!(x_error=?e, ?req, "X checked request error")
                        }
                        xcb::Error::Connection(ref e) => {
                            debug!(conn_error=?e, ?req, "X checked request error")
                        }
                    };
                    bail!(e)
                }
                return Ok(response.unwrap());
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
                debug!(x_error=?e, ?req, "X void request error");
            }
            reply
        })?)
    }

    /// Create a new X with an optional name of the server to connect to.  If the `name` is `None`,
    /// this will use the `DISPLAY` environment variable.
    fn new<'s>(name: impl Into<Option<&'s str>>) -> Result<Self> {
        let name = name.into();
        trace!("Connecting to display {:?}", name);
        let (conn, preferred_screen_index) = xcb::Connection::connect_with_extensions(
            name,
            &[
                xcb::Extension::Composite,
                xcb::Extension::Damage,
                xcb::Extension::RandR,
                xcb::Extension::Test,
                xcb::Extension::XFixes,
            ],
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
        let x = X::initialize(1920, 1080, 1, Some(&mut Command::new("glxgears")))
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
    async fn setup_displays() {
        let mut x = X::initialize(
            1920,
            1080,
            X::DEFAULT_MAX_DISPLAY_COUNT,
            Some(&mut Command::new("glxgears")),
        )
        .await
        .expect("x initialize");

        assert_eq!(x.displays.keys().len(), x.max_display_count);

        let mut displays = Vec::with_capacity(x.max_display_count);
        for i in 0..x.max_display_count {
            displays.push(x.get_display(i).await.expect("get display"));
        }

        // Expect only the first Display to be enabled
        assert!(displays[0].is_enabled);
        assert!(!displays[1..].iter().any(|d| d.is_enabled))
    }

    #[test(tokio::test(flavor = "multi_thread"))]
    #[serial]
    async fn set_display() {
        let max_width = 7680;
        let max_height = 4320;

        let mut x = X::initialize(
            max_width,
            max_height,
            X::DEFAULT_MAX_DISPLAY_COUNT,
            Some(&mut Command::new("glxgears")),
        )
        .await
        .expect("x initialize");

        let areas = vec![
            Rect::new(Point::zero(), Size::new(max_width, max_height)),
            Rect::new(Point::zero(), Size::new(1920, 1080)),
            Rect::new(Point::new(1920, 0), Size::new(1920, 1080)),
            Rect::new(Point::zero(), Size::new(X::MIN_WIDTH, X::MIN_HEIGHT)),
            Rect::new(
                Point::new(X::MIN_WIDTH, 0),
                Size::new(X::MIN_WIDTH, X::MIN_HEIGHT),
            ),
            Rect::new(Point::zero(), Size::new(1024, 768)),
            Rect::new(Point::new(1024, 0), Size::new(1024, 768)),
            Rect::new(Point::zero(), Size::new(800, 600)),
            Rect::new(Point::new(800, 0), Size::new(800, 600)),
            Rect::new(Point::zero(), Size::new(1440, 900)),
            Rect::new(Point::new(1440, 0), Size::new(1440, 900)),
            Rect::new(Point::zero(), Size::new(1280, 1024)),
            Rect::new(Point::new(1280, 0), Size::new(1280, 1024)),
            Rect::new(Point::zero(), Size::new(823, 688)),
            Rect::new(Point::new(823, 0), Size::new(823, 688)),
            Rect::new(Point::zero(), Size::new(823, 688)),
            Rect::new(Point::new(823, 0), Size::new(823, 688)),
            Rect::new(Point::zero(), Size::new(800, 600)),
            Rect::new(Point::new(800, 0), Size::new(800, 600)),
        ];

        let mut display = Display {
            area: Rect::default(),
            is_enabled: true,
        };

        // Ensure display changes work at both origin and offset
        for area in &areas {
            display.area = *area;
            x.resize_screen_if([&display], Ordering::Greater)
                .await
                .expect("resize screen");
            x.set_display(0, &display)
                .await
                .unwrap_or_else(|err| panic!("set display to {display:?}: {err}"));
            assert_eq!(display, x.get_display(0).await.expect("get display"));
        }

        // Ensure only 2 modes still exist
        let screen_resources = x.screen_resources().await.expect("screen resources");
        assert_eq!(screen_resources.modes().len(), 2);

        // Ensure the default mode is first
        assert_eq!(screen_resources.modes()[0].id, x.default_mode_id);

        // Ensure the most recent resolution is last
        let last_area = areas.last().expect("last");
        let name = format!("{}x{}", last_area.width(), last_area.height());
        assert_eq! {
            screen_resources
                .modes().last().expect("last").id,
            x.find_mode_by_name(&name).await.expect("find_by_name").expect("missing mode").resource_id()
        }

        for id in 1..x.max_display_count {
            // Ensure next CRTC is (still) disabled
            display.area = Rect::default();
            display.is_enabled = false;
            assert_eq!(display, x.get_display(id).await.expect("get display"));

            // Ensure next CRTC is enabled
            display.area = *last_area;
            display.is_enabled = true;
            x.set_display(id, &display).await.expect("set display");
            assert_eq!(display, x.get_display(id).await.expect("get display"));
        }

        // Ensure setting Display beyond max bounds fails
        display.area.origin = Point::new(max_width, max_height);
        assert!(x.set_display(0, &display).await.is_err());

        // Ensure setting Display too small fails
        display.area = Rect::new(
            Point::new(0, 0),
            Size::new(X::MIN_WIDTH - 5, X::MIN_HEIGHT - 5),
        );
        assert!(x.set_display(0, &display).await.is_err());
    }

    fn create_displays(width: usize, height: usize, count: usize) -> Vec<Display> {
        let size = Size::new(width / count, height);
        Display::linear_displays(count, size)
    }

    #[test(tokio::test(flavor = "multi_thread"))]
    #[serial]
    async fn set_displays() {
        let max_width = (X::DEFAULT_MAX_DISPLAY_COUNT + 5) * X::MIN_WIDTH;
        let max_height = 1080;

        let mut x = X::initialize(
            max_width,
            max_height,
            X::DEFAULT_MAX_DISPLAY_COUNT,
            Some(&mut Command::new("glxgears")),
        )
        .await
        .expect("x initialize");

        let displays = create_displays(max_width, max_height, x.max_display_count);
        let result = x.set_displays(displays).await.expect("set displays");

        // Ensure resulting display count matches what we proposed
        assert!(result.displays.len() == x.max_display_count);

        let displays = create_displays(max_width, max_height, x.max_display_count + 5);
        let result = x.set_displays(displays).await.expect("set displays");

        // Ensure resulting displays are truncated to max_display_count
        assert!(result.displays.len() == x.max_display_count);

        let displays = create_displays(max_width, max_height, 1);
        let mut result = x
            .set_displays(displays.clone())
            .await
            .unwrap_or_else(|_| panic!("set displays {displays:?} failed"));

        // Ensure resulting displays are expanded to max_display_count
        assert!(result.displays.len() == x.max_display_count);
        result.displays.swap_remove(0);

        // Ensure expanded displays are disabled
        for d in &result.displays {
            assert!(!d.is_enabled);
        }
    }

    #[test]
    fn parse_shmem_id_line() {
        let s = "screen 0 shmid 123";
        let (screen, id) =
            scan_fmt!(&s, "screen {d} shmid {d}", usize, usize).expect("scan_fmt failed");
        assert_eq!(screen, 0);
        assert_eq!(id, 123);
    }
}
