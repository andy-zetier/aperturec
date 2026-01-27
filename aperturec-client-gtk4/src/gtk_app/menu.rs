use super::actions::Action;
#[cfg(not(target_os = "macos"))]
use gtk4::gdk;
use gtk4::{self as gtk, gio, glib, prelude::*};
use tracing::*;

#[cfg(not(target_os = "macos"))]
const MENU_CSS: &str = r#"
menubar {
  background-color: @theme_bg_color;
  color: @theme_fg_color;
  border-bottom: 1px solid @borders;
}
.arrow-button {
  background: rgba(0, 0, 0, 0.6);
  color: #fff;
  border: none;
  box-shadow: none;
  padding: 4px;
  min-width: 20px;
  min-height: 20px;
  border-radius: 4px;
}
menubar > item > popover {
  background: transparent;
  border: none;
  box-shadow: none;
  padding: 0;
}
menubar > item > popover > contents {
  box-shadow: none;
  padding: 0;
  margin: 0;
}
"#;

fn create_application_menu() -> gio::Menu {
    debug!("building application menu");
    let menu = gio::Menu::new();

    let file_menu = gio::Menu::new();
    file_menu.append(
        Some(Action::Disconnect.label()),
        Some(&Action::Disconnect.qualified_name()),
    );
    menu.append_submenu(Some("File"), &file_menu);

    let view_menu = gio::Menu::new();
    view_menu.append(
        Some(Action::Refresh.label()),
        Some(&Action::Refresh.qualified_name()),
    );
    view_menu.append(
        Some(Action::ShortcutPassthrough.label()),
        Some(&Action::ShortcutPassthrough.qualified_name()),
    );
    view_menu.append(
        Some(Action::Window.label()),
        Some(&Action::Window.qualified_name()),
    );
    view_menu.append(
        Some(Action::SingleMonitorFullscreen.label()),
        Some(&Action::SingleMonitorFullscreen.qualified_name()),
    );
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    view_menu.append(
        Some(Action::MultiMonitorFullscreen.label()),
        Some(&Action::MultiMonitorFullscreen.qualified_name()),
    );

    menu.append_submenu(Some("View"), &view_menu);

    #[cfg(target_os = "macos")]
    {
        let window_item = create_window_menu_item();
        menu.append_item(&window_item);
    }
    menu
}

#[cfg(target_os = "macos")]
fn create_window_menu_item() -> gio::MenuItem {
    let window_menu = gio::Menu::new();
    let item = gio::MenuItem::new(Some("Window"), None);
    item.set_submenu(Some(&window_menu));
    let special = glib::Variant::from("window-submenu");
    item.set_attribute_value("gtk-macos-special", Some(&special));
    item
}

/// Setup native macOS menu bar
#[cfg(target_os = "macos")]
pub fn setup_native_menu_bar(app: &gtk::Application) {
    debug!("setting up native menu bar");
    let menu = create_application_menu();
    app.set_menubar(Some(&menu));
}

#[cfg(not(target_os = "macos"))]
pub fn load_overlay_css(display: &gdk::Display) {
    debug!("loading overlay CSS");
    let css_provider = gtk::CssProvider::new();
    css_provider.load_from_data(MENU_CSS);

    gtk::style_context_add_provider_for_display(
        display,
        &css_provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}

/// Create overlay layout with menu revealer and toggle arrows (for all non-macOS modes)
#[cfg(not(target_os = "macos"))]
pub fn create_overlay_layout(drawing_area: &gtk::DrawingArea) -> gtk::Widget {
    trace!("creating overlay layout");
    // Drawing area is the main child; menu + toggle are overlays
    let overlay = gtk::Overlay::new();
    drawing_area.set_hexpand(true);
    drawing_area.set_vexpand(true);
    drawing_area.set_halign(gtk::Align::Fill);
    drawing_area.set_valign(gtk::Align::Fill);
    overlay.set_child(Some(drawing_area));

    // Menu bar inside a revealer
    let menu_revealer = gtk::Revealer::new();
    let menu = create_application_menu();
    let menu_bar = gtk::PopoverMenuBar::from_model(Some(&menu));
    menu_bar.set_hexpand(true);
    menu_bar.set_halign(gtk::Align::Fill);
    menu_revealer.set_child(Some(&menu_bar));
    menu_revealer.set_reveal_child(false); // Hidden by default
    menu_revealer.set_transition_type(gtk::RevealerTransitionType::SlideDown);
    menu_revealer.set_transition_duration(250);
    menu_revealer.set_halign(gtk::Align::Fill);
    menu_revealer.set_valign(gtk::Align::Start);
    menu_revealer.set_can_target(false); // Pass through events when not revealed

    // Show/hide buttons as separate overlay children
    let show_button = gtk::Button::with_label("▼");
    show_button.set_halign(gtk::Align::Center);
    show_button.set_valign(gtk::Align::Start);
    show_button.set_margin_top(2);
    show_button.set_margin_bottom(2);
    show_button.set_can_focus(false);
    show_button.add_css_class("arrow-button");

    let hide_button = gtk::Button::with_label("▲");
    hide_button.set_halign(gtk::Align::Center);
    hide_button.set_valign(gtk::Align::Start);
    hide_button.set_margin_top(2);
    hide_button.set_margin_bottom(2);
    hide_button.set_can_focus(false);
    hide_button.add_css_class("arrow-button");
    hide_button.set_visible(false);
    hide_button.set_can_target(false);

    // Handlers to swap menu visibility and buttons
    // Overlay menubar and toggles as separate overlays so they don't affect drawing area size
    menu_revealer.set_valign(gtk::Align::Start);
    overlay.add_overlay(&menu_revealer);

    let show_overlay = gtk::Box::new(gtk::Orientation::Vertical, 0);
    show_overlay.set_halign(gtk::Align::Center);
    show_overlay.set_valign(gtk::Align::Start);
    show_overlay.set_can_target(true);
    show_overlay.set_sensitive(true);
    show_overlay.append(&show_button);
    overlay.add_overlay(&show_overlay);

    // Hide button overlay: position below the menubar by applying a top margin equal to menu height
    let hide_overlay = gtk::Box::new(gtk::Orientation::Vertical, 0);
    hide_overlay.set_halign(gtk::Align::Center);
    hide_overlay.set_valign(gtk::Align::Start);
    hide_overlay.set_can_target(false);
    hide_overlay.set_sensitive(true);
    hide_overlay.set_visible(false);
    hide_overlay.append(&hide_button);
    overlay.add_overlay(&hide_overlay);

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

    overlay.upcast()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gio::prelude::MenuModelExt;
    use gtk4::glib;

    fn item_label(model: &gio::MenuModel, index: i32) -> String {
        let ty = glib::VariantTy::new("s").expect("variant type");
        model
            .item_attribute_value(index, "label", Some(ty))
            .and_then(|variant| variant.get::<String>())
            .expect("label")
    }

    fn item_action(model: &gio::MenuModel, index: i32) -> String {
        let ty = glib::VariantTy::new("s").expect("variant type");
        model
            .item_attribute_value(index, "action", Some(ty))
            .and_then(|variant| variant.get::<String>())
            .expect("action")
    }

    #[test]
    fn application_menu_contains_file_and_view_submenus() {
        let menu = create_application_menu();
        let model = menu.upcast::<gio::MenuModel>();

        let expected_top_items = if cfg!(target_os = "macos") { 3 } else { 2 };
        assert_eq!(model.n_items(), expected_top_items);
        assert_eq!(item_label(&model, 0), "File");
        assert_eq!(item_label(&model, 1), "View");

        let file_menu = model
            .item_link(0, "submenu")
            .expect("file submenu")
            .upcast::<gio::MenuModel>();
        assert_eq!(file_menu.n_items(), 1);
        assert_eq!(item_label(&file_menu, 0), Action::Disconnect.label());
        assert_eq!(
            item_action(&file_menu, 0),
            Action::Disconnect.qualified_name()
        );

        let view_menu = model
            .item_link(1, "submenu")
            .expect("view submenu")
            .upcast::<gio::MenuModel>();
        let expected_view_items = if cfg!(any(target_os = "macos", target_os = "windows")) {
            4
        } else {
            5
        };
        assert_eq!(view_menu.n_items(), expected_view_items);
    }
}
