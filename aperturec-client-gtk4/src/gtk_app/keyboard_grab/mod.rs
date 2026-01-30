//! Platform-specific keyboard grab handling.
//!
//! Windows uses a low-level keyboard hook (`windows.rs`).
//! Linux uses GTK's `inhibit_system_shortcuts` via `linux.rs`.

#[cfg(target_os = "windows")]
#[path = "windows.rs"]
mod platform;

#[cfg(target_os = "linux")]
#[path = "linux.rs"]
mod platform;

pub use platform::KeyboardGrab;
