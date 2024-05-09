mod image;

use crate::client::{
    ClientDecoder, Configuration, ControlMessage, EventMessage, GoodbyeMessageBuilder,
    KeyEventMessageBuilder, MouseButtonEventMessageBuilder, Point, PointerEventMessageBuilder,
    UiMessage,
};
use crate::gtk3::image::Image;

use aperturec_trace::log;
use gtk::cairo::{Context, ImageSurface};
use gtk::gdk::{keys, Display, EventMask, ModifierType, ScrollDirection, WindowState};
use gtk::prelude::*;
use gtk::{glib, Adjustment, ApplicationWindow, DrawingArea, ScrolledWindow};
use keycode::{KeyMap, KeyMapping};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

#[cfg(target_os = "windows")]
use windows::Win32::UI::Input::KeyboardAndMouse::{MapVirtualKeyW, MAPVK_VK_TO_VSC_EX};

use crossbeam_channel::{unbounded, Receiver, Sender};

pub struct ItcChannels {
    control_to_ui_rx: Cell<Option<glib::Receiver<UiMessage>>>,
    pub control_to_ui_tx: glib::Sender<UiMessage>,

    pub notify_control_tx: Sender<ControlMessage>,
    pub notify_control_rx: Cell<Option<Receiver<ControlMessage>>>,

    pub event_from_ui_rx: Cell<Option<Receiver<EventMessage>>>,
    event_from_ui_tx: Sender<EventMessage>,

    img_from_decoder_rx: Cell<Option<glib::Receiver<Image>>>,
    pub img_from_decoder_tx: glib::Sender<Image>,

    pub img_to_decoder_rxs: Vec<Cell<Option<Receiver<Image>>>>,
    img_to_decoder_txs: Vec<Cell<Option<Sender<Image>>>>,
}

impl ItcChannels {
    pub fn new(config: &Configuration) -> Self {
        let (control_to_ui_tx, control_to_ui_rx) =
            glib::MainContext::channel(glib::Priority::default());

        let (notify_control_tx, notify_control_rx) = unbounded();
        let (event_from_ui_tx, event_from_ui_rx) = unbounded();

        let (img_from_decoder_tx, img_from_decoder_rx) =
            glib::MainContext::channel(glib::Priority::default());

        let mut this = Self {
            control_to_ui_rx: Cell::new(Some(control_to_ui_rx)),
            control_to_ui_tx,
            notify_control_rx: Cell::new(Some(notify_control_rx)),
            notify_control_tx,
            event_from_ui_rx: Cell::new(Some(event_from_ui_rx)),
            event_from_ui_tx,
            img_from_decoder_rx: Cell::new(Some(img_from_decoder_rx)),
            img_from_decoder_tx,
            img_to_decoder_rxs: Vec::new(),
            img_to_decoder_txs: Vec::new(),
        };

        // Generate channels for the decoders
        for _ in 0..config.decoder_max {
            let (tx, rx) = unbounded();
            this.img_to_decoder_rxs.push(Cell::new(Some(rx)));
            this.img_to_decoder_txs.push(Cell::new(Some(tx)));
        }

        this
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
        itc: ItcChannels,
        width: i32,
        height: i32,
        fps: Duration,
        decoders: &[ClientDecoder],
    ) {
        let app = gtk::Application::builder()
            .application_id("com.zetier.aperturec.client")
            .build();

        // Need to copy decoder vector
        let decoders = decoders.to_vec();
        log::debug!(
            "GTK UI starting with {}x{} window @ {:?} FPS. {:?}",
            width,
            height,
            fps,
            &decoders
        );

        app.connect_activate(move |app| {
            build_ui(app, &itc, width, height, fps, &decoders);
        });

        //
        // Run with empty args to ignore the ApertureC args
        //
        app.run_with_args(&[""; 0]);
    }
}

fn init_image(width: i32, height: i32) -> Image {
    let mut image = Image::new(width.try_into().unwrap(), height.try_into().unwrap());

    image.with_surface(|surface| {
        let cr = Context::new(surface).expect("GTK failed to create Cairo ctx!");
        cr.set_source_rgb(0., 1., 0.);
        cr.paint().expect("GTK invalid Cairo surface state");
    });

    image
}

fn render_image(cr: &Context, image: &ImageSurface, origin: (u32, u32), dimensions: (u32, u32)) {
    let (x, y) = (origin.0 as f64, origin.1 as f64);
    let (w, h) = (dimensions.0 as f64, dimensions.1 as f64);

    let (clip_x0, clip_y0, clip_x1, clip_y1) = cr
        .clip_extents()
        .expect("GTK failed to get Cairo clip extents!");
    if clip_x0 >= x + w || clip_y0 >= y + h || clip_x1 <= x || clip_y1 <= y {
        // No need to re-draw
        return;
    }

    cr.set_source_surface(image, x, y)
        .expect("GTK surface is in an invalid state");

    // Temporarily set clip to the current surface to prevent EXTEND_PAD from interfering with
    // adjacent Surfaces
    cr.save().expect("Failed to save cairo context");
    cr.rectangle(x, y, w, h);
    cr.clip();

    // Surfaces use EXTEND_NONE by default, however, we need to coerce the underlying pattern to
    // use EXTEND_PAD to deal with transparent edge pixels on scaled displays
    cr.source().set_extend(gtk::cairo::Extend::Pad);

    cr.paint().expect("Invalid cairo surface state");
    cr.restore().expect("Failed to restore cairo context");

    cr.set_source_rgba(0., 0., 0., 0.);
}

fn build_ui(
    app: &gtk::Application,
    itc: &ItcChannels,
    width: i32,
    height: i32,
    _fps: Duration,
    decoders: &[ClientDecoder],
) {
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

    //NOTE: This presumes each decoder is the same size
    let image0 = init_image(
        decoders[0].width.try_into().unwrap(),
        decoders[0].height.try_into().unwrap(),
    );

    let mut images = Vec::new();
    let mut txs = Vec::new();

    for (i, decoder) in decoders.iter().enumerate() {
        //
        // Generate two Images for each ClientDecoder: one is pushed on the local image vec to be
        // displayed at the first (and subsequent) redraw events. The other is sent to the
        // ClientDecoder over the associated unbounded channel so that new data can be written to
        // it when a FramebufferUpdate is received. The images are swapped as new data is received
        // from the ClientDecoder.
        //
        let origin = (decoder.origin.0, decoder.origin.1);
        images.push(RefCell::new(image0.clone_with_decoder_info(i, origin)));
        let tx = itc.img_to_decoder_txs[i].take().unwrap();
        let _ = tx.send(image0.clone_with_decoder_info(i, origin));
        txs.push(tx);
    }

    let workspace = Rc::new((images, txs));

    area.connect_draw(
        glib::clone!(@weak workspace => @default-return gtk::Inhibit(false), move |_, cr| {
            let (ref images, _) = *workspace;

            for image in images.iter() {
                let origin = image.borrow().origin();
                let dimensions = image.borrow().dims();
                image.borrow_mut().with_surface(|surface| {
                    render_image(cr, surface, origin, dimensions);
                });
            }
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
    let last_mouse_pos = Rc::new(Cell::new(Point(0, 0)));
    let lm_motion = last_mouse_pos.clone();
    let lm_button_down = last_mouse_pos.clone();
    let lm_button_up = last_mouse_pos.clone();
    let lm_scroll = last_mouse_pos.clone();

    //
    // Setup event channel Senders
    //
    let key_press_tx = itc.event_from_ui_tx.clone();
    let key_release_tx = itc.event_from_ui_tx.clone();
    let button_press_tx = itc.event_from_ui_tx.clone();
    let button_release_tx = itc.event_from_ui_tx.clone();
    let scroll_press_tx = itc.event_from_ui_tx.clone();
    let pointer_motion_tx = itc.event_from_ui_tx.clone();
    let notify_control_tx = itc.notify_control_tx.clone();

    //
    // Setup keyboard tracking
    //
    area.connect_key_press_event(glib::clone!(@strong window => move |_, key| {
        log::trace!("GTK KeyPressEvent: {:?} : {:?}", key.keyval(), key.state());

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
                    log::warn!("GTK failed to tx KeyEventMessage: {}", err)
                });
            },
        }

        gtk::Inhibit(false)
    }));

    area.connect_key_release_event(glib::clone!(@strong window => move |_, key| {
        log::trace!("GTK KeyReleaseEvent: {:?} : {:?}", key.keyval(), key.state());

        key_release_tx.send(
            EventMessage::KeyEventMessage(
                KeyEventMessageBuilder::default()
                    .key(gtk_key_to_x11(key).into())
                    .is_pressed(false)
                    .build()
                    .expect("GTK failed to build KeyEventMessage!")
            )
        ).unwrap_or_else(|err| {
            log::warn!("GTK failed to tx KeyEventMessage: {}", err)
        });

        gtk::Inhibit(false)
    }));

    area.connect_button_press_event(
        glib::clone!(@strong window => move |_, event| {
            log::trace!("GTK ButtonPressEvent: {:?} @ {:?}", event.button(), lm_button_down.as_ref().get());

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
                log::warn!("GTK failed to tx MouseButtonEventMessage: {}", err)
            });

            gtk::Inhibit(false)
        }),
    );

    area.connect_button_release_event(
        glib::clone!(@strong window => move |_, event| {
            log::trace!("GTK ButtonReleaseEvent: {:?} @ {:?}", event.button(), lm_button_up.as_ref().get());

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
                log::warn!("GTK failed to tx MouseButtonEventMessage: {}", err)
            });

            gtk::Inhibit(false)
        }),
    );

    area.connect_scroll_event(glib::clone!(@strong window => move |_, event| {
        let button = match event.direction() {
            ScrollDirection::Up => 3,
            ScrollDirection::Down => 4,
            ScrollDirection::Left => 5,
            ScrollDirection::Right => 6,
            _ => 0
        };

        log::trace!("GTK ButtonPressEvent: {:?} @ {:?} (scroll)", button, lm_scroll.as_ref().get());

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
            log::warn!("GTK failed to tx MouseButtonEventMessage: {}", err)
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
            log::warn!("GTK failed to tx MouseButtonEventMessage: {}", err)
        });

        gtk::Inhibit(false)
    }));

    area.connect_motion_notify_event(move |_, event| {
        let pos = Point(event.position().0 as u32, event.position().1 as u32);
        lm_motion.set(pos);

        // Motion logging is *very* noisy
        //log::trace!("GTK MotionEvent: {:?}", pos);

        pointer_motion_tx
            .send(EventMessage::PointerEventMessage(
                PointerEventMessageBuilder::default()
                    .pos(lm_motion.as_ref().get())
                    .build()
                    .expect("GTK failed to build PointerEventMessage!"),
            ))
            .unwrap_or_else(|err| log::warn!("GTK failed to tx PointerEventMessage: {}", err));

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

    itc.img_from_decoder_rx
        .take()
        .expect("GTK failed to attach decoder channel!")
        .attach(None, move |updated_img| {
            let (ref images, ref txs) = *workspace;
            let decoder_id = updated_img.decoder_id;

            let tx = &txs[decoder_id];
            let origin = updated_img.origin_signed();
            let dimension = updated_img.dims_signed();

            //
            // Replace the stale Image with the new one. The data in the images vector will be used
            // to render the display at the next redraw event.
            //
            let mut stale_img = images[decoder_id].replace(updated_img);

            //
            // TODO: Drawing requests should be limited to whatever our FPS cap is (passed in as
            // `_fps`). There is still some work to do here to figure out how best to implement
            // this. For now, queue drawing requests as fast as we can.
            //
            area.queue_draw_area(origin.0, origin.1, dimension.0, dimension.1);

            // Copy updated areas from the current frame to the stale frame
            stale_img
                .copy_updates(&images[decoder_id].borrow())
                .unwrap_or_else(|err| log::warn!("GTK Failed to copy Image updates: {}", err));

            let _ = tx.send(stale_img);

            // Image swap logging is *very* noisy
            /*trace!(
                "GTK image update from decoder_{} : {:?}",
                decoder_id,
                origin
            );*/

            glib::source::Continue(true)
        });

    itc.control_to_ui_rx
        .take()
        .expect("GTK failed to attach control channel!")
        .attach(
            None,
            glib::clone!(@strong window => move |msg| {
                    match msg {
                        UiMessage::QuitMessage(msg) => {
                            log::debug!("GTK received shutdown notification: {}", msg);
                            window.close();
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

    log::debug!("GTK UI built, showing {}x{} window", width, height);
    window.show_all()
}
