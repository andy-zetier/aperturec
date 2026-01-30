//! GTK window implementation and input controller wiring.

#[cfg(any(target_os = "linux", target_os = "windows"))]
use super::keyboard_grab::KeyboardGrab;
use super::keyboard_input::KeyboardEvent;
#[cfg(any(target_os = "linux", target_os = "windows"))]
use super::menu::bind_overlay_menu;
use super::{DEFAULT_RESOLUTION, image, input::map_gdk_key, ui::UiEvent};

use aperturec_client::Connection;
use aperturec_graphics::prelude::*;
use aperturec_utils::channels::SenderExt;

use async_channel::Sender;
use gtk4::{
    self as gtk, gdk, gio,
    glib::{self, subclass::prelude::ObjectSubclassExt, translate::IntoGlib},
    prelude::*,
};
use std::{cell::RefCell, rc::Rc, sync::Arc};
use tracing::*;

/// Accumulated scroll delta before emitting a discrete scroll tick.
const SCROLL_TICK_THRESHOLD: f64 = 1.0;
/// Button index for scroll-up events.
const SCROLL_BUTTON_UP: u32 = 3;
/// Button index for scroll-down events.
const SCROLL_BUTTON_DOWN: u32 = 4;
/// Button index for scroll-left events.
const SCROLL_BUTTON_LEFT: u32 = 5;
/// Button index for scroll-right events.
const SCROLL_BUTTON_RIGHT: u32 = 6;
/// Accumulator for fractional scroll deltas.
#[derive(Default)]
struct ScrollAccum {
    /// Accumulated horizontal scroll delta.
    x: f64,
    /// Accumulated vertical scroll delta.
    y: f64,
}

/// GTK subclass implementation details for `ClientWindow`.
mod imp {
    use gtk4::subclass::prelude::*;
    use gtk4::{self as gtk, CompositeTemplate, TemplateChild, glib};

    /// Template-backed widgets used by the client window.
    #[derive(Debug, Default, CompositeTemplate)]
    #[template(resource = "/com/aperturec/client/ui/window.ui")]
    pub struct ClientWindow {
        /// Drawing area that renders the remote desktop.
        #[template_child]
        pub drawing_area: TemplateChild<gtk::DrawingArea>,
        /// Revealer used for the overlay menu.
        #[template_child]
        pub menu_revealer: TemplateChild<gtk::Revealer>,
        /// Menu bar widget in the overlay.
        #[template_child]
        pub menu_bar: TemplateChild<gtk::PopoverMenuBar>,
        /// Button that shows the overlay menu.
        #[template_child]
        pub show_button: TemplateChild<gtk::Button>,
        /// Button that hides the overlay menu.
        #[template_child]
        pub hide_button: TemplateChild<gtk::Button>,
        /// Container for the show overlay controls.
        #[template_child]
        pub show_overlay: TemplateChild<gtk::Box>,
        /// Container for the hide overlay controls.
        #[template_child]
        pub hide_overlay: TemplateChild<gtk::Box>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ClientWindow {
        const NAME: &'static str = "AperturecClientWindow";
        type Type = super::ClientWindow;
        type ParentType = gtk::ApplicationWindow;

        fn class_init(klass: &mut Self::Class) {
            Self::bind_template(klass);
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for ClientWindow {}
    impl WidgetImpl for ClientWindow {}
    impl WindowImpl for ClientWindow {}
    impl ApplicationWindowImpl for ClientWindow {}
}

// GTK application window subclass backed by the composite template.
glib::wrapper! {
    pub struct ClientWindow(ObjectSubclass<imp::ClientWindow>)
        @extends gtk::ApplicationWindow, gtk::Window, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget, gtk::Native,
            gtk::Root, gtk::ShortcutManager, gio::ActionGroup, gio::ActionMap;
}

impl ClientWindow {
    /// Construct a new application window instance.
    pub fn new() -> Self {
        glib::Object::builder().build()
    }

    /// Access the internal template implementation.
    fn imp(&self) -> &imp::ClientWindow {
        imp::ClientWindow::from_obj(self)
    }
}

/// Wrapper around a GTK window plus its drawing surface and backing image.
pub struct Window {
    /// The GTK window instance.
    pub gtk_window: ClientWindow,
    /// The drawing area used for rendering.
    pub drawing_area: gtk::DrawingArea,
    /// Backing image buffer for draw events.
    pub image: Rc<RefCell<image::Image>>,
}

impl Window {
    /// Convert local widget coordinates to global coordinates, dropping invalid values.
    fn global_coords(origin: Point, x: f64, y: f64) -> Option<(usize, usize)> {
        if !x.is_finite() || !y.is_finite() {
            return None;
        }
        if x < 0.0 || y < 0.0 {
            return None;
        }
        let x_rounded = x.round();
        let y_rounded = y.round();
        if x_rounded < 0.0 || y_rounded < 0.0 {
            return None;
        }
        let x_u = x_rounded as usize;
        let y_u = y_rounded as usize;
        let global_x = origin.x.checked_add(x_u)?;
        let global_y = origin.y.checked_add(y_u)?;
        Some((global_x, global_y))
    }

    /// Build a motion controller that forwards pointer movement.
    fn motion_controller(
        conn: Arc<Connection>,
        origin: Point,
        pointer_pos: Rc<RefCell<Option<(f64, f64)>>>,
    ) -> gtk::EventControllerMotion {
        let ec_motion = gtk::EventControllerMotion::new();
        ec_motion.connect_motion(glib::clone!(
            #[weak]
            conn,
            #[strong]
            pointer_pos,
            #[strong]
            origin,
            move |ec, x, y| {
                trace!(%x, %y, "motion");
                *pointer_pos.borrow_mut() = Some((x, y));
                if let Some(widget) = ec.widget() {
                    widget.grab_focus();
                }
                if let Some((global_x, global_y)) = Self::global_coords(origin, x, y) {
                    if let Err(error) = conn.pointer_move(global_x, global_y) {
                        warn!(%error, "failed handling motion");
                    }
                } else {
                    trace!(%x, %y, "dropping motion with invalid coordinates");
                }
            }
        ));
        ec_motion
    }

    /// Build a key controller that forwards key presses/releases.
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    fn key_controller(
        keyboard_event_tx: Sender<KeyboardEvent>,
        keyboard_grab: KeyboardGrab,
    ) -> gtk::EventControllerKey {
        let ec_key = gtk::EventControllerKey::new();
        ec_key.connect_key_pressed(glib::clone!(
            #[strong]
            keyboard_event_tx,
            #[strong]
            keyboard_grab,
            move |_, keyval, keycode, _state| {
                if keyboard_grab.should_ignore_gtk_keys() {
                    return glib::signal::Propagation::Stop;
                }
                if let Some(code) = map_gdk_key(keyval, keycode) {
                    debug!(keycode = code, "press");
                    keyboard_event_tx.send_or_warn(KeyboardEvent {
                        code,
                        pressed: true,
                    });
                } else {
                    warn!(keyval = %keyval.into_glib(), %keycode, "unmapped key press; dropping");
                }
                glib::signal::Propagation::Stop
            }
        ));
        ec_key.connect_key_released(glib::clone!(
            #[strong]
            keyboard_event_tx,
            #[strong]
            keyboard_grab,
            move |_, keyval, keycode, _state| {
                if keyboard_grab.should_ignore_gtk_keys() {
                    return;
                }
                if let Some(code) = map_gdk_key(keyval, keycode) {
                    debug!(keycode = code, "release");
                    keyboard_event_tx.send_or_warn(KeyboardEvent {
                        code,
                        pressed: false,
                    });
                } else {
                    warn!(keyval = %keyval.into_glib(), %keycode, "unmapped key release; dropping");
                }
            }
        ));
        ec_key
    }

    /// Build a key controller that forwards key presses/releases.
    #[cfg(target_os = "macos")]
    fn key_controller(keyboard_event_tx: Sender<KeyboardEvent>) -> gtk::EventControllerKey {
        let ec_key = gtk::EventControllerKey::new();
        ec_key.connect_key_pressed(glib::clone!(
            #[strong]
            keyboard_event_tx,
            move |_, keyval, keycode, _state| {
                if let Some(code) = map_gdk_key(keyval, keycode) {
                    debug!(keycode = code, "press");
                    keyboard_event_tx.send_or_warn(KeyboardEvent {
                        code,
                        pressed: true,
                    });
                } else {
                    warn!(keyval = %keyval.into_glib(), %keycode, "unmapped key press; dropping");
                }
                glib::signal::Propagation::Stop
            }
        ));
        ec_key.connect_key_released(glib::clone!(
            #[strong]
            keyboard_event_tx,
            move |_, keyval, keycode, _state| {
                if let Some(code) = map_gdk_key(keyval, keycode) {
                    debug!(keycode = code, "release");
                    keyboard_event_tx.send_or_warn(KeyboardEvent {
                        code,
                        pressed: false,
                    });
                } else {
                    warn!(keyval = %keyval.into_glib(), %keycode, "unmapped key release; dropping");
                }
            }
        ));
        ec_key
    }
    /// Build a focus controller for logging focus transitions.
    fn focus_controller() -> gtk::EventControllerFocus {
        let ec_focus = gtk::EventControllerFocus::new();
        ec_focus.connect_enter(|_| {
            debug!("entered");
        });
        ec_focus.connect_leave(|_| {
            debug!("leave");
        });
        ec_focus
    }

    /// Build a click gesture controller that forwards mouse button events.
    fn gesture_controller(
        conn: Arc<Connection>,
        origin: Point,
        pointer_pos: Rc<RefCell<Option<(f64, f64)>>>,
    ) -> gtk::GestureClick {
        let gc = gtk::GestureClick::builder()
            .button(0)
            .exclusive(false)
            .build();
        gc.connect_pressed(glib::clone!(
            #[weak]
            conn,
            #[strong]
            origin,
            #[strong]
            pointer_pos,
            move |gesture, _, x, y| {
                // GTK4 buttons start at 1, while the server expects the buttons to start at 0
                *pointer_pos.borrow_mut() = Some((x, y));
                let raw = gesture.current_button();
                if raw == 0 {
                    return;
                }
                let button = raw - 1;
                debug!(%button, %x, %y, "press");
                let Some((global_x, global_y)) = Self::global_coords(origin, x, y) else {
                    trace!(%x, %y, "dropping press with invalid coordinates");
                    return;
                };
                if let Err(error) = conn.mouse_button_press(button, global_x, global_y) {
                    warn!(%error, "failed handling mouse press");
                }
            }
        ));
        gc.connect_released(glib::clone!(
            #[weak]
            conn,
            #[strong]
            origin,
            #[strong]
            pointer_pos,
            move |gesture, _, x, y| {
                // GTK4 buttons start at 1, while the server expects the buttons to start at 0
                *pointer_pos.borrow_mut() = Some((x, y));
                let raw = gesture.current_button();
                if raw == 0 {
                    return;
                }
                let button = raw - 1;
                debug!(%button, %x, %y, "release");
                let Some((global_x, global_y)) = Self::global_coords(origin, x, y) else {
                    trace!(%x, %y, "dropping release with invalid coordinates");
                    return;
                };
                if let Err(error) = conn.mouse_button_release(button, global_x, global_y) {
                    warn!(%error, "failed handling mouse release");
                }
            }
        ));
        gc
    }

    /// Convert accumulated scroll value into whole tick counts.
    fn drain_scroll_ticks(value: &mut f64) -> i32 {
        let ticks = (*value / SCROLL_TICK_THRESHOLD).trunc() as i32;
        *value -= ticks as f64 * SCROLL_TICK_THRESHOLD;
        ticks
    }

    /// Send a scroll button press + release pair.
    fn send_scroll_button(conn: &Connection, button: u32, x: usize, y: usize) {
        if let Err(error) = conn.mouse_button_press(button, x, y) {
            warn!(%error, %button, "failed handling scroll press");
            return;
        }
        if let Err(error) = conn.mouse_button_release(button, x, y) {
            warn!(%error, %button, "failed handling scroll release");
        }
    }

    /// Build a scroll controller that translates deltas into button events.
    fn scroll_controller(
        conn: Arc<Connection>,
        origin: Point,
        pointer_pos: Rc<RefCell<Option<(f64, f64)>>>,
    ) -> gtk::EventControllerScroll {
        let flags =
            gtk::EventControllerScrollFlags::BOTH_AXES | gtk::EventControllerScrollFlags::DISCRETE;
        let ec_scroll = gtk::EventControllerScroll::new(flags);
        let accum = Rc::new(RefCell::new(ScrollAccum::default()));
        ec_scroll.connect_scroll(glib::clone!(
            #[weak]
            conn,
            #[strong]
            origin,
            #[strong]
            accum,
            #[strong]
            pointer_pos,
            #[upgrade_or]
            glib::Propagation::Stop,
            move |controller, dx, dy| {
                trace!(%dx, %dy, "scroll");
                if let Some(widget) = controller.widget() {
                    widget.grab_focus();
                }
                let event_pos = controller
                    .current_event()
                    .and_then(|event| event.position());
                let pos = if let Some(pos) = event_pos {
                    *pointer_pos.borrow_mut() = Some(pos);
                    Some(pos)
                } else {
                    *pointer_pos.borrow()
                };
                let Some((local_x, local_y)) = pos else {
                    trace!("scroll event missing position and no cached pointer; dropping");
                    return glib::Propagation::Stop;
                };

                let mut accum = accum.borrow_mut();
                accum.x += dx;
                accum.y += dy;
                let ticks_x = Self::drain_scroll_ticks(&mut accum.x);
                let ticks_y = Self::drain_scroll_ticks(&mut accum.y);
                drop(accum);

                if ticks_x == 0 && ticks_y == 0 {
                    return glib::Propagation::Stop;
                }

                let Some((global_x, global_y)) = Self::global_coords(origin, local_x, local_y)
                else {
                    trace!(%local_x, %local_y, "dropping scroll with invalid coordinates");
                    return glib::Propagation::Stop;
                };

                if ticks_y > 0 {
                    for _ in 0..ticks_y {
                        Self::send_scroll_button(&conn, SCROLL_BUTTON_DOWN, global_x, global_y);
                    }
                } else if ticks_y < 0 {
                    for _ in 0..ticks_y.abs() {
                        Self::send_scroll_button(&conn, SCROLL_BUTTON_UP, global_x, global_y);
                    }
                }

                if ticks_x > 0 {
                    for _ in 0..ticks_x {
                        Self::send_scroll_button(&conn, SCROLL_BUTTON_RIGHT, global_x, global_y);
                    }
                } else if ticks_x < 0 {
                    for _ in 0..ticks_x.abs() {
                        Self::send_scroll_button(&conn, SCROLL_BUTTON_LEFT, global_x, global_y);
                    }
                }

                glib::Propagation::Stop
            }
        ));
        ec_scroll
    }

    /// Configure the drawing area with input controllers and draw callback.
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    fn configure_drawing_area(
        drawing_area: &gtk::DrawingArea,
        image: Rc<RefCell<image::Image>>,
        conn: Arc<Connection>,
        origin: Point,
        keyboard_event_tx: Sender<KeyboardEvent>,
        keyboard_grab: KeyboardGrab,
    ) {
        drawing_area.set_focusable(true);
        let pointer_pos = Rc::new(RefCell::new(None));
        drawing_area.add_controller(Self::motion_controller(
            conn.clone(),
            origin,
            pointer_pos.clone(),
        ));
        drawing_area.add_controller(Self::focus_controller());
        drawing_area.add_controller(Self::key_controller(keyboard_event_tx, keyboard_grab));
        drawing_area.add_controller(Self::gesture_controller(
            conn.clone(),
            origin,
            pointer_pos.clone(),
        ));
        drawing_area.add_controller(Self::scroll_controller(conn, origin, pointer_pos));
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
    }

    #[cfg(target_os = "macos")]
    fn configure_drawing_area(
        drawing_area: &gtk::DrawingArea,
        image: Rc<RefCell<image::Image>>,
        conn: Arc<Connection>,
        origin: Point,
        keyboard_event_tx: Sender<KeyboardEvent>,
    ) {
        drawing_area.set_focusable(true);
        let pointer_pos = Rc::new(RefCell::new(None));
        drawing_area.add_controller(Self::motion_controller(
            conn.clone(),
            origin,
            pointer_pos.clone(),
        ));
        drawing_area.add_controller(Self::focus_controller());
        drawing_area.add_controller(Self::key_controller(keyboard_event_tx));
        drawing_area.add_controller(Self::gesture_controller(
            conn.clone(),
            origin,
            pointer_pos.clone(),
        ));
        drawing_area.add_controller(Self::scroll_controller(conn, origin, pointer_pos));
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
    }

    /// Create a new window with a drawing area and input controllers.
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    pub fn new(
        conn: Arc<Connection>,
        size: Size,
        origin: Point,
        ui_event_tx: Sender<UiEvent>,
        keyboard_event_tx: Sender<KeyboardEvent>,
        keyboard_grab: KeyboardGrab,
    ) -> Window {
        debug!(?size, ?origin, "creating window");
        let image = Rc::new(RefCell::new(image::Image::new(size)));
        let gtk_window = ClientWindow::new();
        gtk_window.set_default_size(size.width as _, size.height as _);
        gtk_window.set_size_request(
            DEFAULT_RESOLUTION.width as i32,
            DEFAULT_RESOLUTION.height as i32,
        );
        gtk_window.set_decorated(true);

        let imp = gtk_window.imp();
        let drawing_area = imp.drawing_area.get();
        Self::configure_drawing_area(
            &drawing_area,
            image.clone(),
            conn.clone(),
            origin,
            keyboard_event_tx,
            keyboard_grab,
        );

        #[cfg(any(target_os = "linux", target_os = "windows"))]
        {
            bind_overlay_menu(
                &imp.menu_bar.get(),
                &imp.menu_revealer.get(),
                &imp.show_button.get(),
                &imp.hide_button.get(),
                &imp.show_overlay.get(),
                &imp.hide_overlay.get(),
            );
        }
        #[cfg(target_os = "macos")]
        {
            let menu_revealer = imp.menu_revealer.get();
            let show_overlay = imp.show_overlay.get();
            let hide_overlay = imp.hide_overlay.get();
            menu_revealer.set_visible(false);
            show_overlay.set_visible(false);
            hide_overlay.set_visible(false);
        }

        gtk_window.connect_close_request(move |_| {
            debug!("window close requested");
            ui_event_tx.send_or_warn(UiEvent::WindowClosed);
            glib::signal::Propagation::Proceed
        });

        Window {
            gtk_window,
            drawing_area,
            image,
        }
    }

    #[cfg(target_os = "macos")]
    pub fn new(
        conn: Arc<Connection>,
        size: Size,
        origin: Point,
        ui_event_tx: Sender<UiEvent>,
        keyboard_event_tx: Sender<KeyboardEvent>,
    ) -> Window {
        debug!(?size, ?origin, "creating window");
        let image = Rc::new(RefCell::new(image::Image::new(size)));
        let gtk_window = ClientWindow::new();
        gtk_window.set_default_size(size.width as _, size.height as _);
        gtk_window.set_size_request(
            DEFAULT_RESOLUTION.width as i32,
            DEFAULT_RESOLUTION.height as i32,
        );
        gtk_window.set_decorated(true);

        let imp = gtk_window.imp();
        let drawing_area = imp.drawing_area.get();
        Self::configure_drawing_area(
            &drawing_area,
            image.clone(),
            conn.clone(),
            origin,
            keyboard_event_tx,
        );

        let menu_revealer = imp.menu_revealer.get();
        let show_overlay = imp.show_overlay.get();
        let hide_overlay = imp.hide_overlay.get();
        menu_revealer.set_visible(false);
        show_overlay.set_visible(false);
        hide_overlay.set_visible(false);

        gtk_window.connect_close_request(move |_| {
            debug!("window close requested");
            ui_event_tx.send_or_warn(UiEvent::WindowClosed);
            glib::signal::Propagation::Proceed
        });

        Window {
            gtk_window,
            drawing_area,
            image,
        }
    }

    /// Show the window.
    pub fn show(&mut self) {
        trace!("show window");
        self.gtk_window.show();
    }

    /// Hide the window.
    pub fn hide(&mut self) {
        trace!("hide window");
        self.gtk_window.hide();
    }

    /// Enter fullscreen on the current monitor.
    pub fn fullscreen(&mut self) {
        debug!("enter fullscreen");
        self.gtk_window.fullscreen();
    }

    /// Exit fullscreen and return to windowed mode.
    pub fn window(&mut self) {
        debug!("exit fullscreen");
        self.gtk_window.unfullscreen();
    }

    /// Enter fullscreen on the specified monitor.
    pub fn fullscreen_on_monitor(&mut self, gdk_monitor: gdk::Monitor) {
        debug!("enter fullscreen on monitor");
        self.gtk_window.fullscreen_on_monitor(&gdk_monitor);
    }

    /// Set the cursor for the drawing area.
    pub fn set_cursor(&self, cursor: Option<&gdk::Cursor>) {
        self.drawing_area.set_cursor(cursor);
    }
}
