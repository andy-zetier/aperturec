use super::ClientArgs;
use super::actions::ActionManager;
use super::ensure_gtk_init;
use super::menu;
use super::monitors::{Monitors, current_lock_state};
use super::ui::{Ui, UiEvent};
use anyhow::{Result, bail};
use aperturec_client::{Client, Connection, DisplayMode, config};
use aperturec_graphics::display::Display;
use aperturec_graphics::prelude::*;
use async_channel::{Receiver, Sender, unbounded};
use libadwaita as adw;
use gtk4::glib::prelude::Cast;
#[cfg(not(target_os = "macos"))]
use gtk4::gdk;
#[cfg(target_os = "macos")]
use gtk4::glib;
#[cfg(target_os = "macos")]
use gtk4::prelude::*;
use gtk4::{self as gtk, gio};
use std::sync::Arc;
use tracing::*;

pub struct App {
    app: gtk::Application,
    ui: Ui,
    ui_event_tx: Sender<UiEvent>,
    ui_event_rx: Receiver<UiEvent>,
    conn: Arc<Connection>,
}

fn build_initial_displays_requested(
    display_mode: DisplayMode,
    monitor_displays: &[Display],
) -> Vec<Display> {
    match display_mode {
        DisplayMode::Windowed { size } => vec![Display {
            area: Rect::new(Point::origin(), size),
            is_enabled: true,
        }],
        DisplayMode::SingleFullscreen => vec![
            monitor_displays
                .first()
                .cloned()
                .expect("no monitors available"),
        ],
        DisplayMode::MultiFullscreen => monitor_displays.to_vec(),
    }
}

impl App {
    pub fn new(args: ClientArgs) -> Result<Self> {
        debug!("app new");
        Self::initialize(args)
    }

    pub fn initialize(args: ClientArgs) -> Result<Self> {
        debug!("app initialize");
        ensure_gtk_init();

        let config = config::Configuration::from_args(args.client)?;
        debug!(?config.initial_display_mode, "loaded configuration");
        let monitors = Monitors::current()?;
        debug!(count = monitors.as_displays().count(), "monitors detected");
        let monitor_displays: Vec<Display> = monitors.as_displays().collect();
        let initial_displays_requested =
            build_initial_displays_requested(config.initial_display_mode, &monitor_displays);
        debug!(
            count = initial_displays_requested.len(),
            "initial displays requested"
        );
        let initial_display_mode = config.initial_display_mode;
        let mut client = Client::new(config, current_lock_state()?, &initial_displays_requested);
        debug!("connecting to server");
        let conn = Arc::new(client.connect()?);

        let app: gtk::Application = adw::Application::builder()
            .application_id("com.zetier.aperturec.client")
            .flags(gio::ApplicationFlags::NON_UNIQUE)
            .build()
            .upcast();

        let (ui_event_tx, ui_event_rx) = unbounded();
        trace!("ui event channel created");

        // Setup actions and menu bar
        #[cfg(target_os = "macos")]
        {
            debug!("registering macOS actions/menu");
            let action_manager = ActionManager::new(app.clone(), ui_event_tx.clone());
            action_manager.register_all_actions();
            action_manager.register_accelerators();
            action_manager.register_macos_system_actions();

            app.connect_startup(move |app| {
                glib::set_application_name("ApertureC Client");
                glib::set_prgname(Some("ApertureC Client"));
                menu::setup_native_menu_bar(app);
            });
        }
        #[cfg(not(target_os = "macos"))]
        {
            debug!("registering actions/menu");
            let action_manager = ActionManager::new(app.clone(), ui_event_tx.clone());
            action_manager.register_all_actions();
            action_manager.register_accelerators();

            let display = gdk::Display::default().expect("Could not get default display");
            menu::load_overlay_css(&display);
        }

        debug!("initializing UI");
        let ui = Ui::new(
            conn.clone(),
            initial_display_mode,
            ui_event_tx.clone(),
            app.clone(),
        )?;

        Ok(App {
            app,
            ui,
            ui_event_tx,
            ui_event_rx,
            conn,
        })
    }

    pub fn run(self) -> Result<()> {
        let App {
            app,
            ui,
            ui_event_tx,
            ui_event_rx,
            conn,
        } = self;
        debug!("running event loop");
        super::event_loop::run(app, ui, ui_event_tx, ui_event_rx, conn.clone());
        debug!("event loop exited; shutting down");
        Self::shutdown(conn)
    }

    fn shutdown(conn: Arc<Connection>) -> Result<()> {
        debug!(
            count = Arc::strong_count(&conn),
            "connection strong count at shutdown"
        );
        let Some(connection) = Arc::into_inner(conn) else {
            bail!("outlying connection references");
        };
        connection.disconnect();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::build_initial_displays_requested;
    use aperturec_client::DisplayMode;
    use aperturec_graphics::display::Display;
    use aperturec_graphics::prelude::*;

    fn sample_displays() -> Vec<Display> {
        vec![
            Display {
                area: Rect::new(Point::origin(), Size::new(1920, 1080)),
                is_enabled: true,
            },
            Display {
                area: Rect::new(Point::new(1920, 0), Size::new(1920, 1080)),
                is_enabled: true,
            },
        ]
    }

    #[test]
    fn build_initial_displays_windowed_ignores_monitors() {
        let size = Size::new(800, 600);
        let displays =
            build_initial_displays_requested(DisplayMode::Windowed { size }, &sample_displays());
        assert_eq!(displays.len(), 1);
        assert_eq!(
            displays[0],
            Display {
                area: Rect::new(Point::origin(), size),
                is_enabled: true,
            }
        );
    }

    #[test]
    fn build_initial_displays_single_fullscreen_uses_first_monitor() {
        let monitors = sample_displays();
        let displays = build_initial_displays_requested(DisplayMode::SingleFullscreen, &monitors);
        assert_eq!(displays, vec![monitors[0].clone()]);
    }

    #[test]
    fn build_initial_displays_multi_fullscreen_uses_all_monitors() {
        let monitors = sample_displays();
        let displays = build_initial_displays_requested(DisplayMode::MultiFullscreen, &monitors);
        assert_eq!(displays, monitors);
    }
}
