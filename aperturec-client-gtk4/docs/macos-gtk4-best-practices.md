# macOS GTK4 best practices checklist

Source: https://docs.gtk.org/gtk4/osx.html

## App bundle / Info.plist
- [x] Ship as a macOS app bundle (WarpStations-MacOS wraps the GTK client)
- [x] `NSSupportsAutomaticGraphicsSwitching = true` in Info.plist
- [ ] If we add file associations, add UTI + `CFBundleDocumentTypes` mappings

## Menus and actions
- [x] Provide `app.quit` and `app.preferences` actions for the application menu
- [x] Add a Window menu with `gtk-macos-special = window-submenu`
- [ ] If we add a GtkHeaderBar, set `use-native-controls = true`

## Vulkan
- [ ] If we enable Vulkan, set `VK_ICD_FILENAMES` and `VK_LAYER_PATH` per guide
