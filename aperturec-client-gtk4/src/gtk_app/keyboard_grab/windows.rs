//! Windows keyboard grab hook (best-effort user-mode).

use super::super::input::map_windows_virtual_key;
use super::super::keyboard_input::KeyboardEvent;
use super::super::ui::UiEvent;
use aperturec_utils::channels::SenderExt;
use async_channel::Sender;
use gtk4::{self as gtk, glib, prelude::GtkWindowExt};
use std::sync::Mutex;
use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicBool, AtomicU32, Ordering},
};
use std::thread::{self, JoinHandle};
use tracing::{debug, error, trace, warn};
use windows::Win32::Foundation::{HINSTANCE, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, VK_CONTROL, VK_F5, VK_F12, VK_G, VK_MENU, VK_RETURN, VK_SHIFT, VK_W,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetMessageW, HC_ACTION, KBDLLHOOKSTRUCT,
    KBDLLHOOKSTRUCT_FLAGS, LLKHF_ALTDOWN, MSG, PostThreadMessageW, SetWindowsHookExW,
    TranslateMessage, UnhookWindowsHookEx, WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP, WM_QUIT,
    WM_SYSKEYDOWN, WM_SYSKEYUP,
};

/// Global hook state for the process.
static HOOK_STATE: OnceLock<Arc<HookState>> = OnceLock::new();

/// Windows keyboard grab controller.
#[derive(Clone)]
pub struct KeyboardGrab {
    state: Arc<HookState>,
    ui_event_tx: Sender<UiEvent>,
}

impl KeyboardGrab {
    /// Initialize the keyboard grab hook thread (idempotent).
    pub fn new(ui_event_tx: Sender<UiEvent>, keyboard_event_tx: Sender<KeyboardEvent>) -> Self {
        let state = HOOK_STATE.get_or_init(|| {
            let (status_tx, status_rx) = async_channel::unbounded();
            let ui_event_tx = ui_event_tx.clone();
            glib::MainContext::default().spawn_local(async move {
                while let Ok(active) = status_rx.recv().await {
                    ui_event_tx.send_or_warn(UiEvent::ToggleKeyboardGrabStatus(active));
                }
            });
            HookState::new(status_tx, keyboard_event_tx)
        });
        let state = state.clone();
        state.ensure_thread();
        Self { state, ui_event_tx }
    }

    /// Register a GTK window for focus tracking.
    pub fn register_window(&self, window: &gtk::Window) {
        let ui_event_tx = self.ui_event_tx.clone();
        window.connect_is_active_notify(move |_| {
            ui_event_tx.send_or_warn(UiEvent::WindowFocusChanged);
        });
    }

    /// Returns true if GTK key events should be ignored while the hook is active.
    pub fn should_ignore_gtk_keys(&self) -> bool {
        self.state.capture_active()
    }

    /// Enable or disable keyboard grab.
    pub fn set_enabled(&self, enabled: bool) {
        let was_enabled = self.state.enabled.swap(enabled, Ordering::SeqCst);
        if was_enabled != enabled {
            self.state.update_active();
        }
    }

    /// Update whether the GTK window is focused.
    pub fn set_focused(&self, focused: bool) {
        let was_focused = self.state.focused.swap(focused, Ordering::SeqCst);
        if was_focused != focused {
            self.state.update_active();
        }
    }

    /// Returns true when the grab is active.
    pub fn active(&self) -> bool {
        self.state.active.load(Ordering::SeqCst)
    }

    /// Recompute the active state from current flags.
    pub fn sync(&self) {
        self.state.update_active();
    }
}

impl KeyboardGrab {
    /// Stop the hook thread and unhook the Windows keyboard hook.
    pub fn shutdown(&self) {
        self.state.shutdown();
    }
}

/// Shared hook state owned by the process-wide hook thread.
struct HookState {
    status_tx: Sender<bool>,
    event_tx: Sender<KeyboardEvent>,
    enabled: AtomicBool,
    focused: AtomicBool,
    hook_ready: AtomicBool,
    active: AtomicBool,
    running: AtomicBool,
    passthrough_keys: AtomicU32,
    thread_id: AtomicU32,
    thread_handle: Mutex<Option<JoinHandle<()>>>,
}

impl HookState {
    /// Construct a new hook state container.
    fn new(status_tx: Sender<bool>, event_tx: Sender<KeyboardEvent>) -> Arc<Self> {
        Arc::new(Self {
            status_tx,
            event_tx,
            enabled: AtomicBool::new(false),
            focused: AtomicBool::new(false),
            hook_ready: AtomicBool::new(false),
            active: AtomicBool::new(false),
            running: AtomicBool::new(false),
            passthrough_keys: AtomicU32::new(0),
            thread_id: AtomicU32::new(0),
            thread_handle: Mutex::new(None),
        })
    }

    /// Start the hook thread once for this process.
    fn ensure_thread(self: &Arc<Self>) {
        if self.running.swap(true, Ordering::SeqCst) {
            return;
        }
        let state = Arc::clone(self);
        let handle = thread::Builder::new()
            .name("aperturec-keyboard-grab".to_string())
            .spawn(move || run_hook_thread(state))
            .expect("failed to spawn keyboard hook thread");
        *self.thread_handle.lock().expect("hook handle mutex") = Some(handle);
    }

    /// Returns true when the hook should capture keys for the server.
    fn capture_active(&self) -> bool {
        self.enabled.load(Ordering::SeqCst)
            && self.focused.load(Ordering::SeqCst)
            && self.hook_ready.load(Ordering::SeqCst)
    }

    /// Update the active state and notify the UI if it changes.
    fn update_active(&self) {
        let active = self.capture_active();
        let was_active = self.active.swap(active, Ordering::SeqCst);
        if was_active != active {
            let _ = self.status_tx.try_send(active);
        }
    }

    /// Stop the hook thread and wait for it to exit.
    fn shutdown(&self) {
        if !self.running.swap(false, Ordering::SeqCst) {
            return;
        }
        let thread_id = self.thread_id.load(Ordering::SeqCst);
        if thread_id != 0 {
            // SAFETY: Posting WM_QUIT to a known thread id is safe; failure is ignored.
            let _ = unsafe { PostThreadMessageW(thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) };
        }
        if let Some(handle) = self.thread_handle.lock().expect("hook handle mutex").take() {
            if let Err(err) = handle.join() {
                error!(?err, "keyboard hook thread panicked");
            }
        }
    }
}

/// Hook thread entry point that installs and services the Windows hook.
fn run_hook_thread(state: Arc<HookState>) {
    // SAFETY: The Windows API returns the current thread id for this thread.
    let thread_id = unsafe { GetCurrentThreadId() };
    state.thread_id.store(thread_id, Ordering::SeqCst);

    // SAFETY: Passing null requests the current module handle for this process.
    let instance = unsafe { GetModuleHandleW(None) }
        .ok()
        .map(|module| HINSTANCE(module.0));
    // SAFETY: Registering a low-level keyboard hook with a valid callback.
    let hook = unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook), instance, 0) };
    let hook = match hook {
        Ok(hook) => hook,
        Err(err) => {
            error!(?err, "failed to install Windows keyboard hook");
            state.hook_ready.store(false, Ordering::SeqCst);
            state.update_active();
            return;
        }
    };
    state.hook_ready.store(true, Ordering::SeqCst);
    state.update_active();
    debug!("Windows keyboard hook installed");

    let mut msg = MSG::default();
    loop {
        // SAFETY: GetMessageW expects a valid MSG pointer and is called from the hook thread.
        let result = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        if result.0 == 0 {
            break;
        }
        if result.0 == -1 {
            warn!("keyboard hook message loop error");
            break;
        }
        // SAFETY: Translate/dispatch are standard message loop calls for this thread.
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    // SAFETY: Unhooking the hook installed by this thread is safe; ignore errors.
    let _ = unsafe { UnhookWindowsHookEx(hook) };
    state.hook_ready.store(false, Ordering::SeqCst);
    state.update_active();
    debug!("Windows keyboard hook removed");
}

/// Low-level keyboard hook procedure for Windows.
unsafe extern "system" fn keyboard_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        if let Some(state) = HOOK_STATE.get() {
            if state.capture_active() {
                let msg = wparam.0 as u32;
                let pressed = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
                let released = msg == WM_KEYUP || msg == WM_SYSKEYUP;
                if pressed || released {
                    if lparam.0 == 0 {
                        // SAFETY: Defer to next hook when there is no event payload.
                        return unsafe { CallNextHookEx(None, code, wparam, lparam) };
                    }
                    // SAFETY: lparam points to a KBDLLHOOKSTRUCT for low-level keyboard hooks.
                    let info = unsafe { *(lparam.0 as *const KBDLLHOOKSTRUCT) };
                    if should_passthrough_shortcut(state, &info, pressed, released) {
                        // SAFETY: Allow GTK to receive whitelisted shortcuts.
                        return unsafe { CallNextHookEx(None, code, wparam, lparam) };
                    }
                    if let Some(code) = map_windows_virtual_key(info.vkCode) {
                        let event = KeyboardEvent { code, pressed };
                        if pressed {
                            trace!(keycode = code, "hook press");
                        } else if released {
                            trace!(keycode = code, "hook release");
                        }
                        if let Err(error) = state.event_tx.try_send(event) {
                            trace!(%error, "dropping keyboard grab event");
                        }
                    } else {
                        trace!(vk = info.vkCode, "dropping unmapped hook key");
                    }
                    return LRESULT(1);
                }
            }
        }
    }
    // SAFETY: Forward unhandled events to the next hook in the chain.
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

/// Bitmask for the passthrough Ctrl+Alt+G shortcut.
const PASSTHROUGH_G: u32 = 1 << 0;
/// Bitmask for the passthrough Ctrl+Alt+W shortcut.
const PASSTHROUGH_W: u32 = 1 << 1;
/// Bitmask for the passthrough Ctrl+Alt+F5 shortcut.
const PASSTHROUGH_F5: u32 = 1 << 2;
/// Bitmask for the passthrough Ctrl+Alt+F12 shortcut.
const PASSTHROUGH_F12: u32 = 1 << 3;
/// Bitmask for the passthrough Ctrl+Alt+Enter shortcut.
const PASSTHROUGH_RETURN: u32 = 1 << 4;

/// Return the passthrough bit for a virtual key, if it is a known shortcut key.
fn shortcut_key_bit(vk: u32) -> Option<u32> {
    match vk {
        vk if vk == VK_G.0 as u32 => Some(PASSTHROUGH_G),
        vk if vk == VK_W.0 as u32 => Some(PASSTHROUGH_W),
        vk if vk == VK_F5.0 as u32 => Some(PASSTHROUGH_F5),
        vk if vk == VK_F12.0 as u32 => Some(PASSTHROUGH_F12),
        vk if vk == VK_RETURN.0 as u32 => Some(PASSTHROUGH_RETURN),
        _ => None,
    }
}

/// Verify modifier state for Ctrl+Alt shortcut combos.
fn modifiers_match_for_vk(vk: u32, flags: KBDLLHOOKSTRUCT_FLAGS) -> bool {
    let ctrl_down = key_down(VK_CONTROL);
    let shift_down = key_down(VK_SHIFT);
    let alt_down = flags.contains(LLKHF_ALTDOWN) || key_down(VK_MENU);
    if !ctrl_down || !alt_down {
        return false;
    }
    if vk == VK_RETURN.0 as u32 {
        !shift_down
    } else {
        true
    }
}

/// Returns true when the provided virtual key is currently pressed.
fn key_down(vk: windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY) -> bool {
    // SAFETY: GetAsyncKeyState reads global keyboard state and is safe to call.
    unsafe { GetAsyncKeyState(vk.0 as i32) as u16 & 0x8000 != 0 }
}

/// Returns true when the hook should allow a shortcut through to GTK.
fn should_passthrough_shortcut(
    state: &HookState,
    info: &KBDLLHOOKSTRUCT,
    pressed: bool,
    released: bool,
) -> bool {
    let Some(bit) = shortcut_key_bit(info.vkCode) else {
        return false;
    };
    if pressed && modifiers_match_for_vk(info.vkCode, info.flags) {
        state.passthrough_keys.fetch_or(bit, Ordering::SeqCst);
        return true;
    }
    if released {
        let mask = state.passthrough_keys.load(Ordering::SeqCst);
        if mask & bit != 0 || modifiers_match_for_vk(info.vkCode, info.flags) {
            state.passthrough_keys.fetch_and(!bit, Ordering::SeqCst);
            return true;
        }
    }
    false
}
