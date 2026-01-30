//! Keyboard input event channel and forwarder.

use aperturec_client::{Connection, InputError};

use async_channel::{Receiver, Sender, unbounded};
use gtk4::glib;
use std::sync::Arc;
use tracing::*;

/// Keyboard input event forwarded to the server connection.
#[derive(Debug, Clone, Copy)]
pub struct KeyboardEvent {
    /// XKB keycode for the event.
    pub code: u32,
    /// True when pressed, false when released.
    pub pressed: bool,
}

/// Create the keyboard event channel.
pub fn create_keyboard_channel() -> (Sender<KeyboardEvent>, Receiver<KeyboardEvent>) {
    unbounded()
}

/// Spawn a task that forwards keyboard events to the server connection.
pub fn spawn_keyboard_forwarder(
    conn: Arc<Connection>,
    keyboard_rx: Receiver<KeyboardEvent>,
) -> glib::JoinHandle<()> {
    glib::MainContext::default().spawn_local(glib::clone!(
        #[strong]
        conn,
        async move {
            debug!("keyboard input task started");
            while let Ok(event) = keyboard_rx.recv().await {
                if conn.is_shutting_down() {
                    break;
                }
                if event.pressed {
                    if let Err(error) = conn.key_press(event.code) {
                        if matches!(error, InputError::ThreadDied)
                            && (conn.is_shutting_down() || !conn.is_active())
                        {
                            debug!(%error, "keyboard input stopped during shutdown");
                            break;
                        }
                        warn!(%error, "failed to handle key press");
                    }
                } else if let Err(error) = conn.key_release(event.code) {
                    if matches!(error, InputError::ThreadDied)
                        && (conn.is_shutting_down() || !conn.is_active())
                    {
                        debug!(%error, "keyboard input stopped during shutdown");
                        break;
                    }
                    warn!(%error, "failed to handle key release");
                }
            }
            debug!("keyboard input task exiting");
        }
    ))
}
