use crate::client::{
    CursorData, DisplayEventMessage, DisplayMode, EventMessage, KeyEventMessageBuilder,
    MonitorGeometry, MonitorsGeometry, MouseButtonEventMessageBuilder, PointerEventMessageBuilder,
    UiMessage,
};
use crate::frame::Draw;
use crate::gtk3::image::Image;
use crate::metrics::RefreshCount;

use aperturec_graphics::{display::*, euclid_collections::*, prelude::*};

use anyhow::{Result, anyhow, bail};
use crossbeam::channel::{Receiver, Sender, unbounded};
use gtk::{cairo, gdk, gdk_pixbuf, gio, glib, prelude::*};
use keycode::{KeyMap, KeyMapping, KeyMappingId};
use ndarray::prelude::*;
use std::cell::{Cell, RefCell};
use std::collections::{BTreeSet, HashMap};
use std::iter;
use std::rc::Rc;
use std::sync::LazyLock;
use std::time::Duration;
use tracing::*;

#[cfg(target_os = "windows")]
use windows::Win32::UI::Input::KeyboardAndMouse::{
    MAPVK_VK_TO_VSC_EX,
    MapVirtualKeyW,
    VIRTUAL_KEY,
    VK_APPS, // Windows / context menu keys
    VK_DELETE,
    VK_DOWN,
    VK_END,
    VK_HOME,
    VK_INSERT,
    VK_LEFT,
    VK_LWIN,
    VK_NEXT, // page-up / page-down
    VK_PRIOR,
    VK_RCONTROL,
    VK_RIGHT,
    VK_RMENU, // right Ctrl / right Alt
    VK_RWIN,
    VK_UP,
};

#[cfg(target_os = "windows")]
mod win_key_hook {
    use super::{EventMessage, KeyEventMessageBuilder, convert_win_virtual_key_to_scan_code, warn};
    use crossbeam::channel::Sender;
    use keycode::{KeyMap, KeyMapping};
    use std::sync::OnceLock;
    use windows::Win32::{
        Foundation::{LPARAM, LRESULT, WPARAM},
        UI::{
            Input::KeyboardAndMouse::{
                GetAsyncKeyState, VK_CONTROL, VK_ESCAPE, VK_LWIN, VK_RWIN, VK_TAB,
            },
            WindowsAndMessaging::{
                CallNextHookEx, HHOOK, KBDLLHOOKSTRUCT, LLKHF_ALTDOWN, SetWindowsHookExW,
                WH_KEYBOARD_LL, WM_KEYDOWN, WM_SYSKEYDOWN,
            },
        },
    };

    /*  GLOBALS ------------------------------------------------------------------------------- */

    // Hook handle kept for the life-time of the process so the hook remains active.
    static mut HOOK: Option<HHOOK> = None;

    // Cross-thread channel used to forward the synthesised key-events back to the GTK thread.
    // Stored in a OnceLock so it can be read inside the C-ABI hook callback.
    static EVENT_TX: OnceLock<Sender<EventMessage>> = OnceLock::new();

    /*  PUBLIC API ---------------------------------------------------------------------------- */

    /// Installs a process-wide low-level keyboard hook that swallows the Win keys whenever
    /// one of this process’ windows is the foreground window.  The swallowed key press /
    /// release events are re-injected into the application by sending them through the
    /// existing `EventMessage` channel, guaranteeing that the rest of the input handling
    /// code continues to work unchanged.
    #[allow(static_mut_refs)]
    pub fn install(tx: Sender<EventMessage>) {
        // Store the sender for use in the callback.
        let _ = EVENT_TX.set(tx);

        unsafe {
            if HOOK.is_some() {
                return;
            }

            extern "system" fn hook_proc(n_code: i32, w_param: WPARAM, l_param: LPARAM) -> LRESULT {
                if n_code >= 0 {
                    // SAFETY: guaranteed by hook contract.
                    let kb: &KBDLLHOOKSTRUCT = unsafe { &*(l_param.0 as *const KBDLLHOOKSTRUCT) };
                    let vk = kb.vkCode as u32;

                    // --- determine whether this event must be swallowed -------------
                    let alt_down = kb.flags.contains(LLKHF_ALTDOWN);
                    // high-order bit set ⇒ key currently down
                    let ctrl_down = unsafe { GetAsyncKeyState(VK_CONTROL.0 as i32) } < 0;

                    let swallow = match vk {
                        k if k == VK_LWIN.0 as u32 || k == VK_RWIN.0 as u32 => true,
                        k if k == VK_TAB.0 as u32 && alt_down => true, // Alt+Tab / Shift+Alt+Tab
                        k if k == VK_ESCAPE.0 as u32 && (alt_down || ctrl_down) => true, // Alt+Esc / Ctrl+Esc
                        _ => false,
                    };

                    if swallow {
                        // Is this a press or a release?
                        let pressed = matches!(w_param.0 as u32, WM_KEYDOWN | WM_SYSKEYDOWN);

                        // Translate to X11 keycode (same logic used elsewhere)
                        if let Ok(scan) = convert_win_virtual_key_to_scan_code(vk) {
                            if let (Some(sender), Some(code)) = (
                                EVENT_TX.get(),
                                KeyMap::try_from(KeyMapping::Win(scan as u16))
                                    .map(|m| m.xkb)
                                    .ok(),
                            ) {
                                sender
                                    .send(EventMessage::KeyEventMessage(
                                        KeyEventMessageBuilder::default()
                                            .key(code.into())
                                            .is_pressed(pressed)
                                            .build()
                                            .expect("build KeyEventMessage"),
                                    ))
                                    .unwrap_or_else(|err| warn!(%err, "win_key_hook failed to tx KeyEventMessage"));
                            }
                        }
                        // prevent local OS reaction
                        return LRESULT(1);
                    }
                }
                // Not handled – pass on
                unsafe { CallNextHookEx(None, n_code, w_param, l_param) }
            }

            let hook = SetWindowsHookExW(
                WH_KEYBOARD_LL,
                Some(hook_proc),
                None, // no module handle needed for same-process hook proc
                0,
            )
            .expect("set hook");

            HOOK = Some(hook);
        }
    }
}

static GTK_INIT: LazyLock<()> = LazyLock::new(|| {
    gdk::set_allowed_backends("x11,*");
    gtk::init().expect("Failed to initialize GTK");
});

pub mod image;

pub const DEFAULT_RESOLUTION: Size = Size::new(800, 600);

pub struct ClientSideItcChannels {
    pub ui_tx: glib::Sender<UiMessage>,
    pub notify_event_rx: Receiver<EventMessage>,
    pub notify_event_tx: Sender<EventMessage>,
    pub notify_media_rx: Receiver<DisplayConfiguration>,
    pub notify_media_tx: Sender<DisplayConfiguration>,
}

pub struct GtkSideItcChannels {
    ui_rx: glib::Receiver<UiMessage>,
    notify_event_tx: Sender<EventMessage>,
}

pub struct ItcChannels {
    pub client_half: ClientSideItcChannels,
    pub gtk_half: GtkSideItcChannels,
}

impl Default for ItcChannels {
    fn default() -> Self {
        ItcChannels::new()
    }
}

impl ItcChannels {
    pub fn new() -> Self {
        let (ui_tx, ui_rx) = glib::MainContext::channel(glib::Priority::default());

        let (notify_event_tx, notify_event_rx) = unbounded();
        let (notify_media_tx, notify_media_rx) = unbounded();

        ItcChannels {
            client_half: ClientSideItcChannels {
                ui_tx,
                notify_event_rx,
                notify_event_tx: notify_event_tx.clone(),
                notify_media_rx,
                notify_media_tx: notify_media_tx.clone(),
            },
            gtk_half: GtkSideItcChannels {
                ui_rx,
                notify_event_tx,
            },
        }
    }
}

#[derive(PartialEq, Clone, Copy, Debug)]
pub struct LockState {
    pub is_caps_locked: bool,
    pub is_num_locked: bool,
    pub is_scroll_locked: bool,
}

impl LockState {
    pub fn get_current() -> Self {
        *GTK_INIT;
        let display = gdk::Display::default().expect("Failed to get default GTK display");
        match gdk::Keymap::for_display(&display) {
            None => panic!("can't get keymap"),
            Some(keymap) => Self {
                is_caps_locked: keymap.is_caps_locked(),
                is_num_locked: keymap.is_num_locked(),
                is_scroll_locked: keymap.is_scroll_locked(),
            },
        }
    }
}

/// Generate a `MonitorsGeometry` using `gdk`'s API. The returned `MonitorsGeometry` is guaranteed to
/// have a display rooted at the origin and only contain monitors which are non-overlapping and
/// contiguous.
pub fn get_monitors_geometry() -> Result<MonitorsGeometry> {
    *GTK_INIT;

    fn gdkrect2rect(geo: gdk::Rectangle) -> Rect {
        Rect::new(
            Point::new(geo.x() as _, geo.y() as _),
            Size::new(geo.width() as _, geo.height() as _),
        )
    }

    let display = gdk::Display::default().ok_or(anyhow!("Failed to get default GTK display"))?;
    let num_monitors = display.n_monitors();
    if num_monitors <= 0 {
        bail!("No GTK monitors found for display {:?}", display);
    }

    let mut monitors = EuclidMap::new();
    for i in 0..num_monitors {
        let mon = display.monitor(i).ok_or(anyhow!("missing monitor {}", i))?;
        let total_area = gdkrect2rect(mon.geometry());
        let usable_area = gdkrect2rect(mon.workarea());
        if !total_area.contains_rect(&usable_area) {
            bail!("usable area exceeds total area of monitor");
        }

        let overlaps = monitors.insert(total_area, usable_area);
        if !overlaps.is_empty() {
            bail!(
                "at least two monitors overlap: {:?} and {:?}",
                total_area,
                overlaps
            );
        }
    }

    if monitors.is_empty() {
        bail!("no monitors");
    }

    Ok(MonitorsGeometry { monitors })
}

#[derive(Debug, Clone, Copy)]
enum KeyboardShortcut {
    ToggleFullscreen { multi_monitor: bool },
    Refresh,
    Disconnect,
}

impl KeyboardShortcut {
    fn from_event(key_event: &gdk::EventKey) -> Option<Self> {
        let state = key_event.state() & gdk::ModifierType::MODIFIER_MASK;

        if !state.contains(gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::MOD1_MASK) {
            None
        } else if key_event.keyval() == gdk::keys::constants::Return {
            if state.contains(gdk::ModifierType::SHIFT_MASK) {
                Some(Self::ToggleFullscreen {
                    multi_monitor: false,
                })
            } else {
                Some(Self::ToggleFullscreen {
                    multi_monitor: true,
                })
            }
        // Some DEs will switch TTYs with Ctrl-Alt-F#. These DE's may allow these key-combos to
        // propagate to the client (wihout a TTY switch) by using Shift (eg. Ctrl-Alt-Shift-F12)
        } else if key_event.keyval() == gdk::keys::constants::F12 {
            Some(Self::Disconnect)
        } else if key_event.keyval() == gdk::keys::constants::F5 {
            Some(Self::Refresh)
        } else {
            None
        }
    }
}

// Converts a Windows virtual key code to the OEM scan code using the native Windows API.
// Reason driving this function (and using the unsafe Windows API) is that when working with GTK,
// the keycode associated with the EventKey on Windows ends up being the virtual key code
// (see https://learn.microsoft.com/en-us/windows/win32/inputdev/virtual-key-codes) whereas
// the `keycode` crate that we use for mapping keycodes to Xkb expects the Windows OEM scan code.
//
// # Arguments
//
// * `virtual_key` - The virtual key code to convert.
//
// # Returns
//
// Returns the scan code corresponding to the virtual key code.
#[cfg(target_os = "windows")]
fn convert_win_virtual_key_to_scan_code(virtual_key: u32) -> anyhow::Result<u16> {
    // SAFETY: calling a pure Win32 API with value parameters only.
    let raw = unsafe { MapVirtualKeyW(virtual_key, MAPVK_VK_TO_VSC_EX) };
    if raw == 0 {
        anyhow::bail!(
            "Failed to convert virtual key code {:#X} to scan code",
            virtual_key
        );
    }

    // Low byte is the scan-code; MapVirtualKey sets bit-8 for some (not all)
    // extended keys.  We additionally mark known extended virtual keys
    // ourselves so that every extended key is encoded as 0xE0XX.
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
        sc |= 0xE000; // apply 0xE0 prefix
    }

    Ok(sc)
}

fn gtk_key_to_x11(key: &gdk::EventKey) -> u16 {
    let keycode = key.keycode().expect("Failed to get keycode!");
    let mapping = {
        #[cfg(target_os = "macos")]
        {
            KeyMapping::Mac(keycode)
        }

        #[cfg(target_os = "windows")]
        {
            let scancode = convert_win_virtual_key_to_scan_code(keycode as u32)
                .unwrap_or_else(|err| panic!("{}", err));
            KeyMapping::Win(scancode)
        }

        #[cfg(not(any(target_os = "windows", target_os = "macos")))]
        {
            KeyMapping::Xkb(keycode)
        }
    };

    let map = KeyMap::try_from(mapping).expect("Could not convert native key code");
    map.xkb
}

pub struct GtkUi {
    app: Rc<gtk::Application>,
    keyboard_shortcut_rx: glib::Receiver<KeyboardShortcut>,
    ui_rx: glib::Receiver<UiMessage>,
    workspace: Rc<UiWorkspace>,
    windows: Rc<RefCell<Windows>>,
}

impl GtkUi {
    pub fn new(
        itc: GtkSideItcChannels,
        initial_display_mode: DisplayMode,
        initial_monitors_geometry: MonitorsGeometry,
        initial_display_config: DisplayConfiguration,
    ) -> Result<Self> {
        let app = Rc::new(
            gtk::Application::builder()
                .application_id("com.zetier.aperturec.client")
                .flags(gio::ApplicationFlags::NON_UNIQUE)
                .build(),
        );

        let (keyboard_shortcut_tx, keyboard_shortcut_rx) =
            glib::MainContext::channel(glib::Priority::default());

        let workspace = Rc::new(UiWorkspace::new(
            &itc,
            keyboard_shortcut_tx,
            initial_monitors_geometry,
        )?);
        let windows = Rc::new(RefCell::new(Windows::new(
            app.clone(),
            workspace.clone(),
            initial_display_mode,
            &initial_display_config,
        )?));

        Ok(Self {
            app,
            keyboard_shortcut_rx,
            ui_rx: itc.ui_rx,
            workspace,
            windows,
        })
    }

    pub fn run(self) {
        let windows = self.windows;
        let workspace = self.workspace;

        #[cfg(target_os = "windows")]
        {
            // Install global low-level keyboard hook now that we have the UI workspace
            win_key_hook::install(workspace.event_tx.clone());
        }

        self.app
            .connect_activate(glib::clone!(@strong windows => move |_| {
                let mut windows = windows.borrow_mut();
                match windows.mode {
                    WindowMode::Multi => {
                        windows.multi.activate()
                    }
                    WindowMode::Single { fullscreen } => {
                        windows.single.set_fullscreen(fullscreen);
                        windows.single.activate()
                    }
                }
            }));

        self.keyboard_shortcut_rx.attach(
            None,
            glib::clone!(@strong windows, @strong workspace => move |shortcut| {
                let server_can_multi_monitor = windows.borrow().server_can_multi_monitor;
                match shortcut {
                    KeyboardShortcut::ToggleFullscreen { multi_monitor } => {
                        let curr_mode = windows.borrow().mode;
                        match (curr_mode, multi_monitor, workspace.monitors_geometry.borrow().is_multi(), server_can_multi_monitor) {
                            (WindowMode::Single { fullscreen: false }, true, true, true) => {
                                debug!("windowed -> fullscreen multi-monitor");
                                windows.borrow_mut().fullscreen_multi()
                            }
                            (WindowMode::Single { fullscreen: false }, false, _, _) |
                                (WindowMode::Single { fullscreen: false }, _, false, _) |
                                (WindowMode::Single { fullscreen: false }, _, _, false) => {
                                debug!("windowed -> fullscreen single-monitor");
                                windows.borrow_mut().fullscreen_single()
                            }
                            (WindowMode::Single { fullscreen: true }, true, true, true) => {
                                debug!("fullscreen single-monitor -> fullscreen multi-monitor");
                                windows.borrow_mut().fullscreen_multi()
                            }
                            (WindowMode::Single { fullscreen: true }, false, _, _) |
                                (WindowMode::Single { fullscreen: true }, _, false, _) |
                                (WindowMode::Single { fullscreen: true }, _, _, false) => {
                                debug!("fullscreen single-monitor -> windowed");
                                windows.borrow_mut().window()
                            }
                            (WindowMode::Multi, true, _, _) => {
                                debug!("fullscreen multi-monitor -> windowed");
                                windows.borrow_mut().window()
                            }
                            (WindowMode::Multi, false, _, _) => {
                                debug!("fullscreen multi-monitor -> fullscreen single-monitor");
                                windows.borrow_mut().fullscreen_single()
                            }
                        }
                    },
                    KeyboardShortcut::Disconnect => {
                        windows.borrow_mut().close();
                    },
                    KeyboardShortcut::Refresh => {
                        let wins = windows.borrow_mut();
                        let displays: Vec<Display> = match wins.mode {
                            WindowMode::Single { .. } => {
                                let size = wins.single.internals.size();
                                wins.single.internals.blank();

                                vec![Display {
                                    area: Rect::from_size(size),
                                    is_enabled: true,
                                }]
                            }
                            WindowMode::Multi => {
                                wins
                                    .multi
                                    .windows
                                    .values()
                                    .for_each(|window| window.internals.blank());

                                wins.multi.windows.keys().map(|area| Display {
                                    area,
                                    is_enabled: true,
                                }).collect()
                            }
                        };

                        match workspace
                            .event_tx
                            .send(EventMessage::DisplayEventMessage(DisplayEventMessage { displays }))
                            {
                                Ok(_) => RefreshCount::inc(),
                                Err(err) => warn!(%err, "GTK failed to tx DisplayEventMessage"),
                            }

                    }
                }
                glib::source::Continue(true)
            }),
        );

        self.ui_rx.attach(
            None,
            glib::clone!(@strong windows, @strong workspace => move |msg| {
                let mut windows = windows.borrow_mut();

                match msg {
                    UiMessage::Quit(msg) => {
                        debug!("GTK received shutdown notification: {}", msg);
                        workspace.shutdown_started.set(true);
                        windows.close();
                        return glib::source::Continue(false);
                    }
                    UiMessage::CursorImage { cursor_data, id } => {
                        let CursorData { data, width, height, x_hot, y_hot } = cursor_data;
                        let bytes = gtk::glib::Bytes::from_owned(data);

                        //
                        // Pixbuf::from_bytes() makes the following assumptions about our data:
                        //   - Colorspace::Rgb is the only supported colorspace
                        //   - We have an alpha channel (true)
                        //   - 8 bits_per_sample is the only supported bitdepth for Pixbufs
                        //   - Our rowstride is (width * RGBA) where each channel is 1 byte
                        //
                        let pixbuf = gdk_pixbuf::Pixbuf::from_bytes(
                            &bytes,
                            gtk::gdk_pixbuf::Colorspace::Rgb,
                            true,
                            8,
                            width,
                            height,
                            width * 4,
                        );
                        let cursor = gdk::Cursor::from_pixbuf(&gdk::Display::default().unwrap(), &pixbuf, x_hot, y_hot);
                        let mut cursor_map = workspace.cursor_map.borrow_mut();
                        cursor_map.insert(id, cursor);
                        windows.set_cursor(cursor_map.get(&id).unwrap());
                    }
                    UiMessage::CursorChange { id } => {
                        match workspace.cursor_map.borrow_mut().get(&id) {
                            Some(cursor) => windows.set_cursor(cursor),
                            None => warn!("No cursor for ID {}. Ignoring", id),
                        }
                    }
                    UiMessage::DisplayChange { display_config } => {
                        trace!(?display_config);
                        if let Err(error) = windows.update_display_configuration(display_config) {
                            warn!(%error, "GTK did not accept new display configuration");
                        }
                    }
                    UiMessage::Draw { draw } => windows.draw(draw),
                }
                glib::source::Continue(true)
            }),
        );
        //
        // Run with empty args to ignore the ApertureC args
        //
        self.app.run_with_args(&[""; 0]);
    }
}

struct UiWorkspace {
    event_tx: Sender<EventMessage>,
    monitors_geometry: RefCell<MonitorsGeometry>,
    keyboard_shortcut_tx: glib::Sender<KeyboardShortcut>,
    last_mouse_pos: Cell<Point>,
    cursor_map: RefCell<HashMap<usize, gdk::Cursor>>,
    lock_state: Cell<LockState>,
    held_keys: RefCell<BTreeSet<u16>>,
    shutdown_started: Cell<bool>,
}

impl UiWorkspace {
    fn new(
        itc: &GtkSideItcChannels,
        keyboard_shortcut_tx: glib::Sender<KeyboardShortcut>,
        initial_monitors_geometry: MonitorsGeometry,
    ) -> Result<Self> {
        debug!(?initial_monitors_geometry);
        Ok(UiWorkspace {
            event_tx: itc.notify_event_tx.clone(),
            keyboard_shortcut_tx,
            monitors_geometry: initial_monitors_geometry.into(),
            last_mouse_pos: Point::zero().into(),
            cursor_map: RefCell::default(),
            held_keys: RefCell::default(),
            lock_state: Cell::new(LockState::get_current()),
            shutdown_started: false.into(),
        })
    }
}

mod signal_handlers {
    use super::*;

    pub fn key_press(workspace: &UiWorkspace, key: &gdk::EventKey) {
        trace!(value=?key.keyval(), state=?key.state(), "GTK KeyPressEvent");

        if let Some(shortcut) = KeyboardShortcut::from_event(key) {
            while let Some(x11key) = workspace.held_keys.borrow_mut().pop_first() {
                debug!(?x11key, "releasing");
                workspace
                    .event_tx
                    .send(EventMessage::KeyEventMessage(
                        KeyEventMessageBuilder::default()
                            .key(x11key.into())
                            .is_pressed(false)
                            .build()
                            .expect("GTK failed to build KeyEventMessage!"),
                    ))
                    .unwrap_or_else(|err| warn!("GTK failed to tx KeyEventMessage: {}", err));
            }
            workspace
                .keyboard_shortcut_tx
                .send(shortcut)
                .unwrap_or_else(|error| warn!(%error, "GTK failed to tx KeyboardShortcut"));
        } else {
            let x11key = gtk_key_to_x11(key);
            workspace.held_keys.borrow_mut().insert(x11key);
            workspace
                .event_tx
                .send(EventMessage::KeyEventMessage(
                    KeyEventMessageBuilder::default()
                        .key(x11key.into())
                        .is_pressed(true)
                        .build()
                        .expect("GTK failed to build KeyEventMessage!"),
                ))
                .unwrap_or_else(|err| warn!("GTK failed to tx KeyEventMessage: {}", err));
        }
    }

    pub fn key_release(workspace: &UiWorkspace, key: &gdk::EventKey) {
        trace!(value=?key.keyval(), state=?key.state(), "GTK KeyReleaseEvent");

        let x11key = gtk_key_to_x11(key);
        workspace.held_keys.borrow_mut().remove(&x11key);
        workspace
            .event_tx
            .send(EventMessage::KeyEventMessage(
                KeyEventMessageBuilder::default()
                    .key(x11key.into())
                    .is_pressed(false)
                    .build()
                    .expect("GTK failed to build KeyEventMessage!"),
            ))
            .unwrap_or_else(|err| warn!("GTK failed to tx KeyEventMessage: {}", err));
    }

    pub fn button_press(workspace: &UiWorkspace, event: &gdk::EventButton) {
        let mouse_pos = workspace.last_mouse_pos.get();
        trace!(
            "GTK ButtonPressEvent: {:?} @ {:?} with {:?}",
            event.button(),
            mouse_pos,
            event.state(),
        );

        workspace
            .event_tx
            .send(EventMessage::MouseButtonEventMessage(
                MouseButtonEventMessageBuilder::default()
                    .button(event.button() - 1)
                    .is_pressed(true)
                    .pos(mouse_pos)
                    .build()
                    .expect("GTK failed to build MouseButtonEventMessage!"),
            ))
            .unwrap_or_else(|err| warn!("GTK failed to tx MouseButtonEventMessage: {}", err));
    }

    pub fn button_release(workspace: &UiWorkspace, event: &gdk::EventButton) {
        let mouse_pos = workspace.last_mouse_pos.get();
        trace!(
            "GTK ButtonReleaseEvent: {:?} @ {:?}",
            event.button(),
            mouse_pos
        );

        workspace
            .event_tx
            .send(EventMessage::MouseButtonEventMessage(
                MouseButtonEventMessageBuilder::default()
                    .button(event.button() - 1)
                    .is_pressed(false)
                    .pos(mouse_pos)
                    .build()
                    .expect("GTK failed to build MouseButtonEventMessage!"),
            ))
            .unwrap_or_else(|err| warn!("GTK failed to tx MouseButtonEventMessage: {}", err));
    }

    pub fn scroll(workspace: &UiWorkspace, event: &gdk::EventScroll) {
        let (up_mag, down_mag, left_mag, right_mag) = match event.direction() {
            gdk::ScrollDirection::Up => (1, 0, 0, 0),
            gdk::ScrollDirection::Down => (0, 1, 0, 0),
            gdk::ScrollDirection::Left => (0, 0, 1, 0),
            gdk::ScrollDirection::Right => (0, 0, 0, 1),
            gdk::ScrollDirection::Smooth => {
                let (xdelta, ydelta) = event.delta();
                let left_mag = if xdelta < 0. {
                    xdelta.abs().ceil() as usize
                } else {
                    0
                };
                let right_mag = if xdelta > 0. {
                    xdelta.ceil() as usize
                } else {
                    0
                };
                let up_mag = if ydelta < 0. {
                    ydelta.abs().ceil() as usize
                } else {
                    0
                };
                let down_mag = if ydelta > 0. {
                    ydelta.ceil() as usize
                } else {
                    0
                };
                (up_mag, down_mag, left_mag, right_mag)
            }
            _ => (0, 0, 0, 0),
        };
        let buttons = iter::repeat_n(3, up_mag)
            .chain(iter::repeat_n(4, down_mag))
            .chain(iter::repeat_n(5, left_mag))
            .chain(iter::repeat_n(6, right_mag))
            .collect::<Vec<_>>();
        let mouse_pos = workspace.last_mouse_pos.get();
        trace!(
            "GTK ButtonPressEvent: {:?} @ {:?} (scroll)",
            buttons, mouse_pos
        );

        for button in &buttons {
            //
            // Synthesize a press and release event
            //
            workspace
                .event_tx
                .send(EventMessage::MouseButtonEventMessage(
                    MouseButtonEventMessageBuilder::default()
                        .button(*button)
                        .is_pressed(true)
                        .pos(mouse_pos)
                        .build()
                        .expect("GTK failed to build MouseButtonEventMessage!"),
                ))
                .unwrap_or_else(|err| warn!("GTK failed to tx MouseButtonEventMessage: {}", err));
            workspace
                .event_tx
                .send(EventMessage::MouseButtonEventMessage(
                    MouseButtonEventMessageBuilder::default()
                        .button(*button)
                        .is_pressed(false)
                        .pos(mouse_pos)
                        .build()
                        .expect("GTK failed to build MouseButtonEventMessage!"),
                ))
                .unwrap_or_else(|err| warn!("GTK failed to tx MouseButtonEventMessage: {}", err));
        }
    }

    pub fn motion(workspace: &UiWorkspace, event: &gdk::EventMotion, offset: Point) {
        let pos = Point::new(event.position().0 as usize, event.position().1 as usize)
            + offset.to_vector();
        workspace.last_mouse_pos.set(pos);

        workspace
            .event_tx
            .send(EventMessage::PointerEventMessage(
                PointerEventMessageBuilder::default()
                    .pos(pos)
                    .build()
                    .expect("GTK failed to build PointerEventMessage!"),
            ))
            .unwrap_or_else(|err| warn!("GTK failed to tx PointerEventMessage: {}", err));
    }

    pub fn focus_in(workspace: &UiWorkspace) {
        let current_ls = LockState::get_current();
        let previous_ls = workspace.lock_state.get();
        let mut keycodes = vec![];

        if previous_ls.is_caps_locked != current_ls.is_caps_locked {
            keycodes.push(KeyMap::from(KeyMappingId::CapsLock).xkb);
        }
        if previous_ls.is_num_locked != current_ls.is_num_locked {
            keycodes.push(KeyMap::from(KeyMappingId::NumLock).xkb);
        }
        if previous_ls.is_scroll_locked != current_ls.is_scroll_locked {
            keycodes.push(KeyMap::from(KeyMappingId::ScrollLock).xkb);
        }

        for kc in keycodes {
            trace!("GTK lock state changed, sending keycode {:?}", kc);
            workspace
                .event_tx
                .send(EventMessage::KeyEventMessage(
                    KeyEventMessageBuilder::default()
                        .key(kc.into())
                        .is_pressed(true)
                        .build()
                        .expect("GTK failed to build KeyEventMessage!"),
                ))
                .unwrap_or_else(|err| warn!("GTK failed to tx KeyEventMessage: {}", err));

            workspace
                .event_tx
                .send(EventMessage::KeyEventMessage(
                    KeyEventMessageBuilder::default()
                        .key(kc.into())
                        .is_pressed(false)
                        .build()
                        .expect("GTK failed to build KeyEventMessage!"),
                ))
                .unwrap_or_else(|err| warn!("GTK failed to tx KeyEventMessage: {}", err));
        }
    }

    pub fn focus_out(workspace: &UiWorkspace) {
        workspace.lock_state.replace(LockState::get_current());
    }

    pub fn resize(
        workspace: &UiWorkspace,
        internals: Rc<WindowInternals>,
        allocation: gtk::Allocation,
        resize_timeout: Rc<Cell<Option<glib::source::SourceId>>>,
        windowed_mode_size: Rc<Cell<Size>>,
    ) {
        let new_size = Size::new(allocation.width() as _, allocation.height() as _);
        if let Some(old_source_id) = resize_timeout.take() {
            old_source_id.remove();
        }

        let timeout_handler = {
            let curr_size = internals.size();
            let timeout = resize_timeout.clone();
            let tx = workspace.event_tx.clone();

            move || {
                debug!(?curr_size, ?new_size);
                if curr_size == new_size {
                    timeout.set(None);
                    return;
                }
                debug!(?new_size, "resized");

                if let Some(gdk_window) = internals.app_window.window() {
                    if !gdk_window.state().contains(gdk::WindowState::FULLSCREEN) {
                        windowed_mode_size.set(new_size);
                    }
                }

                internals.blank_with_size(new_size);
                internals
                    .drawing_area
                    .set_size(new_size.width as _, new_size.height as _);
                internals
                    .app_window
                    .resize(new_size.width as _, new_size.height as _);
                let display_info = Display {
                    area: Rect::from_size(new_size),
                    is_enabled: true,
                };
                tx.send(EventMessage::DisplayEventMessage(DisplayEventMessage {
                    displays: vec![display_info],
                }))
                .unwrap_or_else(|err| warn!(%err, "GTK failed to tx DisplayEventMessage"));
                timeout.set(None);
            }
        };
        const RESIZE_TIMER: Duration = Duration::from_millis(600);
        let timeout_source_id = glib::source::timeout_add_local_once(RESIZE_TIMER, timeout_handler);
        resize_timeout.set(Some(timeout_source_id));
    }

    pub fn delete(app: &gtk::Application, workspace: &UiWorkspace) {
        workspace
            .event_tx
            .send(EventMessage::UiClosed)
            .unwrap_or_else(|error| {
                if !workspace.shutdown_started.get() {
                    warn!(%error, "GTK failed to send GoodbyeMessage");
                    workspace.shutdown_started.set(true);
                }
            });
        app.quit();
    }

    pub fn draw(image: &mut Image, area: &gtk::Layout, cr: &cairo::Context) {
        if !area.is_visible() {
            return;
        }

        let image_rect = Rect::from_size(image.size());
        image.with_surface(|surface| {
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
}

struct WindowInternals {
    app_window: Rc<gtk::ApplicationWindow>,
    drawing_area: Rc<gtk::Layout>,
    image: Rc<RefCell<Image>>,
}

impl WindowInternals {
    fn new(
        app: Rc<gtk::Application>,
        workspace: Rc<UiWorkspace>,
        size: Size,
        origin_offset: Point,
    ) -> Self {
        let image = Rc::new(RefCell::new(Image::red(size)));

        let app_window = Rc::new(
            gtk::ApplicationWindow::builder()
                .default_width(size.width as _)
                .default_height(size.height as _)
                .resizable(true)
                .title("ApertureC Client")
                .visible(false)
                .can_focus(true)
                .build(),
        );
        app_window.set_size_request(
            DEFAULT_RESOLUTION.width as _,
            DEFAULT_RESOLUTION.height as _,
        );
        app_window.connect_delete_event(
            glib::clone!(@strong workspace, @strong app => move |_, _| {
                signal_handlers::delete(&app, &workspace);
                gtk::Inhibit(true)
            }),
        );

        let drawing_area = Rc::new(
            gtk::Layout::builder()
                .can_focus(true)
                .events(
                    gdk::EventMask::KEY_PRESS_MASK
                        | gdk::EventMask::KEY_RELEASE_MASK
                        | gdk::EventMask::BUTTON_PRESS_MASK
                        | gdk::EventMask::BUTTON_RELEASE_MASK
                        | gdk::EventMask::SCROLL_MASK
                        | gdk::EventMask::POINTER_MOTION_MASK
                        | gdk::EventMask::FOCUS_CHANGE_MASK,
                )
                .app_paintable(false)
                .build(),
        );
        drawing_area.connect_motion_notify_event(
            glib::clone!(@strong workspace => move |_, motion| {
                signal_handlers::motion(&workspace, motion, origin_offset);
                gtk::Inhibit(true)
            }),
        );
        drawing_area.connect_key_press_event(glib::clone!(@strong workspace => move |_, key| {
            signal_handlers::key_press(&workspace, key);
            gtk::Inhibit(true)
        }));
        drawing_area.connect_key_release_event(glib::clone!(@strong workspace => move |_, key| {
            signal_handlers::key_release(&workspace, key);
            gtk::Inhibit(true)
        }));
        drawing_area.connect_button_press_event(
            glib::clone!(@strong workspace => move |_, button| {
                signal_handlers::button_press(&workspace, button);
                gtk::Inhibit(true)
            }),
        );
        drawing_area.connect_button_release_event(
            glib::clone!(@strong workspace => move |_, button| {
                signal_handlers::button_release(&workspace, button);
                gtk::Inhibit(true)
            }),
        );
        drawing_area.connect_scroll_event(glib::clone!(@strong workspace => move |_, scroll| {
            signal_handlers::scroll(&workspace, scroll);
            gtk::Inhibit(true)
        }));
        drawing_area.connect_focus_in_event(
            glib::clone!(@strong workspace => move |drawing_area, _| {
                drawing_area.grab_focus();
                signal_handlers::focus_in(&workspace);
                gtk::Inhibit(true)
            }),
        );
        drawing_area.connect_focus_out_event(glib::clone!(@strong workspace => move |_, _| {
            signal_handlers::focus_out(&workspace);
            gtk::Inhibit(true)
        }));
        drawing_area.connect_draw(
            glib::clone!(@strong workspace, @strong image => move |drawing_area, cr| {
                signal_handlers::draw(&mut image.borrow_mut(), drawing_area, cr);
                gtk::Inhibit(true)
            }),
        );

        app_window.add(&*drawing_area);
        app.connect_activate(glib::clone!(@strong app_window => move |app| {
            app.add_window(&*app_window)
        }));

        WindowInternals {
            app_window,
            drawing_area,
            image,
        }
    }

    fn size(&self) -> Size {
        self.image.borrow().size()
    }

    fn blank(&self) {
        self.blank_with_size(self.size());
    }

    fn blank_with_size(&self, size: Size) {
        self.image.replace(Image::black(size));
        self.drawing_area.queue_draw();
    }

    fn draw(&self, draw: &Draw, window_origin: Point) {
        let window_area = Rect::new(window_origin, self.size());
        if let Some(isect) = draw.area().intersection(&window_area) {
            let image_relative_origin =
                (isect.origin.to_i64() - window_origin.to_vector().to_i64()).to_usize();
            let draw_relative_origin =
                (isect.origin.to_i64() - draw.origin.to_vector().to_i64()).to_usize();
            let draw_area = Rect::new(draw_relative_origin, isect.size);
            let draw_slice = draw.pixels.slice(draw_area.as_slice());
            self.image
                .borrow_mut()
                .draw(draw_slice, image_relative_origin, draw.frame);
            self.drawing_area.queue_draw_area(
                image_relative_origin.x as _,
                image_relative_origin.y as _,
                isect.size.width as _,
                isect.size.height as _,
            );
        }
    }
}

struct SingleDisplayWindow {
    internals: Rc<WindowInternals>,
    needs_dm_on_map: Rc<Cell<bool>>,
    windowed_mode_size: Rc<Cell<Size>>,
}

impl SingleDisplayWindow {
    fn new(
        app: Rc<gtk::Application>,
        workspace: Rc<UiWorkspace>,
        size: Size,
        windowed_mode_size: Size,
    ) -> Self {
        let internals = Rc::new(WindowInternals::new(
            app,
            workspace.clone(),
            size,
            Point::zero(),
        ));
        let windowed_mode_size = Rc::new(Cell::new(windowed_mode_size));
        let resize_timeout: Rc<Cell<Option<_>>> = Rc::default();
        internals.drawing_area.connect_size_allocate(
            glib::clone!(@strong workspace, @strong internals, @strong resize_timeout, @strong windowed_mode_size => move |_, allocation| {
                signal_handlers::resize(
                    &workspace,
                    internals.clone(),
                    *allocation,
                    resize_timeout.clone(),
                    windowed_mode_size.clone()
                );
            }),
        );

        let needs_dm_on_map = Rc::new(Cell::new(false));
        internals.app_window.connect_map(
            glib::clone!(@strong workspace, @strong internals, @strong needs_dm_on_map => move |_| {
                let size = internals.size();
                internals.blank();

                if needs_dm_on_map.take() {
                    let display = Display {
                        area: Rect::from_size(size),
                        is_enabled: true,
                    };
                    workspace.event_tx.send(EventMessage::DisplayEventMessage(DisplayEventMessage {
                        displays: vec![display],
                    }))
                    .unwrap_or_else(|err| warn!(%err, "GTK failed to tx DisplayEventMessage"));
                }
                internals.drawing_area.grab_focus();
            }),
        );
        SingleDisplayWindow {
            internals,
            needs_dm_on_map,
            windowed_mode_size,
        }
    }

    fn set_fullscreen(&mut self, fullscreen: bool) {
        if fullscreen {
            // MacOS can panic when trying to set a window as undecorated
            #[cfg(not(target_os = "macos"))]
            self.internals.app_window.set_decorated(false);
            self.internals.app_window.fullscreen();
        } else {
            let size = self.windowed_mode_size.get();
            self.internals.app_window.set_decorated(true);
            self.internals.app_window.unfullscreen();
            self.internals
                .app_window
                .resize(size.width as _, size.height as _);
        }
    }

    fn activate(&mut self) {
        self.internals.app_window.show_all();
    }

    fn deactivate(&mut self) {
        self.internals.app_window.hide();
    }
}

struct MultiDisplayWindowMapStatuses {
    mapped: EuclidMap<bool>,
}

impl MultiDisplayWindowMapStatuses {
    fn new(monitors_geometry: &MonitorsGeometry) -> Self {
        MultiDisplayWindowMapStatuses {
            mapped: monitors_geometry
                .iter()
                .map(|MonitorGeometry { total_area, .. }| (total_area, false))
                .collect(),
        }
    }

    fn mark_updated(&mut self, area: Rect) {
        self.mapped
            .get_all_overlaps_mut(area)
            .for_each(|(monitor_area, updated)| {
                if monitor_area == area {
                    *updated = true
                }
            });
    }

    fn get_display_message(&mut self) -> Option<DisplayEventMessage> {
        if self.mapped.values().all(|updated| *updated) {
            self.mapped
                .values_mut()
                .for_each(|updated| *updated = false);
            Some(DisplayEventMessage {
                displays: self
                    .mapped
                    .keys()
                    .map(|area| Display {
                        area,
                        is_enabled: true,
                    })
                    .collect(),
            })
        } else {
            None
        }
    }
}

struct MultiDisplayWindow {
    internals: Rc<WindowInternals>,
}

impl MultiDisplayWindow {
    fn new(
        app: Rc<gtk::Application>,
        workspace: Rc<UiWorkspace>,
        area: Rect,
        window_statuses: Rc<RefCell<MultiDisplayWindowMapStatuses>>,
        needs_dm_on_configure: Rc<Cell<bool>>,
    ) -> Self {
        let internals = Rc::new(WindowInternals::new(
            app,
            workspace.clone(),
            area.size,
            area.origin,
        ));
        internals
            .app_window
            .connect_map(glib::clone!(@strong internals => move |_| {
                internals.app_window.set_gravity(gdk::Gravity::NorthWest);
                internals.app_window.move_(area.origin.x as _, area.origin.y as _);
                // MacOS can panic when trying to set a window as undecorated
                #[cfg(not(target_os = "macos"))]
                internals.app_window.set_decorated(false);
                internals.app_window.fullscreen();
                internals.blank();
                internals.drawing_area.grab_focus();
            }));
        internals.app_window.connect_configure_event(
            glib::clone!(@strong internals, @strong window_statuses => move |_, event| {
                let (x, y) = event.position();
                let (w, h) = event.size();
                let moved_location = Point::new(x as _, y as _);
                let new_size = Size::new(w as _, h as _);
                let new_area = Rect::new(moved_location, new_size);
                if area == new_area {
                    window_statuses.borrow_mut().mark_updated(new_area);
                    if let Some(dm) = window_statuses.borrow_mut().get_display_message() {
                        if needs_dm_on_configure.take() {
                            workspace.event_tx.send(EventMessage::DisplayEventMessage(dm))
                                .unwrap_or_else(|err| warn!(%err, "GTK failed to tx DisplayEventMessage"));
                        }
                    }
                }
                false
            }),
        );
        MultiDisplayWindow { internals }
    }
}

struct MultiDisplayWindows {
    windows: EuclidMap<MultiDisplayWindow>,
    needs_dm_on_configure: Rc<Cell<bool>>,
}

impl MultiDisplayWindows {
    fn new(app: Rc<gtk::Application>, workspace: Rc<UiWorkspace>) -> Self {
        let statuses = Rc::new(RefCell::new(MultiDisplayWindowMapStatuses::new(
            &workspace.monitors_geometry.borrow(),
        )));
        let needs_dm_on_configure = Rc::new(Cell::new(false));
        let windows = workspace
            .monitors_geometry
            .borrow()
            .iter()
            .map(|MonitorGeometry { total_area, .. }| {
                (
                    total_area,
                    MultiDisplayWindow::new(
                        app.clone(),
                        workspace.clone(),
                        total_area,
                        statuses.clone(),
                        needs_dm_on_configure.clone(),
                    ),
                )
            })
            .collect();
        MultiDisplayWindows {
            windows,
            needs_dm_on_configure,
        }
    }

    fn activate(&mut self) {
        for (_, window) in &self.windows {
            window.internals.app_window.show_all();
        }
    }

    fn deactivate(&mut self) {
        for (_, window) in &self.windows {
            window.internals.app_window.hide();
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum WindowMode {
    Single { fullscreen: bool },
    Multi,
}

impl WindowMode {
    fn derive_next<T: Into<WindowMode>>(
        current_mode: T,
        display_config: &DisplayConfiguration,
        monitors_geometry: &MonitorsGeometry,
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
            if !monitors_geometry.matches(display_config) {
                bail!(
                    "Server provided multi-monitor initial display configuration which does not match monitors geometry on client"
                );
            }
            Ok(WindowMode::Multi)
        }
    }
}

impl From<DisplayMode> for WindowMode {
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    fn from(dm: DisplayMode) -> Self {
        match dm {
            DisplayMode::Windowed { .. } => WindowMode::Single { fullscreen: false },
            DisplayMode::SingleFullscreen => WindowMode::Single { fullscreen: true },
            DisplayMode::MultiFullscreen => WindowMode::Multi,
        }
    }

    // Windows and MacOS do not work seemlessly with GTK's fullscreen implementation, specifically
    // with requesting windows to move to different monitors then fullscreening them. For now,
    // we just disable fullscreen multi-monitor until these issues are resolved or a non-GTK
    // implementation is created
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    fn from(dm: DisplayMode) -> Self {
        let fullscreen = !matches!(dm, DisplayMode::Windowed { .. });
        WindowMode::Single { fullscreen }
    }
}

struct Windows {
    display_config_id: usize,
    mode: WindowMode,
    workspace: Rc<UiWorkspace>,
    single: SingleDisplayWindow,
    multi: MultiDisplayWindows,
    server_can_multi_monitor: bool,
}

impl Windows {
    fn new(
        app: Rc<gtk::Application>,
        workspace: Rc<UiWorkspace>,
        initial_display_mode: DisplayMode,
        initial_display_config: &DisplayConfiguration,
    ) -> Result<Self> {
        let initial_window_mode = WindowMode::derive_next(
            initial_display_mode,
            initial_display_config,
            &workspace.monitors_geometry.borrow(),
        )?;
        debug!(?initial_window_mode);

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

        let server_can_multi_monitor = !matches!(
            (initial_display_mode, initial_window_mode),
            (DisplayMode::MultiFullscreen, WindowMode::Single { .. })
        );

        let single = SingleDisplayWindow::new(
            app.clone(),
            workspace.clone(),
            dc_size,
            initial_windowed_size,
        );
        let multi = MultiDisplayWindows::new(app.clone(), workspace.clone());

        Ok(Windows {
            mode: initial_window_mode,
            display_config_id: initial_display_config.id,
            workspace: workspace.clone(),
            single,
            multi,
            server_can_multi_monitor,
        })
    }

    fn update_display_configuration(&mut self, display_config: DisplayConfiguration) -> Result<()> {
        debug!(?display_config);
        if display_config.id <= self.display_config_id {
            bail!("out-of-date display configuration");
        }

        let next_window_mode = WindowMode::derive_next(
            self.mode,
            &display_config,
            &self.workspace.monitors_geometry.borrow(),
        )?;
        if let (WindowMode::Multi, WindowMode::Single { fullscreen }) =
            (self.mode, next_window_mode)
        {
            self.single(fullscreen);
            self.server_can_multi_monitor = false;
        }
        Ok(())
    }

    fn draw(&self, draw: Draw) {
        match self.mode {
            WindowMode::Single { .. } => {
                self.single.internals.draw(&draw, Point::zero());
            }
            WindowMode::Multi => {
                for (area, window) in &self.multi.windows {
                    window.internals.draw(&draw, area.origin);
                }
            }
        }
    }

    fn all_windows(&self) -> impl Iterator<Item = &WindowInternals> {
        iter::once(&*self.single.internals)
            .chain(self.multi.windows.values().map(|w| &*w.internals))
    }

    fn close(&mut self) {
        for window in self.all_windows() {
            window.app_window.close();
        }
    }

    fn set_cursor(&mut self, cursor: &gdk::Cursor) {
        for window in self.all_windows() {
            if let Some(window) = window.drawing_area.window() {
                window.set_cursor(Some(cursor));
            }
        }
    }

    fn fullscreen_multi(&mut self) {
        if !matches!(self.mode, WindowMode::Multi) {
            self.multi.needs_dm_on_configure.set(true);
            self.multi.activate();
            self.single.deactivate();
            self.mode = WindowMode::Multi;
        }
    }

    fn fullscreen_single(&mut self) {
        self.single(true)
    }

    fn window(&mut self) {
        self.single(false)
    }

    fn single(&mut self, fullscreen: bool) {
        match self.mode {
            WindowMode::Multi => {
                self.single.set_fullscreen(fullscreen);
                self.single.needs_dm_on_map.set(true);
                self.single.activate();
                self.multi.deactivate();
            }
            WindowMode::Single {
                fullscreen: curr_fullscreen,
            } => {
                if curr_fullscreen != fullscreen {
                    self.single.set_fullscreen(fullscreen);
                }
            }
        }
        self.mode = WindowMode::Single { fullscreen };
    }
}
