//! GTK4 client application glue module.
//!
//! This module wires together platform-specific setup, CLI parsing, and the
//! GTK application/event loop implementation.

/// Application actions and accelerators.
mod actions;
/// GTK application wrapper.
mod app;
/// Runtime event loop glue between GTK and the server connection.
mod event_loop;
/// Pixel buffer and Cairo surface bridge.
mod image;
/// Platform-specific input mapping helpers.
mod input;
/// Platform-specific keyboard grab handling.
#[cfg(any(target_os = "linux", target_os = "windows"))]
mod keyboard_grab;
/// Keyboard input event pipeline.
mod keyboard_input;
/// Menus and menu-overlay helpers.
mod menu;
/// Monitor discovery and lock state querying.
mod monitors;
/// UI state machine and window management.
mod ui;
/// Window and input controller wiring.
mod window;

use aperturec_graphics::prelude::*;

use anyhow::Result;
use clap::Parser;
use gtk4 as gtk;
use gtk4::glib;
use std::{env, sync::LazyLock};
use tracing::*;

/// Fallback resolution used when the platform cannot report a monitor size.
const DEFAULT_RESOLUTION: Size = Size::new(800, 600);

#[cfg(any(target_os = "macos", target_os = "windows"))]
/// CLI resolution grouping for single-display platforms.
type ResolutionGroup = aperturec_client::args::ResolutionGroupSingle;

#[cfg(target_os = "linux")]
/// CLI resolution grouping for multi-display platforms.
type ResolutionGroup = aperturec_client::args::ResolutionGroupMulti;

/// CLI arguments for the GTK client binary.
#[derive(Debug, Parser)]
pub struct ClientArgs {
    /// Client connection and display configuration arguments.
    #[command(flatten)]
    pub client: aperturec_client::args::Args<ResolutionGroup>,

    /// Metrics exporter configuration.
    #[command(flatten)]
    pub metrics: aperturec_utils::args::metrics::MetricsArgGroup,
}

impl ClientArgs {
    /// Parse CLI arguments, accepting ApertureC URIs in the server address position.
    pub fn auto() -> Result<Self> {
        let argv: Vec<String> = env::args().collect();
        if let Ok(uri) = env::var("AC_URI") {
            match aperturec_client::args::Args::<ResolutionGroup>::from_uri(&uri) {
                Ok(parsed) => {
                    if argv.len() > 1 {
                        aperturec_utils::warn_early!(
                            "CLI arguments are ignored when using AC_URI. Unset AC_URI if you would like to use CLI arguments."
                        );
                    }
                    let metrics = MetricsArgs::parse_from([argv
                        .first()
                        .cloned()
                        .unwrap_or_else(|| env!("CARGO_PKG_NAME").to_string())])
                    .metrics;
                    return Ok(ClientArgs {
                        client: parsed,
                        metrics,
                    });
                }
                Err(error) => {
                    aperturec_utils::warn_early!(
                        "Ignoring AC_URI because it failed to parse: {error}"
                    );
                }
            }
        }

        if argv.len() == 2
            && let Ok(parsed) = aperturec_client::args::Args::<ResolutionGroup>::from_uri(&argv[1])
        {
            let metrics = MetricsArgs::parse_from([argv[0].clone()]).metrics;
            return Ok(ClientArgs {
                client: parsed,
                metrics,
            });
        }

        Ok(ClientArgs::parse_from(argv))
    }
}

#[derive(Debug, Parser)]
struct MetricsArgs {
    #[command(flatten)]
    metrics: aperturec_utils::args::metrics::MetricsArgGroup,
}

/// Ensure GTK is initialized once per process.
fn ensure_gtk_init() {
    static GTK_INIT: LazyLock<()> = LazyLock::new(|| {
        debug!("initializing GTK");
        #[cfg(target_os = "macos")]
        {
            glib::set_application_name("ApertureC Client");
            glib::set_prgname(Some("ApertureC Client"));
        }
        gtk::init().expect("Failed to initialize GTK");
    });
    *GTK_INIT;
}

/// Show a minimal startup error dialog and block until the user closes it.
pub fn show_startup_error(message: &str) {
    ensure_gtk_init();

    use gtk4::prelude::*;

    let transient = gtk::Window::new();
    transient.set_title(Some("ApertureC Client"));

    let dialog = gtk::MessageDialog::builder()
        .modal(true)
        .buttons(gtk::ButtonsType::Close)
        .message_type(gtk::MessageType::Error)
        .text("ApertureC Client failed to start")
        .secondary_text(message)
        .build();
    dialog.set_transient_for(Some(&transient));

    let main_loop = glib::MainLoop::new(None, false);
    let main_loop_done = main_loop.clone();
    dialog.connect_response(move |dialog, _| {
        dialog.close();
        main_loop_done.quit();
    });
    dialog.show();
    main_loop.run();
}

/// Construct the GTK application and run its event loop.
pub fn run(args: ClientArgs) -> Result<()> {
    debug!("gtk_app::run starting");
    app::App::new(args)?.run()
}
