//! GTK menu definitions and overlay menu helpers.

#[cfg(any(target_os = "linux", target_os = "windows"))]
use gtk4::gdk;
use gtk4::{self as gtk, gio, glib, prelude::*};
use tracing::*;

/// UI resource path for macOS menu layout.
#[cfg(target_os = "macos")]
const MENU_RESOURCE: &str = "/com/aperturec/client/ui/menu-nomm.ui";
/// UI resource path for Windows menu layout.
#[cfg(target_os = "windows")]
const MENU_RESOURCE: &str = "/com/aperturec/client/ui/menu-windows.ui";
/// UI resource path for Linux menu layout.
#[cfg(target_os = "linux")]
const MENU_RESOURCE: &str = "/com/aperturec/client/ui/menu.ui";

/// Load the application menu model from GTK resources.
fn load_application_menu() -> gio::Menu {
    debug!("loading application menu from resources");
    let builder = gtk::Builder::from_resource(MENU_RESOURCE);
    builder
        .object::<gio::Menu>("app-menu")
        .expect("missing app-menu in menu resource")
}

/// Create a `Window` submenu placeholder for the macOS menu bar.
#[cfg(target_os = "macos")]
fn create_window_menu_item() -> gio::MenuItem {
    let window_menu = gio::Menu::new();
    let item = gio::MenuItem::new(Some("Window"), None);
    item.set_submenu(Some(&window_menu));
    let special = glib::Variant::from("window-submenu");
    item.set_attribute_value("gtk-macos-special", Some(&special));
    item
}

/// Setup native macOS menu bar.
#[cfg(target_os = "macos")]
pub fn setup_native_menu_bar(app: &gtk::Application) {
    debug!("setting up native menu bar");
    let menu = load_application_menu();
    let window_item = create_window_menu_item();
    menu.append_item(&window_item);
    app.set_menubar(Some(&menu));
}

/// Load CSS resources used by the non-macOS menu overlay.
#[cfg(any(target_os = "linux", target_os = "windows"))]
pub fn load_overlay_css(display: &gdk::Display) {
    debug!("loading overlay CSS");
    let css_provider = gtk::CssProvider::new();
    css_provider.load_from_resource("/com/aperturec/client/css/menu-overlay.css");

    gtk::style_context_add_provider_for_display(
        display,
        &css_provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}

/// Wire overlay menu controls (for all non-macOS modes).
#[cfg(any(target_os = "linux", target_os = "windows"))]
pub fn bind_overlay_menu(
    menu_bar: &gtk::PopoverMenuBar,
    menu_revealer: &gtk::Revealer,
    show_button: &gtk::Button,
    hide_button: &gtk::Button,
    show_overlay: &gtk::Box,
    hide_overlay: &gtk::Box,
) {
    trace!("binding overlay menu");
    let menu = load_application_menu();
    menu_bar.set_menu_model(Some(&menu));

    menu_revealer.set_reveal_child(false);
    menu_revealer.set_transition_type(gtk::RevealerTransitionType::SlideDown);
    menu_revealer.set_transition_duration(250);
    menu_revealer.set_can_target(false);

    show_button.add_css_class("arrow-button");
    hide_button.add_css_class("arrow-button");

    show_button.set_visible(true);
    show_button.set_can_target(true);
    show_overlay.set_visible(true);
    show_overlay.set_can_target(true);

    hide_button.set_visible(false);
    hide_button.set_can_target(false);
    hide_overlay.set_visible(false);
    hide_overlay.set_can_target(false);

    show_button.connect_clicked(glib::clone!(
        #[weak]
        menu_revealer,
        #[weak]
        menu_bar,
        #[weak]
        show_button,
        #[weak]
        hide_button,
        #[weak]
        show_overlay,
        #[weak]
        hide_overlay,
        move |_| {
            debug!("show menu overlay");
            menu_revealer.set_reveal_child(true);
            menu_revealer.set_can_target(true);

            let (_, nat, _, _) = menu_bar.measure(gtk::Orientation::Vertical, -1);
            hide_overlay.set_margin_top(nat + 2);

            show_button.set_visible(false);
            show_button.set_can_target(false);
            show_overlay.set_can_target(false);
            show_overlay.set_visible(false);

            hide_button.set_visible(true);
            hide_button.set_can_target(true);
            hide_overlay.set_visible(true);
            hide_overlay.set_can_target(true);
        }
    ));

    hide_button.connect_clicked(glib::clone!(
        #[weak]
        menu_revealer,
        #[weak]
        show_button,
        #[weak]
        hide_button,
        #[weak]
        show_overlay,
        #[weak]
        hide_overlay,
        move |_| {
            debug!("hide menu overlay");
            menu_revealer.set_reveal_child(false);
            menu_revealer.set_can_target(false);

            hide_overlay.set_margin_top(0);

            hide_button.set_visible(false);
            hide_button.set_can_target(false);
            hide_overlay.set_visible(false);
            hide_overlay.set_can_target(false);

            show_button.set_visible(true);
            show_button.set_can_target(true);
            show_overlay.set_visible(true);
            show_overlay.set_can_target(true);
        }
    ));
}

#[cfg(test)]
mod tests {
    use super::MENU_RESOURCE;
    #[cfg(target_os = "macos")]
    use super::create_window_menu_item;

    use crate::gtk_app::actions::Action;

    use gtk4::{
        self as gtk,
        gio::{self, prelude::MenuModelExt},
        prelude::Cast,
    };

    fn item_action(model: &gio::MenuModel, index: i32) -> String {
        let variant = model
            .item_attribute_value(index, "action", None)
            .expect("action");
        if let Some(value) = variant.get::<String>() {
            return value;
        }
        if let Some(value) = variant.get::<Option<String>>() {
            return value.unwrap_or_default();
        }
        variant.str().unwrap_or_default().to_string()
    }

    fn load_menu_for_test() -> gio::Menu {
        gtk::init().expect("gtk init");
        let _ = gio::resources_register_include!("aperturec-client.gresource");
        let builder = gtk::Builder::from_resource(MENU_RESOURCE);
        let menu = builder
            .object::<gio::Menu>("app-menu")
            .expect("missing app-menu");
        #[cfg(target_os = "macos")]
        {
            let window_item = create_window_menu_item();
            menu.append_item(&window_item);
        }
        menu
    }

    #[test]
    fn application_menu_contains_file_and_view_submenus() {
        let menu = load_menu_for_test();
        let model = menu.upcast::<gio::MenuModel>();

        let expected_top_items = if cfg!(target_os = "macos") { 3 } else { 2 };
        assert_eq!(model.n_items(), expected_top_items);

        let file_menu = model
            .item_link(0, "submenu")
            .expect("file submenu")
            .upcast::<gio::MenuModel>();
        let file_section = file_menu
            .item_link(0, "section")
            .expect("file section")
            .upcast::<gio::MenuModel>();
        assert_eq!(file_section.n_items(), 1);
        assert_eq!(
            item_action(&file_section, 0),
            Action::Disconnect.qualified_name()
        );

        let view_menu = model
            .item_link(1, "submenu")
            .expect("view submenu")
            .upcast::<gio::MenuModel>();
        let view_section = view_menu
            .item_link(0, "section")
            .expect("view section")
            .upcast::<gio::MenuModel>();
        let expected_view_items = if cfg!(target_os = "macos") {
            3
        } else if cfg!(target_os = "windows") {
            4
        } else {
            5
        };
        assert_eq!(view_section.n_items(), expected_view_items);
    }
}
