use aperturec_client::{
    Client, Connection, Cursor, DisplayMode, Draw, Event as ServerEvent, QuitReason, config,
    state::LockState,
};
use aperturec_graphics::{
    display::{Display, DisplayConfiguration},
    euclid_collections::{EuclidMap, EuclidSet},
    prelude::*,
};
use aperturec_utils::channels::SenderExt;

use anyhow::{Result, bail};
use async_channel::{Sender, unbounded};
use clap::Parser;
use futures::StreamExt;
use glib::translate::IntoGlib;
use gtk4::{
    self as gtk,
    gdk::{self, prelude::*},
    gio, glib,
    prelude::*,
};
#[cfg(target_os = "macos")]
use keycode::{KeyMap, KeyMapping};
use std::{
    cell::{Cell, RefCell},
    iter, mem,
    pin::pin,
    rc::Rc,
    sync::{
        Arc, LazyLock,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};
use strum::{AsRefStr, EnumIter, IntoEnumIterator};
use tracing::*;
use tracing_subscriber::prelude::*;
#[cfg(target_os = "windows")]
use windows::Win32::UI::Input::KeyboardAndMouse::{
    MAPVK_VK_TO_VSC_EX, MapVirtualKeyW, VIRTUAL_KEY, VK_APPS, VK_DELETE, VK_DOWN, VK_END, VK_HOME,
    VK_INSERT, VK_LEFT, VK_LWIN, VK_NEXT, VK_PRIOR, VK_RCONTROL, VK_RIGHT, VK_RMENU, VK_RWIN,
    VK_UP,
};

mod image;

const DEFAULT_RESOLUTION: Size = Size::new(800, 600);

static GTK_INIT: LazyLock<()> = LazyLock::new(|| {
    gdk::set_allowed_backends("x11,*");
    gtk::init().expect("Failed to initialize GTK");
});

#[cfg(target_os = "macos")]
fn map_gdk_key(keyval: gdk::Key, raw_keycode: u32) -> Option<u32> {
    let mapping = KeyMapping::Mac(raw_keycode as u16);
    match KeyMap::try_from(mapping) {
        Ok(map) => Some(map.xkb as u32),
        Err(_) => {
            warn!(keyval = %keyval.into_glib(), raw_keycode = raw_keycode, "macOS key mapping failed; dropping key");
            None
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn map_gdk_key(_keyval: gdk::Key, keycode: u32) -> Option<u32> {
    Some(keycode)
}

// Windows helper copied from GTK3 client: convert virtual-key to OEM scan code for keycode crate.
#[cfg(target_os = "windows")]
fn convert_win_virtual_key_to_scan_code(virtual_key: u32) -> anyhow::Result<u16> {
    let raw = unsafe { MapVirtualKeyW(virtual_key, MAPVK_VK_TO_VSC_EX) };
    if raw == 0 {
        anyhow::bail!(
            "Failed to convert virtual key code {:#X} to scan code",
            virtual_key
        );
    }
    let mut sc = (raw & 0x00FF) as u16;
    let has_ext_bit = (raw & 0x0100) != 0;
    let is_ext_vk = matches!(
        VIRTUAL_KEY(virtual_key as u16),
        VK_LEFT
            | VK_RIGHT
            | VK_UP
            | VK_DOWN
            | VK_HOME
            | VK_END
            | VK_INSERT
            | VK_DELETE
            | VK_PRIOR
            | VK_NEXT
            | VK_RCONTROL
            | VK_RMENU
            | VK_LWIN
            | VK_RWIN
            | VK_APPS
    );
    if has_ext_bit || is_ext_vk {
        sc |= 0xE000;
    }
    Ok(sc)
}

#[cfg(target_os = "windows")]
fn map_gdk_key(keyval: gdk::Key, raw_keycode: u32) -> Option<u32> {
    let scancode = match convert_win_virtual_key_to_scan_code(raw_keycode) {
        Ok(sc) => sc,
        Err(err) => {
            warn!(
                "Failed to convert key code {:#X}: {}. Ignoring key event.",
                raw_keycode, err
            );
            return None;
        }
    };
    let mapping = KeyMapping::Win(scancode);
    match KeyMap::try_from(mapping) {
        Ok(map) => Some(map.xkb as u32),
        Err(_) => {
            warn!(
                "Failed to convert key code {:#X} to X11 keycode. Ignoring key event.",
                raw_keycode
            );
            None
        }
    }
}

fn current_lock_state() -> Result<LockState> {
    *GTK_INIT;
    let Some(display) = gdk::Display::default() else {
        bail!("no default display");
    };
    let Some(seat) = display.default_seat() else {
        bail!("no default seat");
    };
    let Some(keyboard) = seat.keyboard() else {
        bail!("no keyboard");
    };
    Ok(LockState {
        is_caps_locked: keyboard.is_caps_locked(),
        is_num_locked: keyboard.is_num_locked(),
        is_scroll_locked: keyboard.is_scroll_locked(),
    })
}

#[derive(Debug, Clone, Default)]
struct Monitors {
    map: EuclidMap<(Rect, gdk::Monitor)>,
}

impl Monitors {
    #[allow(unused)]
    fn total_areas(&self) -> impl Iterator<Item = Rect> {
        self.map.keys()
    }

    fn usable_areas(&self) -> impl Iterator<Item = Rect> {
        self.map.values().map(|&(usable_area, _)| usable_area)
    }

    fn matches(&self, display_configuration: &DisplayConfiguration) -> bool {
        display_configuration
            .display_decoder_infos
            .iter()
            .filter(|ddi| ddi.display.is_enabled)
            .map(|ddi| ddi.display.area)
            .collect::<EuclidSet>()
            == self.usable_areas().collect()
    }

    #[allow(unused)]
    fn is_multi(&self) -> bool {
        self.map.len() > 1
    }

    fn as_displays(&self) -> impl Iterator<Item = Display> {
        self.map.iter().map(|(_, &(usable_area, _))| Display {
            area: usable_area,
            is_enabled: true,
        })
    }

    fn current() -> Result<Self> {
        *GTK_INIT;

        fn gdkrect2rect(geo: gdk::Rectangle) -> Rect {
            Rect::new(
                Point::new(geo.x() as _, geo.y() as _),
                Size::new(geo.width() as _, geo.height() as _),
            )
        }

        fn monitor_workarea(mon: &gdk::Monitor) -> Rect {
            #[cfg(target_os = "macos")]
            {
                gdkrect2rect(gdk4_macos::MacosMonitor::workarea(mon))
            }

            #[cfg(target_os = "windows")]
            {
                Rect::new(Point::new(0, 0), Size::new(800, 600))
                // unimplemented!("unsure about how to get the monitors on windows")
                // gdkrect2rect(gdk4_win32::Win32Monitor::workarea(mon))
            }

            #[cfg(target_os = "linux")]
            {
                gdkrect2rect(mon.geometry())
            }
        }

        let Some(display) = gdk::Display::default() else {
            bail!("no default display");
        };

        let mut map = EuclidMap::new();
        let monitor_objs_list = display.monitors();

        if monitor_objs_list.n_items() == 0 {
            bail!("no monitors found for display {display:?}");
        }

        for mon_res in monitor_objs_list.iter::<gdk::Monitor>() {
            let mon = mon_res?;

            let total_area = gdkrect2rect(mon.geometry());
            let usable_area = monitor_workarea(&mon);
            if !total_area.contains_rect(&usable_area) {
                bail!("usable area exceeds total area of monitor");
            }

            let overlaps = map.insert(total_area, (usable_area, mon.clone()));
            if !overlaps.is_empty() {
                bail!("at least two monitors overlap: {total_area:?} and {overlaps:?}");
            }
        }

        if map.is_empty() {
            bail!("no monitors");
        }

        Ok(Monitors { map })
    }

    fn gdk_monitor_at_point(&self, point: Point) -> Option<gdk::Monitor> {
        self.map.get(point).map(|(_, (_, mon))| mon.clone())
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum WindowMode {
    Single { fullscreen: bool },
    Multi,
}

impl WindowMode {
    fn derive_next<T: Into<WindowMode>>(
        current_mode: T,
        display_config: &DisplayConfiguration,
        monitors: &Monitors,
    ) -> Result<Self> {
        let current_mode = current_mode.into();
        let num_enabled_displays = display_config
            .display_decoder_infos
            .iter()
            .filter(|ddi| ddi.display.is_enabled)
            .count();

        if num_enabled_displays == 0 {
            bail!("no enabled displays");
        }

        if num_enabled_displays == 1 {
            match current_mode {
                WindowMode::Single { fullscreen: true } => {
                    Ok(WindowMode::Single { fullscreen: true })
                }
                WindowMode::Single { fullscreen: false } => {
                    Ok(WindowMode::Single { fullscreen: false })
                }
                WindowMode::Multi => {
                    warn!(
                        "Server provided single-monitor display configuration in multi-monitor mode"
                    );
                    info!("Reverting to single-monitor mode");
                    Ok(WindowMode::Single { fullscreen: true })
                }
            }
        } else {
            if matches!(current_mode, WindowMode::Single { .. }) {
                bail!("Server provided multi-monitor display configuration in single-window mode");
            }
            if !monitors.matches(display_config) {
                bail!(
                    "Server provided multi-monitor initial display configuration which does not match monitors geometry on client"
                );
            }
            Ok(WindowMode::Multi)
        }
    }
}

impl From<DisplayMode> for WindowMode {
    fn from(dm: DisplayMode) -> Self {
        match dm {
            DisplayMode::Windowed { .. } => WindowMode::Single { fullscreen: false },
            DisplayMode::SingleFullscreen => WindowMode::Single { fullscreen: true },
            DisplayMode::MultiFullscreen => WindowMode::Multi,
        }
    }
}

struct Ui {
    conn: Arc<Connection>,
    mm_windows: EuclidMap<Window>,
    single_window: Window,
    server_can_mm: bool,
    hotplug_disabled_mm: bool,
    window_mode: WindowMode,
    resize_timeout: Rc<Cell<Option<glib::SourceId>>>,
    display_config_id: usize,
    monitors: Monitors,
    app: gtk::Application,
    ui_event_tx: Sender<UiEvent>,
}

impl Ui {
    fn new(
        conn: Arc<Connection>,
        initial_display_mode: DisplayMode,
        ui_event_tx: Sender<UiEvent>,
        app: gtk::Application,
    ) -> Result<Self> {
        let initial_display_config = conn.display_configuration();

        let monitors = Monitors::current()?;
        let initial_window_mode =
            WindowMode::derive_next(initial_display_mode, &initial_display_config, &monitors)?;

        // Notify UI when monitors are hot-plugged or removed so we can fall back
        // to a safe mode for the remainder of the session.
        if let Some(display) = gdk::Display::default() {
            let monitors_model = display.monitors();
            monitors_model.connect_items_changed(glib::clone!(
                #[strong(rename_to = tx)]
                ui_event_tx,
                move |_, _, _, _| {
                    tx.send_or_warn(UiEvent::MonitorsChanged);
                }
            ));
        } else {
            warn!("No default display found; monitor hotplug will not be handled");
        }

        let dc_size = initial_display_config.display_decoder_infos[0]
            .display
            .area
            .size;
        let initial_windowed_size = match initial_window_mode {
            WindowMode::Multi => DEFAULT_RESOLUTION,
            WindowMode::Single { fullscreen } => {
                if fullscreen {
                    dc_size / 2
                } else {
                    dc_size
                }
            }
        };

        let server_can_mm = !matches!(
            (initial_display_mode, initial_window_mode),
            (DisplayMode::MultiFullscreen, WindowMode::Single { .. })
        );

        let single_window = Window::new(conn.clone(), initial_windowed_size, ui_event_tx.clone());
        single_window.drawing_area.connect_resize(glib::clone!(
            #[strong(rename_to = tx)]
            ui_event_tx,
            move |_, width, height| {
                tx.send_or_warn(UiEvent::WindowResized(Size::new(width as _, height as _)))
            }
        ));

        let mm_windows = monitors
            .usable_areas()
            .map(|area| {
                (
                    area,
                    Window::new(conn.clone(), area.size, ui_event_tx.clone()),
                )
            })
            .collect();

        let mut ui = Ui {
            conn,
            mm_windows,
            single_window,
            server_can_mm,
            hotplug_disabled_mm: false,
            window_mode: initial_window_mode,
            resize_timeout: Rc::default(),
            display_config_id: initial_display_config.id,
            monitors,
            app,
            ui_event_tx,
        };

        ui.set_window_mode(initial_window_mode)?;
        ui.update_display_mode_actions();
        Ok(ui)
    }

    fn report_resize(&mut self, size: Size) {
        debug!(?size, "resize");
        if let Some(source_id) = self.resize_timeout.take() {
            source_id.remove();
        }

        if !matches!(self.window_mode, WindowMode::Single { .. }) {
            return;
        }

        const RESIZE_TIMER: Duration = Duration::from_millis(600);
        self.resize_timeout
            .set(Some(glib::source::timeout_add_local_once(
                RESIZE_TIMER,
                glib::clone!(
                    #[weak(rename_to = conn)]
                    self.conn,
                    #[weak(rename_to = resize_timeout)]
                    self.resize_timeout,
                    move || {
                        debug!(?size, "resize timeout complete");
                        let display = Display::new(Rect::new(Point::zero(), size), true);
                        if let Err(error) = conn.request_display_change(&[display]) {
                            warn!(%error, "request resize");
                        }
                        resize_timeout.set(None);
                    }
                ),
            )));
    }

    fn report_window_closed(&self) {
        debug!("reported window closed");
        for window in self.all_windows() {
            window.gtk_window.close();
            if let Some(app) = window.gtk_window.application() {
                app.remove_window(&window.gtk_window);
            }
        }
    }

    fn set_window_mode(&mut self, window_mode: WindowMode) -> Result<()> {
        if matches!(window_mode, WindowMode::Multi)
            && (!self.server_can_mm || self.hotplug_disabled_mm)
        {
            bail!("multi-monitor mode disabled for this session");
        }
        match window_mode {
            WindowMode::Single { fullscreen } => {
                if !fullscreen {
                    let size = self.single_window.image.borrow().size();
                    self.single_window
                        .gtk_window
                        .set_default_size(size.width as _, size.height as _);
                }
                if fullscreen {
                    self.single_window.fullscreen();
                } else {
                    self.single_window.window();
                }
                self.single_window.show();

                for (_, window) in &mut self.mm_windows {
                    window.hide();
                }
            }
            WindowMode::Multi => {
                for (area, window) in &mut self.mm_windows {
                    let Some(mon) = self.monitors.gdk_monitor_at_point(area.origin) else {
                        panic!("monitor state out of sync");
                    };
                    window.fullscreen_on_monitor(mon);
                    window.show();
                }
                self.single_window.hide();
            }
        }
        self.window_mode = window_mode;
        self.request_display_config()?;
        self.update_display_mode_actions();
        Ok(())
    }

    /// Applies a window mode change initiated by the server without sending a
    /// new display configuration request back to the server (avoids feedback
    /// loops).
    fn set_window_mode_from_server(&mut self, window_mode: WindowMode) -> Result<()> {
        if matches!(window_mode, WindowMode::Multi)
            && (!self.server_can_mm || self.hotplug_disabled_mm)
        {
            bail!("server requested multi-monitor mode after it was disabled");
        }
        match window_mode {
            WindowMode::Single { fullscreen } => {
                if !fullscreen {
                    let size = self.single_window.image.borrow().size();
                    self.single_window
                        .gtk_window
                        .set_default_size(size.width as _, size.height as _);
                }
                if fullscreen {
                    self.single_window.fullscreen();
                } else {
                    self.single_window.window();
                }
                self.single_window.show();

                for (_, window) in &mut self.mm_windows {
                    window.hide();
                }
            }
            WindowMode::Multi => {
                for (area, window) in &mut self.mm_windows {
                    let Some(mon) = self.monitors.gdk_monitor_at_point(area.origin) else {
                        panic!("monitor state out of sync");
                    };
                    window.fullscreen_on_monitor(mon);
                    window.show();
                }
                self.single_window.hide();
            }
        }
        self.window_mode = window_mode;
        self.update_display_mode_actions();
        Ok(())
    }

    fn handle_monitors_changed(&mut self) -> Result<()> {
        if self.hotplug_disabled_mm {
            return Ok(());
        }

        warn!("Monitor configuration changed; disabling multi-monitor mode for this session");
        self.hotplug_disabled_mm = true;
        self.server_can_mm = false;
        self.update_display_mode_actions();

        match Monitors::current() {
            Ok(monitors) => {
                self.monitors = monitors;
            }
            Err(error) => {
                warn!(%error, "failed to refresh monitor info after hotplug");
            }
        }

        if matches!(self.window_mode, WindowMode::Multi) {
            if let Err(error) = self.set_window_mode(WindowMode::Single { fullscreen: true }) {
                warn!(%error, "failed to fall back to single-monitor fullscreen");
            }
        }

        self.show_modal(
            "Monitor configuration changed",
            "Multi-monitor fullscreen has been disabled for this session. Restart the client to re-enable multi-monitor fullscreen",
            gtk::MessageType::Warning,
        );

        Ok(())
    }

    fn update_display_mode_actions(&self) {
        use gio::prelude::ActionMapExt;

        // Update Window action
        if let Some(action) = self.app.lookup_action(Action::Window.as_ref())
            && let Some(simple_action) = action.downcast_ref::<gio::SimpleAction>()
        {
            let enabled = !matches!(self.window_mode, WindowMode::Single { fullscreen: false });
            simple_action.set_enabled(enabled);
        }

        // Update Single Monitor Fullscreen action
        if let Some(action) = self
            .app
            .lookup_action(Action::SingleMonitorFullscreen.as_ref())
            && let Some(simple_action) = action.downcast_ref::<gio::SimpleAction>()
        {
            let enabled = !matches!(self.window_mode, WindowMode::Single { fullscreen: true });
            simple_action.set_enabled(enabled);
        }

        // Update Multi-Monitor Fullscreen action (Linux only)
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        if let Some(action) = self
            .app
            .lookup_action(Action::MultiMonitorFullscreen.as_ref())
            && let Some(simple_action) = action.downcast_ref::<gio::SimpleAction>()
        {
            let enabled = self.server_can_mm
                && !self.hotplug_disabled_mm
                && !matches!(self.window_mode, WindowMode::Multi);
            simple_action.set_enabled(enabled);
        }
    }

    fn request_display_config(&self) -> Result<()> {
        match self.window_mode {
            WindowMode::Single { .. } => {
                let size = self.single_window.image.borrow().size();
                self.single_window.image.replace(image::Image::black(size));
                self.single_window.drawing_area.queue_draw();
                let display = Display::new(Rect::new(Point::zero(), size), true);
                self.conn.request_display_change(&[display])?;
            }
            WindowMode::Multi => {
                let displays: Vec<Display> = self
                    .mm_windows
                    .iter()
                    .map(|(area, window)| {
                        window.image.replace(image::Image::black(area.size));
                        window.drawing_area.queue_draw();
                        Display::new(area, true)
                    })
                    .collect();
                self.conn.request_display_change(&displays)?;
            }
        }
        Ok(())
    }

    fn draw(&mut self, draw: Draw) {
        if let Some((win_area, win)) = self.window_at(draw.origin) {
            win.image.borrow_mut().draw(
                draw.pixels.view(),
                (draw.origin - win_area.origin).to_point(),
                draw.frame,
            );
            win.drawing_area.queue_draw();
        }
    }

    fn change_cursor(&mut self, cursor: Cursor) {
        let shape = cursor.pixels.shape();
        let (width, height) = (shape[0], shape[1]);
        let cursor_vec = if cursor.pixels.is_standard_layout() {
            cursor.pixels.into_owned()
        } else {
            cursor.pixels.to_owned()
        }
        .into_raw_vec_and_offset()
        .0;

        // Convert Pixel32 buffer to RGBA byte stream; cast_vec requires identical
        // alignment between source and target, which Pixel32 (align 4) and u8
        // do not share. Use cast_slice + to_vec to avoid the runtime
        // AlignmentMismatch panic seen in bytemuck::cast_vec.
        let bytes_vec: Vec<u8> = bytemuck::cast_slice(&cursor_vec).to_vec();

        let texture = gdk::MemoryTexture::new(
            width as _,
            height as _,
            gdk::MemoryFormat::R8g8b8a8,
            &glib::Bytes::from_owned(bytes_vec),
            width * mem::size_of::<Pixel32>(),
        );
        let cursor = gdk::Cursor::builder()
            .texture(&texture)
            .hotspot_x(cursor.hot.x as _)
            .hotspot_y(cursor.hot.y as _)
            .build();
        for da in self.all_windows().map(|w| &w.drawing_area) {
            da.set_cursor(Some(&cursor));
        }
    }

    fn set_display_configuration(&mut self, display_config: DisplayConfiguration) -> Result<()> {
        debug!(?display_config);
        if display_config.id <= self.display_config_id {
            bail!("out-of-date display configuration");
        }

        let next_window_mode = match WindowMode::derive_next(
            self.window_mode,
            &display_config,
            &self.monitors,
        ) {
            Ok(mode) => mode,
            Err(error) if self.hotplug_disabled_mm => {
                info!(
                    %error,
                    "Ignoring incompatible multi-monitor config after monitor change; forcing single-monitor fullscreen"
                );
                WindowMode::Single { fullscreen: true }
            }
            Err(error) => return Err(error),
        };

        let effective_window_mode = if self.hotplug_disabled_mm
            && matches!(next_window_mode, WindowMode::Multi)
        {
            info!(
                "Ignoring server multi-monitor config after monitor change; forcing single-monitor fullscreen"
            );
            WindowMode::Single { fullscreen: true }
        } else {
            next_window_mode
        };

        // Detect if server can't handle multi-monitor
        if matches!(self.window_mode, WindowMode::Multi)
            && matches!(next_window_mode, WindowMode::Single { .. })
        {
            info!("Multi-monitor mode disabled - server does not support it");
            self.server_can_mm = false;
            self.update_display_mode_actions();
        }

        match effective_window_mode {
            WindowMode::Multi => { /* geometry already validated */ }
            WindowMode::Single { .. } => {
                let size = display_config.display_decoder_infos[0].display.size();
                self.single_window.image.replace(image::Image::black(size));
                self.single_window
                    .gtk_window
                    .set_default_size(size.width as _, size.height as _);
                self.single_window.drawing_area.queue_draw();
            }
        }

        if matches!(next_window_mode, WindowMode::Multi)
            && !matches!(self.window_mode, WindowMode::Multi)
        {
            for (area, window) in &mut self.mm_windows {
                window.image.replace(image::Image::black(area.size));
                window.drawing_area.queue_draw();
            }
        }

        if effective_window_mode != self.window_mode {
            self.set_window_mode_from_server(effective_window_mode)?;
        }

        self.display_config_id = display_config.id;
        Ok(())
    }

    #[allow(unused)]
    fn show_modal(&self, primary_text: &str, secondary_text: &str, message_type: gtk::MessageType) {
        self.show_modal_with_callback(primary_text, secondary_text, message_type, |_, _| {});
    }

    fn show_modal_with_callback<CB>(
        &self,
        primary_text: &str,
        secondary_text: &str,
        message_type: gtk::MessageType,
        on_response: CB,
    ) where
        CB: Fn(&gtk::MessageDialog, gtk::ResponseType) + 'static,
    {
        let mut dialog_builder = gtk::MessageDialog::builder()
            .modal(true)
            .message_type(message_type)
            .buttons(gtk::ButtonsType::Close)
            .text(primary_text)
            .secondary_text(secondary_text)
            .hide_on_close(true);
        if let Some(parent) = self.app.active_window() {
            dialog_builder = dialog_builder.transient_for(&parent);
        }
        let dialog = dialog_builder.build();
        dialog.connect_response(move |dialog, response| {
            on_response(dialog, response);
            dialog.close();
        });
        dialog.show();
    }

    fn show_exit_reason(&self, reason: QuitReason) {
        let message_type = match reason {
            QuitReason::ServerGoodbye { .. } => gtk::MessageType::Info,
            QuitReason::UnrecoverableError(_) => gtk::MessageType::Error,
        };
        self.show_modal_with_callback(
            "ApertureC Client exiting",
            &format!("{reason}"),
            message_type,
            glib::clone!(
                #[strong(rename_to = tx)]
                self.ui_event_tx,
                move |_, _| {
                    tx.send_or_warn(UiEvent::UserConfirmedClose);
                }
            ),
        );
    }

    fn all_windows(&self) -> impl Iterator<Item = &Window> {
        self.mm_windows
            .iter()
            .map(|(_, window)| window)
            .chain(iter::once(&self.single_window))
    }

    fn window_at(&self, point: Point) -> Option<(Rect, &Window)> {
        match self.window_mode {
            WindowMode::Single { .. } => {
                let image = self.single_window.image.borrow();
                if Rect::from(image.size()).contains(point) {
                    Some((
                        Rect::new(Point::zero(), self.single_window.image.borrow().size()),
                        &self.single_window,
                    ))
                } else {
                    None
                }
            }
            WindowMode::Multi => self
                .mm_windows
                .iter()
                .find(|(area, _)| area.contains(point)),
        }
    }
}

struct Window {
    gtk_window: gtk::ApplicationWindow,
    drawing_area: gtk::DrawingArea,
    image: Rc<RefCell<image::Image>>,
}

impl Window {
    fn new(conn: Arc<Connection>, size: Size, ui_event_tx: Sender<UiEvent>) -> Window {
        let image = Rc::new(RefCell::new(image::Image::new(size)));
        let ec_motion = {
            let ec_motion = gtk::EventControllerMotion::new();
            ec_motion.connect_motion(glib::clone!(
                #[weak]
                conn,
                move |ec, x, y| {
                    trace!(%x, %y, "motion");
                    if let Some(widget) = ec.widget() {
                        widget.grab_focus();
                    }
                    if let Err(error) = conn.pointer_move(x.round() as _, y.round() as _) {
                        warn!(%error, "failed handling motion");
                    }
                }
            ));
            ec_motion
        };

        let ec_key = {
            let ec_key = gtk::EventControllerKey::new();
            ec_key.connect_key_pressed(glib::clone!(
                #[weak]
                conn,
                #[upgrade_or]
                glib::signal::Propagation::Stop,
                move |_, keyval, keycode, _state| {
                    if let Some(code) = map_gdk_key(keyval, keycode) {
                        debug!(keycode = code, "press");
                        if let Err(error) = conn.key_press(code) {
                            warn!(%error, "failed handling key press");
                        }
                    } else {
                        warn!(keyval = %keyval.into_glib(), %keycode, "unmapped key press on macOS; dropping");
                    }
                    glib::signal::Propagation::Stop
                }
            ));
            ec_key.connect_key_released(glib::clone!(
                #[weak]
                conn,
                move |_, keyval, keycode, _state| {
                    if let Some(code) = map_gdk_key(keyval, keycode) {
                        debug!(keycode = code, "release");
                        if let Err(error) = conn.key_release(code) {
                            warn!(%error, "failed handling key release");
                        }
                    } else {
                        warn!(keyval = %keyval.into_glib(), %keycode, "unmapped key release on macOS; dropping");
                    }
                }
            ));
            ec_key
        };

        let ec_focus = {
            let ec_focus = gtk::EventControllerFocus::new();
            ec_focus.connect_enter(|_| {
                debug!("entered");
            });
            ec_focus.connect_leave(|_| {
                debug!("leave");
            });
            ec_focus
        };

        let gc = {
            let gc = gtk::GestureClick::builder()
                .button(0)
                .exclusive(true)
                .build();
            gc.connect_pressed(glib::clone!(
                #[weak]
                conn,
                move |gesture, _, x, y| {
                    // GTK4 buttons start at 1, while the server expects the buttons to start at 0
                    let raw = gesture.current_button();
                    if raw == 0 {
                        return;
                    }
                    let button = raw - 1;
                    debug!(%button, %x, %y, "press");
                    if let Err(error) =
                        conn.mouse_button_press(button, x.round() as _, y.round() as _)
                    {
                        warn!(%error, "failed handling mouse press");
                    }
                }
            ));
            gc.connect_released(glib::clone!(
                #[weak]
                conn,
                move |gesture, _, x, y| {
                    // GTK4 buttons start at 1, while the server expects the buttons to start at 0
                    let raw = gesture.current_button();
                    if raw == 0 {
                        return;
                    }
                    let button = raw - 1;
                    debug!(%button, %x, %y, "release");
                    if let Err(error) =
                        conn.mouse_button_release(button, x.round() as _, y.round() as _)
                    {
                        warn!(%error, "failed handling mouse release");
                    }
                }
            ));
            gc
        };

        let drawing_area = gtk::DrawingArea::builder().focusable(true).build();
        drawing_area.add_controller(ec_motion);
        drawing_area.add_controller(ec_focus);
        drawing_area.add_controller(ec_key);
        drawing_area.add_controller(gc);
        drawing_area.set_draw_func(glib::clone!(
            #[weak]
            image,
            move |_, cr, _, _| {
                let image_rect = Rect::from_size(image.borrow().size());
                image.borrow_mut().with_surface(|surface| {
                    cr.reset_clip();
                    cr.set_source_surface(surface, 0_f64, 0_f64)
                        .expect("cairo set source");

                    for rect in cr
                        .copy_clip_rectangle_list()
                        .expect("clip rectangle")
                        .iter()
                    {
                        let cairo_rect = Rect::new(
                            Point::new(rect.x() as _, rect.y() as _),
                            Size::new(rect.width() as _, rect.height() as _),
                        );
                        if let Some(isect) = image_rect.intersection(&cairo_rect) {
                            cr.rectangle(
                                isect.origin.x as _,
                                isect.origin.y as _,
                                isect.size.width as _,
                                isect.size.height as _,
                            );
                        }
                    }
                    cr.fill().expect("cairo fill");
                });
            }
        ));

        // Create overlay layout with revealer for all non-macOS modes
        #[cfg(not(target_os = "macos"))]
        let window_child = create_overlay_layout(&drawing_area);

        #[cfg(target_os = "macos")]
        let window_child = drawing_area.clone();

        let gtk_window = gtk::ApplicationWindow::builder()
            .resizable(true)
            .title("ApertureC Client")
            .visible(false)
            .can_focus(true)
            .child(&window_child)
            .build();
        gtk_window.set_default_size(size.width as _, size.height as _);

        gtk_window.connect_close_request(move |_| {
            ui_event_tx.send_or_warn(UiEvent::WindowClosed);
            glib::signal::Propagation::Proceed
        });

        Window {
            gtk_window,
            drawing_area,
            image,
        }
    }

    fn show(&mut self) {
        self.gtk_window.show();
    }

    fn hide(&mut self) {
        self.gtk_window.hide();
    }

    fn fullscreen(&mut self) {
        self.gtk_window.fullscreen();
    }

    fn window(&mut self) {
        self.gtk_window.unfullscreen();
    }

    fn fullscreen_on_monitor(&mut self, gdk_monitor: gdk::Monitor) {
        self.gtk_window.fullscreen_on_monitor(&gdk_monitor);
    }
}

enum UiEvent {
    AppActivated(gio::ApplicationHoldGuard),
    WindowClosed,
    WindowResized(Size),
    SetWindowMode(WindowMode),
    Refresh,
    UserConfirmedClose,
    MonitorsChanged,
}

#[derive(Debug, Clone, Copy, AsRefStr, EnumIter)]
enum Action {
    Refresh,
    Disconnect,
    Window,
    SingleMonitorFullscreen,
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    MultiMonitorFullscreen,
}

impl Action {
    fn name(&self) -> &str {
        self.as_ref()
    }

    const fn label(&self) -> &'static str {
        match self {
            Action::Refresh => "Refresh",
            Action::Disconnect => "Disconnect",
            Action::Window => "Windowed Mode",
            Action::SingleMonitorFullscreen => "Single Monitor Fullscreen Mode",
            #[cfg(not(any(target_os = "macos", target_os = "windows")))]
            Action::MultiMonitorFullscreen => "Multi-Monitor Fullscreen Mode",
        }
    }

    const fn shortcuts(&self) -> &'static [&'static str] {
        #[cfg(target_os = "macos")]
        match self {
            Action::Refresh => &["<Ctrl><Primary>F5"],
            Action::Disconnect => &["<Ctrl><Primary>F12"],
            Action::Window => &["<Ctrl><Primary>W"],
            Action::SingleMonitorFullscreen => &["<Ctrl><Primary>F"],
        }
        #[cfg(not(target_os = "macos"))]
        match self {
            Action::Refresh => &["<Ctrl><Alt>F5", "<Ctrl><Alt><Shift>F5"],
            Action::Disconnect => &["<Ctrl><Alt>F12", "<Ctrl><Alt><Shift>F12"],
            Action::Window => &["<Ctrl><Alt>W", "<Ctrl><Alt><Shift>W"],
            Action::SingleMonitorFullscreen => &["<Ctrl><Alt>Return"],
            #[cfg(not(target_os = "windows"))]
            Action::MultiMonitorFullscreen => &["<Ctrl><Alt><Shift>Return"],
        }
    }

    fn qualified_name(&self) -> String {
        format!("app.{}", self.name())
    }
}

struct ActionManager {
    app: gtk::Application,
    ui_event_tx: Sender<UiEvent>,
}

impl ActionManager {
    fn new(app: gtk::Application, ui_event_tx: Sender<UiEvent>) -> Self {
        Self { app, ui_event_tx }
    }

    /// Register all application actions with the GAction system
    fn register_all_actions(&self) {
        for action in Action::iter() {
            let simple_action = gio::SimpleAction::new(action.name(), None);
            let ui_event_tx = self.ui_event_tx.clone();

            simple_action.connect_activate(move |_, _| {
                let event = match action {
                    Action::Disconnect => UiEvent::WindowClosed,
                    Action::Refresh => UiEvent::Refresh,
                    Action::Window => {
                        UiEvent::SetWindowMode(WindowMode::Single { fullscreen: false })
                    }
                    Action::SingleMonitorFullscreen => {
                        UiEvent::SetWindowMode(WindowMode::Single { fullscreen: true })
                    }
                    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
                    Action::MultiMonitorFullscreen => UiEvent::SetWindowMode(WindowMode::Multi),
                };

                ui_event_tx.send_or_warn(event);
            });

            self.app.add_action(&simple_action);
        }
    }

    /// Register global accelerators for all actions
    fn register_accelerators(&self) {
        for action in Action::iter() {
            self.app
                .set_accels_for_action(&action.qualified_name(), action.shortcuts());
        }
    }
}

fn create_application_menu() -> gio::Menu {
    let menu = gio::Menu::new();

    let file_menu = gio::Menu::new();
    file_menu.append(
        Some(Action::Disconnect.label()),
        Some(&Action::Disconnect.qualified_name()),
    );
    menu.append_submenu(Some("File"), &file_menu);

    let view_menu = gio::Menu::new();
    view_menu.append(
        Some(Action::Refresh.label()),
        Some(&Action::Refresh.qualified_name()),
    );
    view_menu.append(
        Some(Action::Window.label()),
        Some(&Action::Window.qualified_name()),
    );
    view_menu.append(
        Some(Action::SingleMonitorFullscreen.label()),
        Some(&Action::SingleMonitorFullscreen.qualified_name()),
    );
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    view_menu.append(
        Some(Action::MultiMonitorFullscreen.label()),
        Some(&Action::MultiMonitorFullscreen.qualified_name()),
    );

    menu.append_submenu(Some("View"), &view_menu);
    menu
}

/// Setup native macOS menu bar
#[cfg(target_os = "macos")]
fn setup_native_menu_bar(app: &gtk::Application) {
    let menu = create_application_menu();
    app.set_menubar(Some(&menu));
}

/// Create overlay layout with menu revealer and toggle arrow (for all non-macOS modes)
#[cfg(not(target_os = "macos"))]
fn create_overlay_layout(drawing_area: &gtk::DrawingArea) -> gtk::Widget {
    // Drawing area is the main child; menu + toggle are overlays
    let overlay = gtk::Overlay::new();
    drawing_area.set_hexpand(true);
    drawing_area.set_vexpand(true);
    drawing_area.set_halign(gtk::Align::Fill);
    drawing_area.set_valign(gtk::Align::Fill);
    overlay.set_child(Some(drawing_area));

    // Menu bar inside a revealer
    let menu_revealer = gtk::Revealer::new();
    let menu = create_application_menu();
    let menu_bar = gtk::PopoverMenuBar::from_model(Some(&menu));
    menu_bar.set_hexpand(true);
    menu_bar.set_halign(gtk::Align::Fill);
    menu_revealer.set_child(Some(&menu_bar));
    menu_revealer.set_reveal_child(false); // Hidden by default
    menu_revealer.set_transition_type(gtk::RevealerTransitionType::SlideDown);
    menu_revealer.set_transition_duration(250);
    menu_revealer.set_halign(gtk::Align::Fill);
    menu_revealer.set_valign(gtk::Align::Start);
    menu_revealer.set_can_target(false); // Pass through events when not revealed

    // Show/hide buttons as separate overlay children
    let show_button = gtk::Button::with_label("▼");
    show_button.set_halign(gtk::Align::Center);
    show_button.set_valign(gtk::Align::Start);
    show_button.set_margin_top(2);
    show_button.set_margin_bottom(2);
    show_button.set_can_focus(false);
    show_button.add_css_class("arrow-button");

    let hide_button = gtk::Button::with_label("▲");
    hide_button.set_halign(gtk::Align::Center);
    hide_button.set_valign(gtk::Align::Start);
    hide_button.set_margin_top(2);
    hide_button.set_margin_bottom(2);
    hide_button.set_can_focus(false);
    hide_button.add_css_class("arrow-button");
    hide_button.set_visible(false);
    hide_button.set_can_target(false);

    // Handlers to swap menu visibility and buttons
    // Overlay menubar and toggles as separate overlays so they don't affect drawing area size
    menu_revealer.set_valign(gtk::Align::Start);
    overlay.add_overlay(&menu_revealer);

    let show_overlay = gtk::Box::new(gtk::Orientation::Vertical, 0);
    show_overlay.set_halign(gtk::Align::Center);
    show_overlay.set_valign(gtk::Align::Start);
    show_overlay.set_can_target(true);
    show_overlay.set_sensitive(true);
    show_overlay.append(&show_button);
    overlay.add_overlay(&show_overlay);

    // Hide button overlay: position below the menubar by applying a top margin equal to menu height
    let hide_overlay = gtk::Box::new(gtk::Orientation::Vertical, 0);
    hide_overlay.set_halign(gtk::Align::Center);
    hide_overlay.set_valign(gtk::Align::Start);
    hide_overlay.set_can_target(false);
    hide_overlay.set_sensitive(true);
    hide_overlay.set_visible(false);
    hide_overlay.append(&hide_button);
    overlay.add_overlay(&hide_overlay);

    show_button.connect_clicked(glib::clone!(
        #[weak]
        menu_revealer,
        #[weak]
        menu_bar,
        #[weak]
        show_button,
        #[weak]
        hide_button,
        #[weak]
        show_overlay,
        #[weak]
        hide_overlay,
        move |_| {
            menu_revealer.set_reveal_child(true);
            menu_revealer.set_can_target(true);

            let (_, nat, _, _) = menu_bar.measure(gtk::Orientation::Vertical, -1);
            hide_overlay.set_margin_top(nat + 2);

            show_button.set_visible(false);
            show_button.set_can_target(false);
            show_overlay.set_can_target(false);
            show_overlay.set_visible(false);

            hide_button.set_visible(true);
            hide_button.set_can_target(true);
            hide_overlay.set_visible(true);
            hide_overlay.set_can_target(true);
        }
    ));

    hide_button.connect_clicked(glib::clone!(
        #[weak]
        menu_revealer,
        #[weak]
        show_button,
        #[weak]
        hide_button,
        #[weak]
        show_overlay,
        #[weak]
        hide_overlay,
        move |_| {
            menu_revealer.set_reveal_child(false);
            menu_revealer.set_can_target(false);

            hide_overlay.set_margin_top(0);

            hide_button.set_visible(false);
            hide_button.set_can_target(false);
            hide_overlay.set_visible(false);
            hide_overlay.set_can_target(false);

            show_button.set_visible(true);
            show_button.set_can_target(true);
            show_overlay.set_visible(true);
            show_overlay.set_can_target(true);
        }
    ));

    // Hover cursor effect
    let motion_show = gtk::EventControllerMotion::new();
    let show_clone = show_button.clone();
    motion_show.connect_enter(move |_, _, _| {
        show_clone.set_cursor_from_name(Some("pointer"));
    });
    show_button.add_controller(motion_show);

    let motion_hide = gtk::EventControllerMotion::new();
    let hide_clone = hide_button.clone();
    motion_hide.connect_enter(move |_, _, _| {
        hide_clone.set_cursor_from_name(Some("pointer"));
    });
    hide_button.add_controller(motion_hide);

    overlay.upcast()
}

pub fn main() -> anyhow::Result<()> {
    let args = aperturec_client::args::Args::parse();
    let (log_layer, _guard) = args.log.as_tracing_layer()?;
    tracing_subscriber::registry().with(log_layer).init();

    info!("ApertureC Client Startup");

    let metrics_exporters = args.metrics.to_exporters(env!("CARGO_CRATE_NAME"));
    if !metrics_exporters.is_empty() {
        aperturec_client::init_metrics(metrics_exporters).expect("Failed to initialize metrics");
    }

    let monitors = Monitors::current()?;
    let config = config::Configuration::from_args(args)?;
    let initial_displays_requested = match config.initial_display_mode {
        DisplayMode::Windowed { size } => vec![Display {
            area: Rect::new(Point::origin(), size),
            is_enabled: true,
        }],
        DisplayMode::SingleFullscreen => vec![monitors.as_displays().next().unwrap()],
        DisplayMode::MultiFullscreen => monitors.as_displays().collect(),
    };
    let initial_display_mode = config.initial_display_mode;
    let mut client = Client::new(config, current_lock_state()?, &initial_displays_requested);
    let conn = Arc::new(client.connect()?);

    let should_stop = Arc::new(AtomicBool::new(false));
    let app = gtk::Application::builder()
        .application_id("com.zetier.aperturec.client")
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();

    let (ui_event_tx, ui_event_rx) = unbounded();

    // Setup actions and menu bar
    #[cfg(target_os = "macos")]
    {
        let ui_event_tx_clone = ui_event_tx.clone();
        app.connect_startup(move |app| {
            let action_manager = ActionManager::new(app.clone(), ui_event_tx_clone.clone());
            action_manager.register_all_actions();
            action_manager.register_accelerators();
            setup_native_menu_bar(app);
        });
    }
    #[cfg(not(target_os = "macos"))]
    {
        let action_manager = ActionManager::new(app.clone(), ui_event_tx.clone());
        action_manager.register_all_actions();
        action_manager.register_accelerators();

        // Add CSS styling for menu bar and toggle arrow
        let css_provider = gtk::CssProvider::new();
        css_provider.load_from_data(
            "menubar {
              background-color: @theme_bg_color;
              color: @theme_fg_color;
              border-bottom: 1px solid @borders;
            }
            .arrow-button {
              background: rgba(0, 0, 0, 0.6);
              color: #fff;
              border: none;
              box-shadow: none;
              padding: 4px;
              min-width: 20px;
              min-height: 20px;
              border-radius: 4px;
            }
            menubar > item > popover {
              background: transparent;
              border: none;
              box-shadow: none;
              padding: 0;
            }
            menubar > item > popover > contents {
              box-shadow: none;
              padding: 0;
              margin: 0;
            }",
        );

        gtk::style_context_add_provider_for_display(
            &gdk::Display::default().expect("Could not get default display"),
            &css_provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }

    let (server_event_tx, server_event_rx) = unbounded();
    let event_thread = thread::spawn(glib::clone!(
        #[weak]
        conn,
        #[weak]
        should_stop,
        move || {
            let _s = trace_span!("main-server-event-thread").entered();
            const TICK: Duration = Duration::from_millis(100);
            while !should_stop.load(Ordering::Acquire) {
                match conn.wait_event_timeout(TICK) {
                    Ok(Some(event)) => server_event_tx.send_or_warn(event),
                    Ok(None) => continue,
                    Err(error) => {
                        warn!(%error, "failed getting server event");
                        break;
                    }
                };
            }
        }
    ));

    let mut ui = Ui::new(
        conn.clone(),
        initial_display_mode,
        ui_event_tx.clone(),
        app.clone(),
    )?;

    glib::MainContext::default().spawn_local(glib::clone!(
        #[weak]
        app,
        async move {
            let mut server_event_stream = pin!(server_event_rx.fuse());
            let mut ui_event_stream = pin!(ui_event_rx.fuse());
            loop {
                futures::select! {
                    event_opt = server_event_stream.next() => match event_opt {
                        Some(ServerEvent::Draw(draw)) => ui.draw(draw),
                        Some(ServerEvent::CursorChange(cursor)) => ui.change_cursor(cursor),
                        Some(ServerEvent::DisplayChange(dc)) => if let Err(error) = ui.set_display_configuration(dc) {
                            warn!(%error, "changing display configuration");
                        }
                        Some(ServerEvent::Quit(quit_reason)) => {
                            ui.show_exit_reason(quit_reason);
                        },
                        None => continue,
                    },
                    event_opt = ui_event_stream.next() => match event_opt {
                        Some(UiEvent::WindowClosed) => {
                            ui.report_window_closed();
                            break;
                        },
                        Some(UiEvent::AppActivated(_hg)) => {
                            for window in ui.all_windows() {
                                 app.add_window(&window.gtk_window);
                            }
                        },
                        Some(UiEvent::WindowResized(size)) => ui.report_resize(size),
                        Some(UiEvent::SetWindowMode(mode)) => {
                            if let Err(error) = ui.set_window_mode(mode) {
                                warn!(%error, "failed to set window mode");
                            }
                        },
                        Some(UiEvent::MonitorsChanged) => {
                            if let Err(error) = ui.handle_monitors_changed() {
                                warn!(%error, "failed to handle monitor change");
                            }
                        }
                        Some(UiEvent::Refresh) => {
                            if let Err(error) = ui.request_display_config() {
                                warn!(%error, "failed to request display refresh");
                            }
                        }
                        Some(UiEvent::UserConfirmedClose) => break,
                        None => continue,
                    },
                    complete => {
                        info!("event streams completed; exiting UI loop");
                        break;
                    },
                }
            }
            app.quit();
        }
    ));

    app.connect_activate(move |app| ui_event_tx.send_or_warn(UiEvent::AppActivated(app.hold())));
    app.run_with_args(&[""; 0]);

    should_stop.store(true, Ordering::Relaxed);
    if let Err(error) = event_thread.join() {
        error!(?error, "event thread panicked");
    }
    let Some(connection) = Arc::into_inner(conn) else {
        bail!("outlying connection references");
    };
    connection.disconnect();
    Ok(())
}
