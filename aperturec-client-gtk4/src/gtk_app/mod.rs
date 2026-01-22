mod actions;
mod app;
mod event_loop;
mod image;
mod input;
mod menu;
mod monitors;
mod ui;
mod window;

use anyhow::Result;
use aperturec_graphics::prelude::*;
use clap::Parser;
#[cfg(target_os = "macos")]
use gtk4::glib;
use gtk4::{self as gtk, gdk};
use std::sync::LazyLock;
use tracing::debug;

const DEFAULT_RESOLUTION: Size = Size::new(800, 600);

#[cfg(any(target_os = "macos", target_os = "windows"))]
type ResolutionGroup = aperturec_client::args::ResolutionGroupSingle;

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
type ResolutionGroup = aperturec_client::args::ResolutionGroupMulti;

#[derive(Debug, Parser)]
pub struct ClientArgs {
    #[command(flatten)]
    pub client: aperturec_client::args::Args<ResolutionGroup>,

    #[command(flatten)]
    pub metrics: aperturec_utils::args::metrics::MetricsArgGroup,
}

fn ensure_gtk_init() {
    static GTK_INIT: LazyLock<()> = LazyLock::new(|| {
        debug!("initializing GTK");
        #[cfg(target_os = "macos")]
        {
            glib::set_application_name("ApertureC Client");
            glib::set_prgname(Some("ApertureC Client"));
        }
        gdk::set_allowed_backends("x11,*");
        gtk::init().expect("Failed to initialize GTK");
    });
    *GTK_INIT;
}

pub fn run(args: ClientArgs) -> Result<()> {
    debug!("gtk_app::run starting");
    app::App::new(args)?.run()
}
