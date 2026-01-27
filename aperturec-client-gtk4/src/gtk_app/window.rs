use super::image;
use super::input::map_gdk_key;
#[cfg(not(target_os = "macos"))]
use super::menu::create_overlay_layout;
use super::ui::UiEvent;
use aperturec_client::Connection;
use aperturec_graphics::prelude::*;
use aperturec_utils::channels::SenderExt;
use async_channel::Sender;
use libadwaita as adw;
use adw::prelude::*;
use gtk4::{self as gtk, gdk, glib};
use glib::prelude::Cast;
use std::{cell::RefCell, rc::Rc, sync::Arc};
use tracing::*;

use glib::translate::IntoGlib;

const SCROLL_TICK_THRESHOLD: f64 = 1.0;
const SCROLL_BUTTON_UP: u32 = 3;
const SCROLL_BUTTON_DOWN: u32 = 4;
const SCROLL_BUTTON_LEFT: u32 = 5;
const SCROLL_BUTTON_RIGHT: u32 = 6;
#[derive(Default)]
struct ScrollAccum {
    x: f64,
    y: f64,
}

pub struct Window {
    pub gtk_window: gtk::ApplicationWindow,
    pub drawing_area: gtk::DrawingArea,
    pub image: Rc<RefCell<image::Image>>,
}

impl Window {
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
                let global_x = origin.x + x.round() as usize;
                let global_y = origin.y + y.round() as usize;
                if let Err(error) = conn.pointer_move(global_x, global_y) {
                    warn!(%error, "failed handling motion");
                }
            }
        ));
        ec_motion
    }

    fn key_controller(conn: Arc<Connection>) -> gtk::EventControllerKey {
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
    }

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
                let global_x = origin.x + x.round() as usize;
                let global_y = origin.y + y.round() as usize;
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
                let global_x = origin.x + x.round() as usize;
                let global_y = origin.y + y.round() as usize;
                if let Err(error) = conn.mouse_button_release(button, global_x, global_y) {
                    warn!(%error, "failed handling mouse release");
                }
            }
        ));
        gc
    }

    fn drain_scroll_ticks(value: &mut f64) -> i32 {
        let ticks = (*value / SCROLL_TICK_THRESHOLD).trunc() as i32;
        *value -= ticks as f64 * SCROLL_TICK_THRESHOLD;
        ticks
    }

    fn send_scroll_button(conn: &Connection, button: u32, x: usize, y: usize) {
        if let Err(error) = conn.mouse_button_press(button, x, y) {
            warn!(%error, %button, "failed handling scroll press");
            return;
        }
        if let Err(error) = conn.mouse_button_release(button, x, y) {
            warn!(%error, %button, "failed handling scroll release");
        }
    }

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

                let global_x = origin.x + local_x.round() as usize;
                let global_y = origin.y + local_y.round() as usize;

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

    fn drawing_area(
        image: Rc<RefCell<image::Image>>,
        conn: Arc<Connection>,
        origin: Point,
    ) -> gtk::DrawingArea {
        let pointer_pos = Rc::new(RefCell::new(None));
        let drawing_area = gtk::DrawingArea::builder().focusable(true).build();
        drawing_area.add_controller(Self::motion_controller(
            conn.clone(),
            origin,
            pointer_pos.clone(),
        ));
        drawing_area.add_controller(Self::focus_controller());
        drawing_area.add_controller(Self::key_controller(conn.clone()));
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
        drawing_area
    }

    fn build_window(
        window_child: &impl IsA<gtk::Widget>,
        size: Size,
        ui_event_tx: Sender<UiEvent>,
    ) -> gtk::ApplicationWindow {
        let gtk_window: gtk::ApplicationWindow = {
            let header_bar = adw::HeaderBar::new();
            header_bar.set_show_start_title_buttons(true);
            header_bar.set_show_end_title_buttons(true);
            let layout = gtk::Box::new(gtk::Orientation::Vertical, 0);
            layout.append(&header_bar);
            layout.append(window_child);
            let adw_window = adw::ApplicationWindow::builder()
                .resizable(true)
                .title("ApertureC Client")
                .visible(false)
                .can_focus(true)
                .content(&layout)
                .build();
            adw_window.upcast()
        };
        gtk_window.set_default_size(size.width as _, size.height as _);


        let shortcut_tx = ui_event_tx.clone();
        gtk_window.connect_realize(move |window| {
            let Some(surface) = window.surface() else {
                trace!("window surface not available; skipping shortcut passthrough monitor");
                return;
            };
            let Some(toplevel) = surface.dynamic_cast_ref::<gdk::Toplevel>() else {
                warn!("window surface is not a toplevel; skipping shortcut passthrough monitor");
                return;
            };
            let shortcut_tx = shortcut_tx.clone();
            toplevel.connect_shortcuts_inhibited_notify(move |toplevel| {
                shortcut_tx
                    .send_or_warn(UiEvent::ShortcutPassthroughStatus(
                        toplevel.is_shortcuts_inhibited(),
                    ));
            });
        });

        gtk_window.connect_close_request(move |_| {
            debug!("window close requested");
            ui_event_tx.send_or_warn(UiEvent::WindowClosed);
            glib::signal::Propagation::Proceed
        });

        gtk_window
    }

    pub fn new(
        conn: Arc<Connection>,
        size: Size,
        origin: Point,
        ui_event_tx: Sender<UiEvent>,
    ) -> Window {
        debug!(?size, ?origin, "creating window");
        let image = Rc::new(RefCell::new(image::Image::new(size)));
        let drawing_area = Self::drawing_area(image.clone(), conn.clone(), origin);

        // Create overlay layout with revealer for all non-macOS modes
        #[cfg(not(target_os = "macos"))]
        let window_child = create_overlay_layout(&drawing_area);

        #[cfg(target_os = "macos")]
        let window_child = drawing_area.clone();
        let gtk_window = Self::build_window(
            &window_child,
            size,
            ui_event_tx,
        );

        Window {
            gtk_window,
            drawing_area,
            image,
        }
    }

    pub fn show(&mut self) {
        trace!("show window");
        self.gtk_window.show();
    }

    pub fn hide(&mut self) {
        trace!("hide window");
        self.gtk_window.hide();
    }

    pub fn fullscreen(&mut self) {
        debug!("enter fullscreen");
        self.gtk_window.fullscreen();
    }

    pub fn window(&mut self) {
        debug!("exit fullscreen");
        self.gtk_window.unfullscreen();
    }

    pub fn fullscreen_on_monitor(&mut self, gdk_monitor: gdk::Monitor) {
        debug!("enter fullscreen on monitor");
        self.gtk_window.fullscreen_on_monitor(&gdk_monitor);
    }

    pub fn set_shortcut_passthrough(&self, enabled: bool) {
        let Some(surface) = self.gtk_window.surface() else {
            trace!("window surface not available; skipping shortcut passthrough update");
            return;
        };
        let Some(toplevel) = surface.dynamic_cast_ref::<gdk::Toplevel>() else {
            warn!("window surface is not a toplevel; skipping shortcut passthrough update");
            return;
        };
        if enabled {
            let event: Option<&gdk::Event> = None;
            toplevel.inhibit_system_shortcuts(event);
        } else {
            toplevel.restore_system_shortcuts();
        }
    }

    // Placeholder for future content-widget cursor handling.

    pub fn set_content_size(&self, size: Size) {
        self.drawing_area
            .set_content_width(size.width as i32);
        self.drawing_area
            .set_content_height(size.height as i32);
    }
}
