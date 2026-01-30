//! Application actions and keyboard accelerators.

use super::ui::{UiEvent, WindowMode};

use aperturec_utils::channels::SenderExt;

use async_channel::Sender;
#[cfg(any(target_os = "linux", target_os = "windows"))]
use gtk4::glib;
use gtk4::{
    self as gtk,
    gio::{self},
    prelude::*,
};
use strum::{AsRefStr, EnumIter, IntoEnumIterator};
use tracing::*;

/// Action identifiers exposed to GTK's `GAction` system.
#[derive(Debug, Clone, Copy, AsRefStr, EnumIter)]
pub enum Action {
    /// Request a display refresh from the server.
    Refresh,
    /// Disconnect from the server and exit.
    Disconnect,
    /// Toggle system keyboard shortcut inhibition (Linux/Windows only).
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    ToggleKeyboardGrab,
    /// Switch to windowed mode.
    Window,
    /// Switch to single-monitor fullscreen mode.
    SingleMonitorFullscreen,
    /// Switch to multi-monitor fullscreen mode (Linux-only).
    #[cfg(target_os = "linux")]
    MultiMonitorFullscreen,
}

impl Action {
    /// Action name used by GTK (`app.<name>` is the full qualified name).
    fn name(&self) -> &str {
        self.as_ref()
    }

    /// Keyboard accelerators for the action.
    const fn shortcuts(&self) -> &'static [&'static str] {
        #[cfg(target_os = "macos")]
        match self {
            Action::Refresh => &["<Ctrl><Meta>F5"],
            Action::Disconnect => &["<Ctrl><Meta>F12"],
            Action::Window => &["<Ctrl><Meta>W"],
            Action::SingleMonitorFullscreen => &["<Ctrl><Meta>F"],
        }
        #[cfg(any(target_os = "linux", target_os = "windows"))]
        match self {
            Action::Refresh => &["<Ctrl><Alt>F5", "<Ctrl><Alt><Shift>F5"],
            Action::Disconnect => &["<Ctrl><Alt>F12", "<Ctrl><Alt><Shift>F12"],
            #[cfg(any(target_os = "linux", target_os = "windows"))]
            Action::ToggleKeyboardGrab => &["<Ctrl><Alt>G", "<Ctrl><Alt><Shift>G"],
            Action::Window => &["<Ctrl><Alt>W", "<Ctrl><Alt><Shift>W"],
            Action::SingleMonitorFullscreen => &["<Ctrl><Alt>Return"],
            #[cfg(target_os = "linux")]
            Action::MultiMonitorFullscreen => &["<Ctrl><Alt><Shift>Return"],
        }
    }

    /// GTK action name including the `app.` namespace prefix.
    pub fn qualified_name(&self) -> String {
        format!("app.{}", self.name())
    }
}

/// Registers actions and their accelerators on a GTK application.
pub struct ActionManager {
    /// GTK application to register actions against.
    app: gtk::Application,
    /// UI event sender for action callbacks.
    ui_event_tx: Sender<UiEvent>,
}

impl ActionManager {
    /// Create a new action manager bound to the application and UI event channel.
    pub fn new(app: gtk::Application, ui_event_tx: Sender<UiEvent>) -> Self {
        Self { app, ui_event_tx }
    }

    /// Register all application actions with the GAction system.
    pub fn register_all_actions(&self) {
        debug!("registering actions");
        for action in Action::iter() {
            trace!(action = action.name(), "register action");
            let ui_event_tx = self.ui_event_tx.clone();
            #[cfg(any(target_os = "linux", target_os = "windows"))]
            if matches!(action, Action::ToggleKeyboardGrab) {
                let initial_state = glib::Variant::from(false);
                let simple_action =
                    gio::SimpleAction::new_stateful(action.name(), None, &initial_state);
                simple_action.connect_activate(move |simple_action, parameter| {
                    trace!(action = action.name(), "action activated");
                    let current = simple_action
                        .state()
                        .and_then(|state| state.get::<bool>())
                        .unwrap_or(false);
                    let next = parameter
                        .and_then(|value| value.get::<bool>())
                        .unwrap_or(!current);
                    simple_action.set_state(&glib::Variant::from(next));
                    ui_event_tx.send_or_warn(UiEvent::SetToggleKeyboardGrab(next));
                });
                self.app.add_action(&simple_action);
                continue;
            }

            let simple_action = gio::SimpleAction::new(action.name(), None);
            simple_action.connect_activate(move |_, _| {
                trace!(action = action.name(), "action activated");
                ui_event_tx.send_or_warn(action.to_event());
            });
            self.app.add_action(&simple_action);
        }
    }

    /// Register global accelerators for all actions.
    pub fn register_accelerators(&self) {
        debug!("registering accelerators");
        for action in Action::iter() {
            trace!(action = action.name(), "register accelerator");
            self.app
                .set_accels_for_action(&action.qualified_name(), action.shortcuts());
        }
    }

    /// Register macOS system actions that populate the application menu.
    #[cfg(target_os = "macos")]
    pub fn register_macos_system_actions(&self) {
        debug!("registering macOS system actions");
        self.register_simple_action("quit", || UiEvent::Shutdown);
        self.register_simple_action("preferences", || UiEvent::ShowPreferences);
    }

    /// Register a simple action that maps directly to a `UiEvent`.
    #[cfg(target_os = "macos")]
    fn register_simple_action<F>(&self, name: &str, event: F)
    where
        F: Fn() -> UiEvent + 'static,
    {
        let simple_action = gio::SimpleAction::new(name, None);
        let ui_event_tx = self.ui_event_tx.clone();
        simple_action.connect_activate(move |_, _| {
            ui_event_tx.send_or_warn(event());
        });
        self.app.add_action(&simple_action);
    }
}

impl Action {
    fn to_event(self) -> UiEvent {
        match self {
            Action::Disconnect => UiEvent::Shutdown,
            Action::Refresh => UiEvent::Refresh,
            Action::Window => UiEvent::SetWindowMode(WindowMode::Single { fullscreen: false }),
            Action::SingleMonitorFullscreen => {
                UiEvent::SetWindowMode(WindowMode::Single { fullscreen: true })
            }
            #[cfg(target_os = "linux")]
            Action::MultiMonitorFullscreen => UiEvent::SetWindowMode(WindowMode::Multi),
            #[cfg(any(target_os = "linux", target_os = "windows"))]
            Action::ToggleKeyboardGrab => {
                unreachable!("toggle keyboard grab handled by stateful action")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Action;

    use strum::IntoEnumIterator;

    #[test]
    fn action_names_are_non_empty() {
        for action in Action::iter() {
            assert!(!action.name().is_empty());
        }
    }

    #[test]
    fn action_qualified_names_are_prefixed() {
        for action in Action::iter() {
            let name = action.qualified_name();
            assert!(name.starts_with("app."));
            assert!(name.contains(action.name()));
        }
    }
}
