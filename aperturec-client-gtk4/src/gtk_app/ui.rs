use super::DEFAULT_RESOLUTION;
use super::actions::Action;
use super::image;
use super::monitors::Monitors;
use super::window::Window;
use anyhow::{Result, bail};
use aperturec_client::{Connection, Cursor, DisplayMode, Draw, QuitReason};
use aperturec_graphics::{
    display::{Display, DisplayConfiguration},
    euclid_collections::EuclidMap,
    prelude::*,
};
use aperturec_utils::channels::SenderExt;
use async_channel::Sender;
use gtk4::{self as gtk, gdk, gio, glib, prelude::*};
use std::{cell::Cell, iter, rc::Rc, sync::Arc, time::Duration};
use tracing::*;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WindowMode {
    Single { fullscreen: bool },
    Multi,
}

#[derive(Debug)]
pub enum UiEvent {
    AppActivated(gio::ApplicationHoldGuard),
    WindowClosed,
    Shutdown,
    #[cfg(target_os = "macos")]
    ShowPreferences,
    WindowFullscreenChanged(bool),
    WindowResized(Size),
    SetWindowMode(WindowMode),
    Refresh,
    UserConfirmedClose,
    MonitorsChanged,
}

fn derive_window_mode(
    current_mode: WindowMode,
    display_config: &DisplayConfiguration,
    monitors_match: bool,
) -> Result<WindowMode> {
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
            WindowMode::Single { fullscreen: true } => Ok(WindowMode::Single { fullscreen: true }),
            WindowMode::Single { fullscreen: false } => {
                Ok(WindowMode::Single { fullscreen: false })
            }
            WindowMode::Multi => {
                warn!("Server provided single-monitor display configuration in multi-monitor mode");
                info!("Reverting to single-monitor mode");
                Ok(WindowMode::Single { fullscreen: true })
            }
        }
    } else {
        if matches!(current_mode, WindowMode::Single { .. }) {
            bail!("Server provided multi-monitor display configuration in single-window mode");
        }
        if !monitors_match {
            bail!(
                "Server provided multi-monitor initial display configuration which does not match monitors geometry on client"
            );
        }
        Ok(WindowMode::Multi)
    }
}

impl WindowMode {
    fn derive_next<T: Into<WindowMode>>(
        current_mode: T,
        display_config: &DisplayConfiguration,
        monitors: &Monitors,
    ) -> Result<Self> {
        derive_window_mode(
            current_mode.into(),
            display_config,
            monitors.matches(display_config),
        )
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

pub struct Ui {
    conn: Arc<Connection>,
    mm_windows: EuclidMap<Window>,
    single_window: Window,
    server_can_mm: bool,
    hotplug_disabled_mm: bool,
    window_mode: WindowMode,
    pending_window_mode: Option<WindowMode>,
    resize_timeout: Rc<Cell<Option<glib::SourceId>>>,
    display_config_id: usize,
    monitors: Monitors,
    app: glib::WeakRef<gtk::Application>,
    ui_event_tx: Sender<UiEvent>,
}

impl Ui {
    pub fn new(
        conn: Arc<Connection>,
        initial_display_mode: DisplayMode,
        ui_event_tx: Sender<UiEvent>,
        app: gtk::Application,
    ) -> Result<Self> {
        debug!(?initial_display_mode, "ui init");
        let initial_display_config = conn.display_configuration();
        debug!(
            id = initial_display_config.id,
            count = initial_display_config.display_decoder_infos.len(),
            "initial display configuration"
        );

        let monitors = Monitors::current()?;
        let initial_window_mode =
            WindowMode::derive_next(initial_display_mode, &initial_display_config, &monitors)?;
        debug!(?initial_window_mode, "initial window mode derived");

        // Notify UI when monitors are hot-plugged or removed so we can fall back
        // to a safe mode for the remainder of the session.
        if let Some(display) = gdk::Display::default() {
            let monitors_model = display.monitors();
            monitors_model.connect_items_changed(glib::clone!(
                #[strong(rename_to = tx)]
                ui_event_tx,
                move |_, _, _, _| {
                    trace!("monitor hotplug detected");
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
        debug!(server_can_mm, "server multi-monitor capability");

        let single_window = Window::new(
            conn.clone(),
            initial_windowed_size,
            Point::zero(),
            ui_event_tx.clone(),
        );
        single_window.drawing_area.connect_resize(glib::clone!(
            #[strong(rename_to = tx)]
            ui_event_tx,
            move |_, width, height| {
                tx.send_or_warn(UiEvent::WindowResized(Size::new(width as _, height as _)))
            }
        ));
        single_window
            .gtk_window
            .connect_fullscreened_notify(glib::clone!(
                #[strong(rename_to = tx)]
                ui_event_tx,
                move |window| {
                    tx.send_or_warn(UiEvent::WindowFullscreenChanged(window.is_fullscreen()));
                }
            ));

        let mm_windows = monitors
            .usable_areas()
            .map(|area| {
                (
                    area,
                    Window::new(conn.clone(), area.size, area.origin, ui_event_tx.clone()),
                )
            })
            .collect();

        let ui = Ui {
            conn,
            mm_windows,
            single_window,
            server_can_mm,
            hotplug_disabled_mm: false,
            window_mode: initial_window_mode,
            pending_window_mode: Some(initial_window_mode),
            resize_timeout: Rc::default(),
            display_config_id: initial_display_config.id,
            monitors,
            app: app.downgrade(),
            ui_event_tx,
        };

        ui.update_display_mode_actions();
        Ok(ui)
    }

    pub fn activate(&mut self) -> Result<()> {
        debug!("ui activate");
        self.ensure_windows_added();
        if let Some(mode) = self.pending_window_mode.take() {
            debug!(?mode, "applying pending window mode");
            self.set_window_mode(mode)?;
        }
        Ok(())
    }

    fn ensure_windows_added(&self) {
        let Some(app) = self.app.upgrade() else {
            warn!("gtk application dropped before windows added");
            return;
        };
        for window in self.all_windows() {
            if window.gtk_window.application().is_none() {
                trace!("adding window to app");
                app.add_window(&window.gtk_window);
            }
        }
    }

    pub fn report_resize(&mut self, size: Size) {
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

    pub fn report_window_closed(&self) {
        debug!("reported window closed");
        for window in self.all_windows() {
            window.gtk_window.close();
            if let Some(app) = window.gtk_window.application() {
                app.remove_window(&window.gtk_window);
            }
        }
    }

    pub fn set_window_mode(&mut self, window_mode: WindowMode) -> Result<()> {
        debug!(?window_mode, "set window mode");
        self.ensure_windows_added();
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
        debug!(?window_mode, "set window mode from server");
        self.ensure_windows_added();
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

    pub fn handle_monitors_changed(&mut self) -> Result<()> {
        debug!("handling monitor change");
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

        if matches!(self.window_mode, WindowMode::Multi)
            && let Err(error) = self.set_window_mode(WindowMode::Single { fullscreen: true })
        {
            warn!(%error, "failed to fall back to single-monitor fullscreen");
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
        let Some(app) = self.app.upgrade() else {
            trace!("app dropped; skipping action updates");
            return;
        };

        // Update Window action
        if let Some(action) = app.lookup_action(Action::Window.as_ref())
            && let Some(simple_action) = action.downcast_ref::<gio::SimpleAction>()
        {
            let enabled = !matches!(self.window_mode, WindowMode::Single { fullscreen: false });
            simple_action.set_enabled(enabled);
        }

        // Update Single Monitor Fullscreen action
        if let Some(action) = app.lookup_action(Action::SingleMonitorFullscreen.as_ref())
            && let Some(simple_action) = action.downcast_ref::<gio::SimpleAction>()
        {
            let enabled = !matches!(self.window_mode, WindowMode::Single { fullscreen: true });
            simple_action.set_enabled(enabled);
        }

        // Update Multi-Monitor Fullscreen action (Linux only)
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        if let Some(action) = app.lookup_action(Action::MultiMonitorFullscreen.as_ref())
            && let Some(simple_action) = action.downcast_ref::<gio::SimpleAction>()
        {
            let enabled = self.server_can_mm
                && !self.hotplug_disabled_mm
                && !matches!(self.window_mode, WindowMode::Multi);
            simple_action.set_enabled(enabled);
        }
    }

    pub fn request_display_config(&self) -> Result<()> {
        debug!(?self.window_mode, "request display configuration");
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

    pub fn draw(&mut self, draw: Draw) {
        trace!(origin = ?draw.origin, frame = draw.frame, "draw");
        if let Some((win_area, win)) = self.window_at(draw.origin) {
            win.image.borrow_mut().draw(
                draw.pixels.view(),
                (draw.origin - win_area.origin).to_point(),
                draw.frame,
            );
            win.drawing_area.queue_draw();
        }
    }

    pub fn change_cursor(&mut self, cursor: Cursor) {
        trace!(
            width = cursor.pixels.shape()[0],
            height = cursor.pixels.shape()[1],
            hot = ?cursor.hot,
            "change cursor"
        );
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
            width * std::mem::size_of::<Pixel32>(),
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

    pub fn set_display_configuration(
        &mut self,
        display_config: DisplayConfiguration,
    ) -> Result<()> {
        debug!(
            id = display_config.id,
            count = display_config.display_decoder_infos.len(),
            "set display configuration"
        );
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
        debug!(%primary_text, %secondary_text, ?message_type, "show modal");
        let mut dialog_builder = gtk::MessageDialog::builder()
            .modal(true)
            .message_type(message_type)
            .buttons(gtk::ButtonsType::Close)
            .text(primary_text)
            .secondary_text(secondary_text)
            .hide_on_close(true);
        if let Some(app) = self.app.upgrade()
            && let Some(parent) = app.active_window()
        {
            dialog_builder = dialog_builder.transient_for(&parent);
        }
        let dialog = dialog_builder.build();
        dialog.connect_response(move |dialog, response| {
            on_response(dialog, response);
            dialog.close();
        });
        dialog.show();
    }

    pub fn show_exit_reason(&self, reason: QuitReason) {
        debug!(?reason, "show exit reason");
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

    #[cfg(target_os = "macos")]
    pub fn show_preferences(&self) {
        self.show_modal(
            "Preferences",
            "ApertureC Client does not expose a preferences window. Configure it via the CLI or aperturec:// URI parameters.",
            gtk::MessageType::Info,
        );
    }

    pub fn handle_fullscreen_changed(&mut self, fullscreen: bool) {
        let WindowMode::Single {
            fullscreen: current,
        } = self.window_mode
        else {
            trace!("fullscreen change ignored in multi-monitor mode");
            return;
        };
        if current == fullscreen {
            return;
        }
        self.window_mode = WindowMode::Single { fullscreen };
        self.update_display_mode_actions();
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

#[cfg(test)]
mod tests {
    use super::*;
    use aperturec_graphics::display::{Display, DisplayDecoderInfo};

    fn display_config_with_enabled(enabled: &[bool]) -> DisplayConfiguration {
        let infos: Vec<DisplayDecoderInfo> = enabled
            .iter()
            .enumerate()
            .map(|(idx, is_enabled)| DisplayDecoderInfo {
                display: Display {
                    area: Rect::new(Point::new(idx * 100, 0), Size::new(100, 100)),
                    is_enabled: *is_enabled,
                },
                decoder_areas: Box::new([]),
            })
            .collect();

        DisplayConfiguration {
            id: 1,
            display_decoder_infos: infos.into_boxed_slice(),
        }
    }

    #[test]
    fn window_mode_from_display_mode() {
        assert_eq!(
            WindowMode::from(DisplayMode::Windowed {
                size: Size::new(640, 480)
            }),
            WindowMode::Single { fullscreen: false }
        );
        assert_eq!(
            WindowMode::from(DisplayMode::SingleFullscreen),
            WindowMode::Single { fullscreen: true }
        );
        assert_eq!(
            WindowMode::from(DisplayMode::MultiFullscreen),
            WindowMode::Multi
        );
    }

    #[test]
    fn derive_window_mode_errors_when_no_enabled_displays() {
        let config = display_config_with_enabled(&[false, false]);
        let result = derive_window_mode(WindowMode::Single { fullscreen: true }, &config, true);
        assert!(result.is_err());
    }

    #[test]
    fn derive_window_mode_single_display_preserves_mode() {
        let config = display_config_with_enabled(&[true]);
        let fullscreen =
            derive_window_mode(WindowMode::Single { fullscreen: true }, &config, true).unwrap();
        assert_eq!(fullscreen, WindowMode::Single { fullscreen: true });

        let windowed =
            derive_window_mode(WindowMode::Single { fullscreen: false }, &config, false).unwrap();
        assert_eq!(windowed, WindowMode::Single { fullscreen: false });
    }

    #[test]
    fn derive_window_mode_multi_to_single_returns_fullscreen() {
        let config = display_config_with_enabled(&[true]);
        let next = derive_window_mode(WindowMode::Multi, &config, true).unwrap();
        assert_eq!(next, WindowMode::Single { fullscreen: true });
    }

    #[test]
    fn derive_window_mode_rejects_multi_when_current_single() {
        let config = display_config_with_enabled(&[true, true]);
        let result = derive_window_mode(WindowMode::Single { fullscreen: false }, &config, true);
        assert!(result.is_err());
    }

    #[test]
    fn derive_window_mode_rejects_when_monitors_mismatch() {
        let config = display_config_with_enabled(&[true, true]);
        let result = derive_window_mode(WindowMode::Multi, &config, false);
        assert!(result.is_err());
    }

    #[test]
    fn derive_window_mode_accepts_multi_when_monitors_match() {
        let config = display_config_with_enabled(&[true, true]);
        let result = derive_window_mode(WindowMode::Multi, &config, true).unwrap();
        assert_eq!(result, WindowMode::Multi);
    }
}
