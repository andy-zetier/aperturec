/// Keyboard lock state for Caps Lock, Num Lock, and Scroll Lock.
///
/// This structure captures the state of keyboard lock keys, which may need
/// to be synchronized between the client and server for proper keyboard
/// input handling.
#[derive(PartialEq, Clone, Copy, Debug)]
pub struct LockState {
    /// Whether Caps Lock is currently enabled.
    pub is_caps_locked: bool,
    /// Whether Num Lock is currently enabled.
    pub is_num_locked: bool,
    /// Whether Scroll Lock is currently enabled.
    pub is_scroll_locked: bool,
}
