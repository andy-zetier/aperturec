//! Linux keyboard grab using GTK's system shortcut inhibition.

use super::super::keyboard_input::KeyboardEvent;
use super::super::ui::UiEvent;

use aperturec_utils::channels::SenderExt;

use async_channel::Sender;
use gtk4::{self as gtk, gdk, prelude::*};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use tracing::*;

/// Linux keyboard grab controller.
#[derive(Clone)]
pub struct KeyboardGrab {
    inner: Rc<KeyboardGrabInner>,
}

struct KeyboardGrabInner {
    ui_event_tx: Sender<UiEvent>,
    windows: RefCell<Vec<gtk::Window>>,
    requested: Cell<bool>,
    inhibited: Cell<bool>,
    focused: Cell<bool>,
    active: Cell<bool>,
}

impl KeyboardGrab {
    /// Create a keyboard grab controller.
    pub fn new(ui_event_tx: Sender<UiEvent>, _keyboard_event_tx: Sender<KeyboardEvent>) -> Self {
        Self {
            inner: Rc::new(KeyboardGrabInner {
                ui_event_tx,
                windows: RefCell::new(Vec::new()),
                requested: Cell::new(false),
                inhibited: Cell::new(false),
                focused: Cell::new(false),
                active: Cell::new(false),
            }),
        }
    }

    /// Register a GTK window for shortcut inhibition state updates.
    pub fn register_window(&self, window: &gtk::Window) {
        self.inner.windows.borrow_mut().push(window.clone());

        let inner = Rc::clone(&self.inner);
        window.connect_is_active_notify(move |_| {
            inner.ui_event_tx.send_or_warn(UiEvent::WindowFocusChanged);
        });

        let inner = Rc::clone(&self.inner);
        window.connect_realize(move |window| {
            let Some(surface) = window.surface() else {
                trace!("window surface not available; skipping keyboard grab monitor");
                return;
            };
            let Some(toplevel) = surface.dynamic_cast_ref::<gdk::Toplevel>() else {
                warn!("window surface is not a toplevel; skipping keyboard grab monitor");
                return;
            };
            let inner = Rc::clone(&inner);
            toplevel.connect_shortcuts_inhibited_notify(move |toplevel| {
                inner.set_inhibited(toplevel.is_shortcuts_inhibited());
            });
        });
    }

    /// Enable or disable system shortcut inhibition across registered windows.
    pub fn set_enabled(&self, enabled: bool) {
        if self.inner.requested.get() == enabled {
            return;
        }
        self.inner.requested.set(enabled);
        self.apply_request();
    }

    /// Update whether the GTK window is focused.
    pub fn set_focused(&self, focused: bool) {
        if self.inner.focused.get() == focused {
            return;
        }
        self.inner.focused.set(focused);
        self.inner.update_active();
    }

    /// Returns true when the keyboard grab is active.
    pub fn active(&self) -> bool {
        self.inner.active.get()
    }

    /// Reapply the requested grab state to all windows.
    pub fn sync(&self) {
        self.apply_request();
    }

    /// Returns true if GTK key events should be ignored on Linux.
    pub fn should_ignore_gtk_keys(&self) -> bool {
        false
    }

    /// No-op shutdown on Linux.
    pub fn shutdown(&self) {}
}

impl KeyboardGrab {
    fn apply_request(&self) {
        let enabled = self.inner.requested.get();
        for window in self.inner.windows.borrow().iter() {
            let Some(surface) = window.surface() else {
                trace!("window surface not available; skipping keyboard grab update");
                continue;
            };
            let Some(toplevel) = surface.dynamic_cast_ref::<gdk::Toplevel>() else {
                warn!("window surface is not a toplevel; skipping keyboard grab update");
                continue;
            };
            if enabled {
                let event: Option<&gdk::Event> = None;
                toplevel.inhibit_system_shortcuts(event);
            } else {
                toplevel.restore_system_shortcuts();
            }
        }
    }
}

impl KeyboardGrabInner {
    fn set_inhibited(&self, inhibited: bool) {
        if self.inhibited.get() == inhibited {
            return;
        }
        self.inhibited.set(inhibited);
        self.update_active();
    }

    fn update_active(&self) {
        let active = self.inhibited.get() && self.focused.get();
        if self.active.get() == active {
            return;
        }
        self.active.set(active);
        self.ui_event_tx
            .send_or_warn(UiEvent::ToggleKeyboardGrabStatus(active));
    }
}
