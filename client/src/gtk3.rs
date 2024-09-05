use crate::client::{
    ControlMessage, CursorData, EventMessage, GoodbyeMessageBuilder, KeyEventMessageBuilder,
    MouseButtonEventMessageBuilder, PointerEventMessageBuilder, UiMessage,
};
use crate::gtk3::image::Image;

use aperturec_graphics::prelude::*;

use crossbeam::channel::{unbounded, Receiver, Sender};
use gtk::cairo::{Context, ImageSurface};
use gtk::gdk::{keys, Display, EventMask, ModifierType, ScrollDirection, WindowState};
use gtk::prelude::*;
use gtk::{glib, Adjustment, ApplicationWindow, DrawingArea, ScrolledWindow};
use keycode::{KeyMap, KeyMapping};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;
use tracing::*;

#[cfg(target_os = "windows")]
use windows::Win32::UI::Input::KeyboardAndMouse::{MapVirtualKeyW, MAPVK_VK_TO_VSC_EX};

pub mod image;

pub struct ClientSideItcChannels {
    pub ui_tx: glib::Sender<UiMessage>,
    pub img_tx: glib::Sender<Image>,
    pub notify_control_rx: Receiver<ControlMessage>,
    pub notify_control_tx: Sender<ControlMessage>,
    pub notify_event_rx: Receiver<EventMessage>,
    pub notify_event_tx: Sender<EventMessage>,
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

pub fn get_fullscreen_dims() -> (i32, i32) {
    gtk::init().expect("Failed to initialize GTK");
    let display = Display::default().expect("Failed to get default GTK display");
    let monitor = display.primary_monitor().unwrap_or_else(|| {
        if display.n_monitors() > 0 {
            display.monitor(0).expect("No GTK monitor 0")
        } else {
            display
                .monitor_at_point(0, 0)
                .expect("No GTK monitor at (0,0)")
        }
    });
    let geo = monitor.geometry();

    (geo.width(), geo.height())
}

enum KeyboardShortcut {
    ToggleFullscreen,
    Disconnect,
}

impl KeyboardShortcut {
    fn from_event(key_event: &gtk::gdk::EventKey) -> Option<Self> {
        let state = key_event.state() & ModifierType::MODIFIER_MASK;

        if !state.contains(ModifierType::CONTROL_MASK | ModifierType::MOD1_MASK) {
            None
        } else if key_event.keyval() == keys::constants::Return {
            Some(Self::ToggleFullscreen)
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
        width: i32,
        height: i32,
        fps: Duration,
        decoder_areas: &[Box2D],
    ) {
        let app = gtk::Application::builder()
            .application_id("com.zetier.aperturec.client")
            .flags(gtk::gio::ApplicationFlags::NON_UNIQUE)
            .build();

        // Need to copy decoder vector
        let decoder_areas = decoder_areas.to_vec();
        debug!(
            "GTK UI starting with {}x{} window @ {:?} FPS. {:?}",
            width, height, fps, &decoder_areas
        );

        let itc = Cell::new(Some(itc));
        app.connect_activate(move |app| {
            if let Some(itc) = itc.take() {
                build_ui(app, itc, width, height)
            }
        });

        //
        // Run with empty args to ignore the ApertureC args
        //
        app.run_with_args(&[""; 0]);
    }
}

fn init_image(size: Size) -> Image {
    let mut image = Image::new(size);

    image.with_surface(|surface| {
        let cr = Context::new(surface).expect("GTK failed to create Cairo ctx!");
        cr.set_source_rgb(0., 1., 0.);
        cr.paint().expect("GTK invalid Cairo surface state");
    });

    image
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

fn build_ui(app: &gtk::Application, itc: GtkSideItcChannels, width: i32, height: i32) {
    let window = ApplicationWindow::builder()
        .application(app)
        .default_width(width)
        .default_height(height)
        .resizable(true)
        .title("ApertureC Client")
        .build();

    let scrolled_window =
        ScrolledWindow::new(None as Option<&Adjustment>, None as Option<&Adjustment>);
    let area = DrawingArea::new();

    scrolled_window.add(&area);
    window.add(&scrolled_window);

    area.set_size_request(width, height);
    area.set_can_focus(true);
    area.add_events(
        EventMask::KEY_PRESS_MASK
            | EventMask::KEY_RELEASE_MASK
            | EventMask::BUTTON_PRESS_MASK
            | EventMask::BUTTON_RELEASE_MASK
            | EventMask::SCROLL_MASK
            | EventMask::POINTER_MOTION_MASK,
    );

    //
    // Setup Images
    //

    let image = init_image(Size::new(width as usize, height as usize));
    let _ = itc.img_tx.send(image.clone());

    let workspace = Rc::new((RefCell::new(image), itc.img_tx));

    area.connect_draw(
        glib::clone!(@weak workspace => @default-return gtk::Inhibit(false), move |_, cr| {
            let (ref image, _) = *workspace;

            image.borrow_mut().with_surface(|surface| {
                render_image(cr, surface);
            });
            gtk::Inhibit(false)
        }),
    );

    //
    // Setup Window state tracking
    //
    let window_state = Rc::new(Cell::new(WindowState::empty()));
    let ws_update = window_state.clone();
    let ws_key = window_state.clone();

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
    let notify_control_tx = itc.notify_control_tx.clone();

    //
    // Setup cursor tracking
    //
    let mut cursor_map = HashMap::new();

    //
    // Setup keyboard tracking
    //
    area.connect_key_press_event(glib::clone!(@strong window => move |_, key| {
        trace!("GTK KeyPressEvent: {:?} : {:?}", key.keyval(), key.state());

        match KeyboardShortcut::from_event(key) {
            Some(KeyboardShortcut::ToggleFullscreen) => {
                if ws_key.get().contains(WindowState::FULLSCREEN) {
                    window.unfullscreen();
                } else {
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
    }));

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

        // Motion logging is *very* noisy
        //trace!("GTK MotionEvent: {:?}", pos);

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

    window.connect_delete_event(move |_, _| {
        _ = notify_control_tx.send(ControlMessage::UiClosed(
            GoodbyeMessageBuilder::default()
                .reason(String::from("UI closed"))
                .build()
                .expect("GTK failed to build GoodbyeMessage"),
        ));

        gtk::Inhibit(false)
    });

    window.connect_window_state_event(move |_, event| {
        ws_update.set(event.new_window_state());
        gtk::Inhibit(false)
    });

    itc.img_rx.attach(
        None,
        glib::clone!(@strong area => move |updated_img| {
            let (ref image, ref tx) = *workspace;

            //
            // Replace the stale Image with the new one. The data in the images vector will be used
            // to render the display at the next redraw event.
            //
            let mut stale_img = image.replace(updated_img);

            //
            // TODO: Drawing requests should be limited to whatever our FPS cap is (passed in as
            // `_fps`). There is still some work to do here to figure out how best to implement
            // this. For now, queue drawing requests as fast as we can.
            //
            for updated_area in &image.borrow().update_history {
                let rect = updated_area.to_rect();
                area.queue_draw_area(
                    rect.origin.x as i32,
                    rect.origin.y as i32,
                    rect.size.width as i32,
                    rect.size.height as i32,
                );
            }

            // Copy updated areas from the current frame to the stale frame
            stale_img
                .copy_updates(&image.borrow())
                .unwrap_or_else(|err| warn!("GTK Failed to copy Image updates: {}", err));

            tx.send(stale_img)
                .unwrap_or_else(|err| warn!("GTK failed to send image to client: {}", err));

            // Image swap logging is *very* noisy
            /*trace!(
                "GTK image update from decoder_{} : {:?}",
                decoder_id,
                origin
            );*/

            glib::source::Continue(true)
        }),
    );

    itc.ui_rx.attach(
        None,
        glib::clone!(@strong window => move |msg| {
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
                    let cursor = gtk::gdk::Cursor::from_pixbuf(&Display::default().unwrap(), &pixbuf, x_hot, y_hot);
                    cursor_map.insert(id, cursor);

                    area.window().unwrap().set_cursor(Some(cursor_map.get(&id).unwrap()))
                }
                UiMessage::CursorChange { id } => {
                    match cursor_map.get(&id) {
                        Some(cursor) => area.window().unwrap().set_cursor(Some(cursor)),
                        None => warn!("No cursor for ID {}. Ignoring", id),
                    }
                }
            }
            glib::source::Continue(true)
        }),
    );

    //
    // Set to fullscreen
    //
    if get_fullscreen_dims() == (width, height) {
        window.set_default_width(width / 2);
        window.set_default_height(height / 2);
        window.fullscreen();
    }

    debug!("GTK UI built, showing {}x{} window", width, height);
    window.show_all()
}
