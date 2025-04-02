use crate::client::{
    ControlMessage, CursorData, DisplayEventMessage, DisplayMode, EventMessage,
    GoodbyeMessageBuilder, KeyEventMessageBuilder, MonitorGeometry, MouseButtonEventMessageBuilder,
    PointerEventMessageBuilder, UiMessage,
};
use crate::gtk3::image::Image;

use aperturec_graphics::{display::*, prelude::*};

use anyhow::{anyhow, bail, Context as _, Result};
use crossbeam::channel::{unbounded, Receiver, Sender};
use gtk::cairo::{Context, ImageSurface};
use gtk::gdk::{
    self, keys, Display as GdkDisplay, EventMask, Keymap, ModifierType, ScrollDirection,
};
use gtk::prelude::*;
use gtk::{glib, Adjustment, ApplicationWindow, DrawingArea, ScrolledWindow};
use keycode::{KeyMap, KeyMapping, KeyMappingId};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::iter;
use std::rc::Rc;
use std::sync::LazyLock;
use std::time::Duration;
use tracing::*;

#[cfg(target_os = "windows")]
use windows::Win32::UI::Input::KeyboardAndMouse::{MapVirtualKeyW, MAPVK_VK_TO_VSC_EX};

static GTK_INIT: LazyLock<()> = LazyLock::new(|| {
    gdk::set_allowed_backends("x11,*");
    gtk::init().expect("Failed to initialize GTK");
});

pub mod image;

pub const DEFAULT_RESOLUTION: Size = Size::new(800, 600);

pub struct ClientSideItcChannels {
    pub ui_tx: glib::Sender<UiMessage>,
    pub img_tx: glib::Sender<Image>,
    pub notify_control_rx: Receiver<ControlMessage>,
    pub notify_control_tx: Sender<ControlMessage>,
    pub notify_event_rx: Receiver<EventMessage>,
    pub notify_event_tx: Sender<EventMessage>,
    pub notify_media_rx: Receiver<DisplayConfiguration>,
    pub notify_media_tx: Sender<DisplayConfiguration>,
    pub img_rx: Receiver<Image>,
}

pub struct GtkSideItcChannels {
    ui_rx: glib::Receiver<UiMessage>,
    img_rx: glib::Receiver<Image>,
    notify_control_tx: Sender<ControlMessage>,
    notify_event_tx: Sender<EventMessage>,
    img_tx: Sender<Image>,
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

        let (notify_control_tx, notify_control_rx) = unbounded();
        let (notify_event_tx, notify_event_rx) = unbounded();
        let (notify_media_tx, notify_media_rx) = unbounded();

        let (glib_img_tx, glib_img_rx) = glib::MainContext::channel(glib::Priority::default());

        let (img_tx, img_rx) = unbounded();

        ItcChannels {
            client_half: ClientSideItcChannels {
                ui_tx,
                img_tx: glib_img_tx,
                notify_control_rx,
                notify_control_tx: notify_control_tx.clone(),
                notify_event_rx,
                notify_event_tx: notify_event_tx.clone(),
                notify_media_rx,
                notify_media_tx: notify_media_tx.clone(),
                img_rx,
            },
            gtk_half: GtkSideItcChannels {
                ui_rx,
                img_rx: glib_img_rx,
                notify_control_tx,
                notify_event_tx,
                img_tx,
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
        let display = GdkDisplay::default().expect("Failed to get default GTK display");
        match Keymap::for_display(&display) {
            None => panic!("can't get keymap"),
            Some(keymap) => Self {
                is_caps_locked: keymap.is_caps_locked(),
                is_num_locked: keymap.is_num_locked(),
                is_scroll_locked: keymap.is_scroll_locked(),
            },
        }
    }
}

/// Generate a `MonitorGeometry` using `gdk`'s API. The returned `MonitorGeometry` is guaranteed to
/// have a display rooted at the origin and only contain monitors which are non-overlapping and
/// contiguous.
pub fn get_monitor_geometry() -> Result<MonitorGeometry> {
    *GTK_INIT;

    fn gdkrect2rect(geo: gdk::Rectangle) -> Rect {
        Rect::new(
            Point::new(geo.x() as _, geo.y() as _),
            Size::new(geo.width() as _, geo.height() as _),
        )
    }

    let display = GdkDisplay::default().ok_or(anyhow!("Failed to get default GTK display"))?;
    let num_monitors = display.n_monitors();
    if num_monitors <= 0 {
        bail!("No GTK monitors found for display {:?}", display);
    }

    let origin_monitor = display.monitor_at_point(0, 0).ok_or(anyhow!(
        "Failed to get origin GTK monitor for display {:?}",
        display
    ))?;
    let origin_rect = gdkrect2rect(origin_monitor.geometry());
    let single = MonitorGeometry::Single {
        size: origin_rect.size,
    };

    if num_monitors == 1 {
        return Ok(single);
    }

    let mut unvisited = vec![origin_monitor.clone()];
    let mut visited = HashSet::new();
    while let Some(monitor) = unvisited.pop() {
        if visited.contains(&monitor) {
            continue;
        }

        let geo = monitor.geometry();
        if let Some(above) = display.monitor_at_point(geo.x(), geo.y() - 1) {
            unvisited.push(above);
        }
        if let Some(below) = display.monitor_at_point(geo.x(), geo.y() + geo.height() + 1) {
            unvisited.push(below);
        }
        if let Some(left) = display.monitor_at_point(geo.x() - 1, geo.y()) {
            unvisited.push(left);
        }
        if let Some(right) = display.monitor_at_point(geo.x() + geo.width() + 1, geo.y()) {
            unvisited.push(right);
        }

        visited.insert(monitor);
    }

    if visited.len() != num_monitors as usize {
        warn!(
            gtk_monitors = num_monitors,
            discovered_monitors = visited.len(),
            "GTK monitors found != monitors discovered, falling back to single monitor"
        );
        return Ok(single);
    }

    let mut rects = visited
        .iter()
        .map(|monitor| gdkrect2rect(monitor.geometry()))
        .collect::<HashSet<_>>();
    if rects.len() != num_monitors as usize {
        warn!("Some monitors were overlapping, falling back to single monitor");
        return Ok(single);
    }

    rects.remove(&origin_rect);
    Ok(MonitorGeometry::Multi {
        origin: origin_rect,
        other: rects,
    })
}

pub fn get_fullscreen_dims() -> anyhow::Result<(i32, i32)> {
    *GTK_INIT;
    let display = GdkDisplay::default().context("Failed to get default GTK display")?;
    let monitor = display
        .monitor_at_point(0, 0)
        .context("Failed to get GTK monitor at (0,0)")?;
    let geo = monitor.geometry();

    Ok((geo.width(), geo.height()))
}

enum KeyboardShortcut {
    ToggleFullscreen { multi_monitor: bool },
    Disconnect,
}

impl KeyboardShortcut {
    fn from_event(key_event: &gtk::gdk::EventKey) -> Option<Self> {
        let state = key_event.state() & ModifierType::MODIFIER_MASK;

        if !state.contains(ModifierType::CONTROL_MASK | ModifierType::MOD1_MASK) {
            None
        } else if key_event.keyval() == keys::constants::Return {
            if state.contains(ModifierType::SHIFT_MASK) {
                Some(Self::ToggleFullscreen {
                    multi_monitor: false,
                })
            } else {
                Some(Self::ToggleFullscreen {
                    multi_monitor: true,
                })
            }
        } else if key_event.keyval() == keys::constants::F12
            && state.contains(ModifierType::SHIFT_MASK)
        {
            Some(Self::Disconnect)
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
fn convert_win_virtual_key_to_scan_code(virtual_key: u32) -> anyhow::Result<u32> {
    let scancode = unsafe { MapVirtualKeyW(virtual_key, MAPVK_VK_TO_VSC_EX) };
    if scancode == 0 {
        // MapVirtualKeyW returns 0 if it fails to convert the virtual key code
        anyhow::bail!(
            "Failed to convert virtual key code {:#X} to scan code",
            virtual_key
        )
    }
    Ok(scancode)
}

fn gtk_key_to_x11(key: &gtk::gdk::EventKey) -> u16 {
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
            KeyMapping::Win(scancode as u16)
        }

        #[cfg(not(any(target_os = "windows", target_os = "macos")))]
        {
            KeyMapping::Xkb(keycode)
        }
    };

    let map = KeyMap::try_from(mapping).expect("Could not convert native key code");
    map.xkb
}

pub struct GtkUi;

impl GtkUi {
    pub fn run_ui(
        itc: GtkSideItcChannels,
        initial_display_mode: DisplayMode,
        monitor_geometry: MonitorGeometry,
        display_config: DisplayConfiguration,
    ) {
        let app = gtk::Application::builder()
            .application_id("com.zetier.aperturec.client")
            .flags(gtk::gio::ApplicationFlags::NON_UNIQUE)
            .build();

        let itc = Cell::new(Some(itc));
        app.connect_activate(move |app| {
            if let Some(itc) = itc.take() {
                build_ui(
                    app,
                    itc,
                    initial_display_mode,
                    monitor_geometry.clone(),
                    display_config.clone(),
                )
            }
        });

        //
        // Run with empty args to ignore the ApertureC args
        //
        app.run_with_args(&[""; 0]);
    }
}

fn render_image(cr: &Context, image: &ImageSurface) {
    cr.reset_clip();
    cr.set_source_surface(image, 0_f64, 0_f64)
        .expect("cairo set source");

    for rect in cr
        .copy_clip_rectangle_list()
        .expect("clip rectangle")
        .iter()
    {
        cr.rectangle(rect.x(), rect.y(), rect.width(), rect.height());
    }
    cr.fill().expect("cairo fill");
}

struct UiWorkspace {
    image: RefCell<Image>,
    img_tx: Sender<Image>,
    is_fullscreen: Cell<bool>,
    is_multimonitored: Cell<bool>,
    can_multimonitor: Cell<bool>,
    windowed_size: Cell<Size>,
    display_config: RefCell<DisplayConfiguration>,
    monitor_geometry: MonitorGeometry,
}

fn build_ui(
    app: &gtk::Application,
    itc: GtkSideItcChannels,
    initial_display_mode: DisplayMode,
    monitor_geometry: MonitorGeometry,
    display_config: DisplayConfiguration,
) {
    let windowed_size = match &initial_display_mode {
        DisplayMode::MultiFullscreen | DisplayMode::SingleFullscreen => {
            let halved = monitor_geometry.single_monitor_size() / 2;
            Size::new(
                halved.width.max(DEFAULT_RESOLUTION.width),
                halved.height.max(DEFAULT_RESOLUTION.height),
            )
        }
        DisplayMode::Windowed { size } => *size,
    };

    let window = ApplicationWindow::builder()
        .application(app)
        .default_width(windowed_size.width as _)
        .default_height(windowed_size.height as _)
        .resizable(true)
        .title("ApertureC Client")
        .build();

    let scrolled_window =
        ScrolledWindow::new(None as Option<&Adjustment>, None as Option<&Adjustment>);
    let area = DrawingArea::new();

    scrolled_window.add(&area);
    window.add(&scrolled_window);

    area.set_size_request(windowed_size.width as _, windowed_size.height as _);
    area.set_can_focus(true);
    area.add_events(
        EventMask::KEY_PRESS_MASK
            | EventMask::KEY_RELEASE_MASK
            | EventMask::BUTTON_PRESS_MASK
            | EventMask::BUTTON_RELEASE_MASK
            | EventMask::SCROLL_MASK
            | EventMask::POINTER_MOTION_MASK
            | EventMask::FOCUS_CHANGE_MASK,
    );

    //
    // Setup Images
    //
    let img = Image::blue(&display_config);
    itc.img_tx.send(img.clone()).unwrap_or_else(|error| {
        warn!(%error, "GTK failed to tx Image");
    });
    let workspace = Rc::new(UiWorkspace {
        image: RefCell::new(img),
        img_tx: itc.img_tx,
        is_fullscreen: false.into(),
        is_multimonitored: matches!(initial_display_mode, DisplayMode::MultiFullscreen).into(),
        can_multimonitor: monitor_geometry.is_multi().into(),
        windowed_size: windowed_size.into(),
        monitor_geometry,
        display_config: display_config.into(),
    });

    area.connect_draw(
        glib::clone!(@weak workspace => @default-return gtk::Inhibit(false), move |_, cr| {
            let UiWorkspace { ref image, .. } = *workspace;
            image.borrow_mut().with_surface(|surface| {
                render_image(cr, surface);
            });
            gtk::Inhibit(false)
        }),
    );

    //
    // Setup mouse tracking
    //
    let last_mouse_pos = Rc::new(Cell::new(Point::new(0, 0)));
    let lm_motion = last_mouse_pos.clone();
    let lm_button_down = last_mouse_pos.clone();
    let lm_button_up = last_mouse_pos.clone();
    let lm_scroll = last_mouse_pos.clone();

    //
    // Setup event channel Senders
    //
    let key_press_tx = itc.notify_event_tx.clone();
    let key_release_tx = itc.notify_event_tx.clone();
    let button_press_tx = itc.notify_event_tx.clone();
    let button_release_tx = itc.notify_event_tx.clone();
    let scroll_press_tx = itc.notify_event_tx.clone();
    let pointer_motion_tx = itc.notify_event_tx.clone();
    let lock_state_tx = itc.notify_event_tx.clone();
    let window_resize_tx = itc.notify_event_tx.clone();
    let notify_control_tx = itc.notify_control_tx.clone();

    //
    // Setup cursor tracking
    //
    let mut cursor_map = HashMap::new();

    //
    // Setup lock state tracking
    //
    let lock_state = Rc::new(Cell::new(LockState::get_current()));
    let ls_in = lock_state.clone();
    let ls_out = lock_state.clone();

    //
    // Setup keyboard tracking
    //
    area.connect_key_press_event(
        glib::clone!(@strong window, @strong workspace => move |_, key| {
            trace!("GTK KeyPressEvent: {:?} : {:?}", key.keyval(), key.state());

            let UiWorkspace {
                ref is_multimonitored,
                ref can_multimonitor,
                ref is_fullscreen,
                ..
            } = *workspace;

            match KeyboardShortcut::from_event(key) {
                Some(KeyboardShortcut::ToggleFullscreen { multi_monitor }) => {
                    if is_fullscreen.get() {
                        if is_multimonitored.get() {
                            if multi_monitor {
                                debug!("fullscreen multi-monitor -> windowed");
                                window.unfullscreen();
                            } else {
                                debug!("fullscreen multi-monitor -> fullscreen single-monitor");
                                is_multimonitored.set(false);
                                set_fullscreen_mode(&window, false);
                            }
                        } else if multi_monitor {
                            if can_multimonitor.get() {
                                debug!("fullscreen single-monitor -> fullscreen multi-monitor");
                                is_multimonitored.set(true);
                                set_fullscreen_mode(&window, true);
                            } else {
                                debug!("fullscreen single-monitor -> windowed");
                                window.unfullscreen();
                            }
                        } else {
                            debug!("fullscreen single-monitor -> windowed");
                            window.unfullscreen();
                        }
                    } else {
                        if multi_monitor {
                            if can_multimonitor.get() {
                                debug!("windowed -> fullscreen multi-monitor");
                                is_multimonitored.set(true);
                                set_fullscreen_mode(&window, true);
                            } else {
                                warn!("Multi-monitor mode disabled");
                                is_multimonitored.set(false);
                                set_fullscreen_mode(&window, false);
                            }
                        } else {
                            debug!("windowed -> fullscreen single-monitor");
                            is_multimonitored.set(false);
                            set_fullscreen_mode(&window, false);
                        }
                        window.fullscreen();
                    }
                },
                Some(KeyboardShortcut::Disconnect) => window.close(),
                None => {
                    key_press_tx.send(
                        EventMessage::KeyEventMessage(
                            KeyEventMessageBuilder::default()
                            .key(gtk_key_to_x11(key).into())
                            .is_pressed(true)
                            .build()
                            .expect("GTK failed to build KeyEventMessage!")
                            )
                        ).unwrap_or_else(|err| {
                        warn!("GTK failed to tx KeyEventMessage: {}", err)
                    });
                },
            }

            gtk::Inhibit(false)
        }),
    );

    area.connect_key_release_event(glib::clone!(@strong window => move |_, key| {
        trace!("GTK KeyReleaseEvent: {:?} : {:?}", key.keyval(), key.state());

        key_release_tx.send(
            EventMessage::KeyEventMessage(
                KeyEventMessageBuilder::default()
                    .key(gtk_key_to_x11(key).into())
                    .is_pressed(false)
                    .build()
                    .expect("GTK failed to build KeyEventMessage!")
            )
        ).unwrap_or_else(|err| {
            warn!("GTK failed to tx KeyEventMessage: {}", err)
        });

        gtk::Inhibit(false)
    }));

    area.connect_button_press_event(glib::clone!(@strong window => move |_, event| {
        trace!("GTK ButtonPressEvent: {:?} @ {:?}", event.button(), lm_button_down.as_ref().get());

        button_press_tx.send(
            EventMessage::MouseButtonEventMessage(
                MouseButtonEventMessageBuilder::default()
                    .button(event.button() - 1)
                    .is_pressed(true)
                    .pos(lm_button_down.as_ref().get())
                    .build()
                    .expect("GTK failed to build MouseButtonEventMessage!")
            )
        ).unwrap_or_else(|err| {
            warn!("GTK failed to tx MouseButtonEventMessage: {}", err)
        });

        gtk::Inhibit(false)
    }));

    area.connect_button_release_event(glib::clone!(@strong window => move |_, event| {
        trace!("GTK ButtonReleaseEvent: {:?} @ {:?}", event.button(), lm_button_up.as_ref().get());

        button_release_tx.send(
            EventMessage::MouseButtonEventMessage(
                MouseButtonEventMessageBuilder::default()
                    .button(event.button() - 1)
                    .is_pressed(false)
                    .pos(lm_button_up.as_ref().get())
                    .build()
                    .expect("GTK failed to build MouseButtonEventMessage!")
            )
        ).unwrap_or_else(|err| {
            warn!("GTK failed to tx MouseButtonEventMessage: {}", err)
        });

        gtk::Inhibit(false)
    }));

    area.connect_scroll_event(glib::clone!(@strong window => move |_, event| {
        let button = match event.direction() {
            ScrollDirection::Up => 3,
            ScrollDirection::Down => 4,
            ScrollDirection::Left => 5,
            ScrollDirection::Right => 6,
            _ => 0
        };

        trace!("GTK ButtonPressEvent: {:?} @ {:?} (scroll)", button, lm_scroll.as_ref().get());

        //
        // Synthesize a press / release event
        //
        scroll_press_tx.send(
            EventMessage::MouseButtonEventMessage(
                MouseButtonEventMessageBuilder::default()
                    .button(button)
                    .is_pressed(true)
                    .pos(lm_scroll.as_ref().get())
                    .build()
                    .expect("GTK failed to build MouseButtonEventMessage!")
            )
        ).unwrap_or_else(|err| {
            warn!("GTK failed to tx MouseButtonEventMessage: {}", err)
        });

        scroll_press_tx.send(
            EventMessage::MouseButtonEventMessage(
                MouseButtonEventMessageBuilder::default()
                    .button(button)
                    .is_pressed(false)
                    .pos(lm_scroll.as_ref().get())
                    .build()
                    .expect("GTK failed to build MouseButtonEventMessage!")
            )
        ).unwrap_or_else(|err| {
            warn!("GTK failed to tx MouseButtonEventMessage: {}", err)
        });

        gtk::Inhibit(false)
    }));

    area.connect_motion_notify_event(move |_, event| {
        let pos = Point::new(event.position().0 as usize, event.position().1 as usize);
        lm_motion.set(pos);

        pointer_motion_tx
            .send(EventMessage::PointerEventMessage(
                PointerEventMessageBuilder::default()
                    .pos(lm_motion.as_ref().get())
                    .build()
                    .expect("GTK failed to build PointerEventMessage!"),
            ))
            .unwrap_or_else(|err| warn!("GTK failed to tx PointerEventMessage: {}", err));

        gtk::Inhibit(false)
    });

    area.connect_focus_in_event(move |_, _| {
        let current_ls = LockState::get_current();
        let previous_ls = ls_in.get();
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
            lock_state_tx
                .send(EventMessage::KeyEventMessage(
                    KeyEventMessageBuilder::default()
                        .key(kc.into())
                        .is_pressed(true)
                        .build()
                        .expect("GTK failed to build KeyEventMessage!"),
                ))
                .unwrap_or_else(|err| warn!("GTK failed to tx KeyEventMessage: {}", err));

            lock_state_tx
                .send(EventMessage::KeyEventMessage(
                    KeyEventMessageBuilder::default()
                        .key(kc.into())
                        .is_pressed(false)
                        .build()
                        .expect("GTK failed to build KeyEventMessage!"),
                ))
                .unwrap_or_else(|err| warn!("GTK failed to tx KeyEventMessage: {}", err));
        }

        gtk::Inhibit(false)
    });

    area.connect_focus_out_event(move |_, _| {
        let lock_state = LockState::get_current();
        ls_out.replace(lock_state);
        gtk::Inhibit(false)
    });

    const RESIZE_TIMER: Duration = Duration::from_millis(600);
    let resize_timeout: Rc<Cell<Option<glib::source::SourceId>>> = Rc::default();
    area.connect_size_allocate(glib::clone!(@strong window, @strong workspace => move |_, allocation| {

        if let Some(old_source_id) = resize_timeout.take() {
            old_source_id.remove();
        }

        let new_size = Size::new(allocation.width() as _, allocation.height() as _);
        let new_position = Point::new(allocation.x() as _, allocation.y() as _);
        let new_extent = Rect::new(new_position, new_size);
        trace!(?new_extent);

        let timeout_handler = {
            let timeout = resize_timeout.clone();
            let tx = window_resize_tx.clone();
            let workspace = workspace.clone();

            move || {
                let UiWorkspace {
                    ref windowed_size,
                    ref is_fullscreen,
                    ref is_multimonitored,
                    ref monitor_geometry,
                    ref can_multimonitor,
                    ..
                } = *workspace;

                if !is_fullscreen.get() && windowed_size.get() == new_size {
                    timeout.set(None);
                    return;
                }
                debug!(?new_size, "resized");

                let display_infos = match (monitor_geometry, is_fullscreen.get(), is_multimonitored.get()) {
                    (MonitorGeometry::Multi { origin, other }, true, true) => {
                        if monitor_geometry.extent() != new_extent {
                            if monitor_geometry.iter().any(|r| r == new_extent) {
                                warn!("Multiple monitors detected, but window is fullscreen in a single monitor");
                            } else {
                                warn!("Attempted fullscreening, but the window does not match any monitor geometry - there may be visual artifacts");
                            }
                            info!("Reverting to single-monitor mode and disabling multi-monitor mode");
                            can_multimonitor.set(false);
                            is_multimonitored.set(false);
                            vec![Display { area: Rect::from_size(new_size), is_enabled: true }]
                        } else {
                            iter::once(origin)
                                .chain(other)
                                .map(|rect| Display {
                                    area: *rect,
                                    is_enabled: true,
                                })
                                .collect()
                        }
                    }
                    (_, false, _) => vec![Display {
                        area: Rect::from_size(new_size), is_enabled: true
                    }],
                    (_, true, _) => vec![Display {
                        area: Rect::from_size(new_size),
                        is_enabled: true,
                    }],
                };
                tx.send(EventMessage::DisplayEventMessage(DisplayEventMessage {
                    displays: display_infos
                }))
                .unwrap_or_else(|err| warn!(%err, "GTK failed to tx DisplayEventMessage"));
                windowed_size.set(new_size);
                timeout.set(None);
            }
        };

        let timeout_source_id = glib::source::timeout_add_local_once(RESIZE_TIMER, timeout_handler);
        resize_timeout.set(Some(timeout_source_id));
    }));

    window.connect_window_state_event(glib::clone!(@strong workspace => move |_, event| {
        let UiWorkspace { ref is_fullscreen, .. } = *workspace;
        if event.changed_mask() & gdk::WindowState::FULLSCREEN != gdk::WindowState::empty() {
            if is_fullscreen.get() {
                debug!("unfullscreening");
                is_fullscreen.set(false);
            } else {
                debug!("fullscreening");
                is_fullscreen.set(true);
            }
        }

        gtk::Inhibit(false)
    }));

    window.connect_delete_event(move |_, _| {
        _ = notify_control_tx.send(ControlMessage::UiClosed(
            GoodbyeMessageBuilder::default()
                .reason(String::from("UI closed"))
                .build()
                .expect("GTK failed to build GoodbyeMessage"),
        ));

        gtk::Inhibit(false)
    });

    itc.img_rx.attach(
        None,
        glib::clone!(@strong area, @strong window, @strong workspace => move |mut updated_img| {
            let UiWorkspace {
                ref image,
                ref img_tx,
                ref display_config,
                ..
            } = *workspace;

            if updated_img.display_config_id < display_config.borrow().id {
                debug!(
                    "received image with out-of-date display config ID {}",
                    updated_img.display_config_id,
                );
                updated_img.update_display_config(&display_config.borrow());
            }

            //
            // Replace the stale Image with the new one. The data in the images vector will be used
            // to render the display at the next redraw event.
            //
            let mut stale_img = image.replace(updated_img);

            for updated_area in &image.borrow().update_history {
                let rect = updated_area.to_rect();
                area.queue_draw_area(
                    rect.origin.x as i32,
                    rect.origin.y as i32,
                    rect.size.width as i32,
                    rect.size.height as i32
                );
            }

            if stale_img.display_config_id < display_config.borrow().id {
                debug!(
                    "Replacing stale image with DC ID {} with new DC ID {}",
                    stale_img.display_config_id, display_config.borrow().id
                );
                stale_img.update_display_config(&display_config.borrow());
            } else {
                stale_img.copy_updates(&image.borrow()).unwrap_or_else(|err| {
                    warn!("Failed to copy updates from new image to stale image: {}", err)
                });
            }
            img_tx.send(stale_img)
                .unwrap_or_else(|err| warn!("GTK failed to send image to client: {}", err));

            glib::source::Continue(true)
        }),
    );

    itc.ui_rx.attach(
        None,
        glib::clone!(@strong window, @strong workspace => move |msg| {
            match msg {
                UiMessage::Quit(msg) => {
                    debug!("GTK received shutdown notification: {}", msg);
                    window.close();
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
                    let pixbuf = gtk::gdk_pixbuf::Pixbuf::from_bytes(
                        &bytes,
                        gtk::gdk_pixbuf::Colorspace::Rgb,
                        true,
                        8,
                        width,
                        height,
                        width * 4,
                    );
                    let cursor = gtk::gdk::Cursor::from_pixbuf(&GdkDisplay::default().unwrap(), &pixbuf, x_hot, y_hot);
                    cursor_map.insert(id, cursor);

                    area.window().unwrap().set_cursor(Some(cursor_map.get(&id).unwrap()))
                }
                UiMessage::CursorChange { id } => {
                    match cursor_map.get(&id) {
                        Some(cursor) => area.window().unwrap().set_cursor(Some(cursor)),
                        None => warn!("No cursor for ID {}. Ignoring", id),
                    }
                }
                UiMessage::DisplayChange { display_config: new_display_config } => {
                    let UiWorkspace {
                        ref is_fullscreen,
                        ref monitor_geometry,
                        ref display_config,
                        ref image,
                        ref is_multimonitored,
                        ..
                    } = *workspace;
                    trace!(?new_display_config);

                    if new_display_config.id == display_config.borrow().id {
                        trace!("Ignoring display configuration with same ID");
                        return glib::source::Continue(true);
                    }

                    if new_display_config.display_decoder_infos.is_empty() {
                        warn!("Server provided display configuration has no displays. Ignoring");
                        return glib::source::Continue(true);
                    }
                    let display_mode = match monitor_geometry.identify_display_mode(
                        &new_display_config,
                         is_fullscreen.get(),
                         is_multimonitored.get(),
                     ) {
                        Ok(dm) => dm,
                        Err(error) => {
                            warn!(%error, "Ignoring invalid display configuration");
                            return glib::source::Continue(true);
                        }
                    };

                    match display_mode {
                        DisplayMode::MultiFullscreen => {
                            let supports_fullscreen = matches!(monitor_geometry, MonitorGeometry::Multi { .. });
                            is_multimonitored.set(supports_fullscreen);
                            set_fullscreen_mode(&window, supports_fullscreen);
                        }
                        DisplayMode::SingleFullscreen => {
                            is_multimonitored.set(false);
                            set_fullscreen_mode(&window, false);
                        }
                        DisplayMode::Windowed { size } => {
                            if is_fullscreen.get() {
                                window.unfullscreen();
                            }
                            window.resize(size.width as _, size.height as _);
                        }
                    }
                    display_config.replace(new_display_config);
                    image.borrow_mut().update_display_config(&display_config.borrow());
                }
            }
            glib::source::Continue(true)
        }),
    );

    window.connect_show(move |window| {
        let UiWorkspace {
            ref is_multimonitored,
            ..
        } = *workspace;
        set_fullscreen_mode(window, is_multimonitored.get());

        if initial_display_mode.is_fullscreen() {
            window.fullscreen();
        }
    });

    window.show_all()
}

fn set_fullscreen_mode(window: &ApplicationWindow, multi_monitor: bool) {
    static WARN_ONCE: LazyLock<()> = LazyLock::new(|| {
        warn!("Not running in X11. Multi-monitor fullscreen may be disabled");
    });
    if let Some(gdk_window) = window.window() {
        if multi_monitor {
            gdk_window.set_fullscreen_mode(gdk::FullscreenMode::AllMonitors);
            if !GdkDisplay::default().unwrap().backend().is_x11() {
                *WARN_ONCE;
            }
        } else {
            gdk_window.set_fullscreen_mode(gdk::FullscreenMode::CurrentMonitor);
        }
    } else {
        warn!("GTK failed to get GDK window, cannot set fullscreen mode spanning all monitors");
    }
}
