use gtk4::gdk;
#[cfg(target_os = "linux")]
use tracing::trace;
#[cfg(any(target_os = "macos", target_os = "windows"))]
use tracing::warn;

#[cfg(target_os = "macos")]
use gtk4::glib::translate::IntoGlib;
#[cfg(any(target_os = "macos", target_os = "windows"))]
use keycode::{KeyMap, KeyMapping};

#[cfg(target_os = "macos")]
pub fn map_gdk_key(keyval: gdk::Key, raw_keycode: u32) -> Option<u32> {
    let mapping = KeyMapping::Mac(raw_keycode as u16);
    match KeyMap::try_from(mapping) {
        Ok(map) => Some(map.xkb as u32),
        Err(_) => {
            warn!(keyval = %keyval.into_glib(), raw_keycode = raw_keycode, "macOS key mapping failed; dropping key");
            None
        }
    }
}

#[cfg(target_os = "linux")]
pub fn map_gdk_key(_keyval: gdk::Key, keycode: u32) -> Option<u32> {
    trace!(keycode, "map key (linux passthrough)");
    Some(keycode)
}

// Windows helper copied from GTK3 client: convert virtual-key to OEM scan code for keycode crate.
#[cfg(target_os = "windows")]
fn convert_win_virtual_key_to_scan_code(virtual_key: u32) -> anyhow::Result<u16> {
    let raw = unsafe {
        windows::Win32::UI::Input::KeyboardAndMouse::MapVirtualKeyW(
            virtual_key,
            windows::Win32::UI::Input::KeyboardAndMouse::MAPVK_VK_TO_VSC_EX,
        )
    };
    if raw == 0 {
        anyhow::bail!(
            "Failed to convert virtual key code {:#X} to scan code",
            virtual_key
        );
    }
    let mut sc = (raw & 0x00FF) as u16;
    let has_ext_bit = (raw & 0x0100) != 0;
    let is_ext_vk = matches!(
        windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY(virtual_key as u16),
        windows::Win32::UI::Input::KeyboardAndMouse::VK_LEFT
            | windows::Win32::UI::Input::KeyboardAndMouse::VK_RIGHT
            | windows::Win32::UI::Input::KeyboardAndMouse::VK_UP
            | windows::Win32::UI::Input::KeyboardAndMouse::VK_DOWN
            | windows::Win32::UI::Input::KeyboardAndMouse::VK_HOME
            | windows::Win32::UI::Input::KeyboardAndMouse::VK_END
            | windows::Win32::UI::Input::KeyboardAndMouse::VK_INSERT
            | windows::Win32::UI::Input::KeyboardAndMouse::VK_DELETE
            | windows::Win32::UI::Input::KeyboardAndMouse::VK_PRIOR
            | windows::Win32::UI::Input::KeyboardAndMouse::VK_NEXT
            | windows::Win32::UI::Input::KeyboardAndMouse::VK_RCONTROL
            | windows::Win32::UI::Input::KeyboardAndMouse::VK_RMENU
            | windows::Win32::UI::Input::KeyboardAndMouse::VK_LWIN
            | windows::Win32::UI::Input::KeyboardAndMouse::VK_RWIN
            | windows::Win32::UI::Input::KeyboardAndMouse::VK_APPS
    );
    if has_ext_bit || is_ext_vk {
        sc |= 0xE000;
    }
    Ok(sc)
}

#[cfg(target_os = "windows")]
pub fn map_gdk_key(_keyval: gdk::Key, raw_keycode: u32) -> Option<u32> {
    let scancode = match convert_win_virtual_key_to_scan_code(raw_keycode) {
        Ok(sc) => sc,
        Err(err) => {
            warn!(
                "Failed to convert key code {:#X}: {}. Ignoring key event.",
                raw_keycode, err
            );
            return None;
        }
    };
    let mapping = KeyMapping::Win(scancode);
    match KeyMap::try_from(mapping) {
        Ok(map) => Some(map.xkb as u32),
        Err(_) => {
            warn!(
                "Failed to convert key code {:#X} to X11 keycode. Ignoring key event.",
                raw_keycode
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_key_mapping_passthrough() {
        let keyval = gdk::Key::_0;
        assert_eq!(map_gdk_key(keyval, 42), Some(42));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_key_mapping_ignores_keyval() {
        let keyval = gdk::Key::A;
        assert_eq!(map_gdk_key(keyval, 0), Some(0));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_virtual_key_zero_is_invalid() {
        assert!(convert_win_virtual_key_to_scan_code(0).is_err());
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_map_key_invalid_returns_none() {
        let keyval = gdk::Key::_0;
        assert_eq!(map_gdk_key(keyval, 0), None);
    }
}
