use super::ui::{UiEvent, WindowMode};
use aperturec_utils::channels::SenderExt;
use async_channel::Sender;
use gtk4::{self as gtk, gio, prelude::*};
use strum::{AsRefStr, EnumIter, IntoEnumIterator};
use tracing::{debug, trace};

#[derive(Debug, Clone, Copy, AsRefStr, EnumIter)]
pub enum Action {
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

    pub const fn label(&self) -> &'static str {
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

    pub fn qualified_name(&self) -> String {
        format!("app.{}", self.name())
    }
}

pub struct ActionManager {
    app: gtk::Application,
    ui_event_tx: Sender<UiEvent>,
}

impl ActionManager {
    pub fn new(app: gtk::Application, ui_event_tx: Sender<UiEvent>) -> Self {
        Self { app, ui_event_tx }
    }

    /// Register all application actions with the GAction system
    pub fn register_all_actions(&self) {
        debug!("registering actions");
        for action in Action::iter() {
            trace!(action = action.name(), "register action");
            let simple_action = gio::SimpleAction::new(action.name(), None);
            let ui_event_tx = self.ui_event_tx.clone();

            simple_action.connect_activate(move |_, _| {
                trace!(action = action.name(), "action activated");
                let event = match action {
                    Action::Disconnect => UiEvent::Shutdown,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_labels_are_non_empty() {
        for action in Action::iter() {
            assert!(!action.label().is_empty());
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
