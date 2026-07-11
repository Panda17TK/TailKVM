use crate::input::HEALTH_MARKER_EXTRA_INFO;
use std::{
    ptr::null_mut,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Sender},
        Mutex, OnceLock,
    },
    thread,
    time::Duration,
};
use windows_sys::Win32::{
    Foundation::{LPARAM, WPARAM},
    UI::{
        Input::KeyboardAndMouse::GetAsyncKeyState,
        WindowsAndMessaging::{
            CallNextHookEx, DispatchMessageW, PeekMessageW, SetWindowsHookExW, TranslateMessage,
            UnhookWindowsHookEx, MSG, PM_REMOVE, WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP,
            WM_SYSKEYDOWN, WM_SYSKEYUP,
        },
    },
};

const LLKHF_EXTENDED: u32 = 0x01;

const VK_CONTROL: i32 = 0x11;
const VK_MENU: i32 = 0x12;
const VK_PAUSE: u32 = 0x13;
// On PC keyboards the Pause/Break key emits VK_CANCEL (0x03) — not VK_PAUSE —
// while Ctrl is held (the hardware "Ctrl+Break" scancode E0 46). The failsafe
// requires Ctrl, so the real keypress arrives as VK_CANCEL; accept both so the
// panic hotkey fires regardless of which VK the combo produces.
const VK_CANCEL: u32 = 0x03;

#[derive(Debug, Clone)]
pub enum KeyboardHookEvent {
    Key {
        vk: u16,
        scan_code: u16,
        down: bool,
        extended: bool,
    },
    Failsafe,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct KbdllHookStruct {
    vk_code: u32,
    scan_code: u32,
    flags: u32,
    time: u32,
    dw_extra_info: usize,
}

static EVENT_SENDER: OnceLock<Mutex<Option<Sender<KeyboardHookEvent>>>> = OnceLock::new();

/// Keys held *before* the hook was installed. Their key-down already reached
/// local apps (pre-hook, unsuppressed); suppressing the matching key-up would
/// leave the local app with a stuck key. The proc lets the FIRST key-up for each
/// of these pass through to the local app (and consumes it from the set), then
/// resumes normal suppression. Seeded by the forwarding layer via
/// [`set_preheld_keys`] at capture start.
static PREHELD_KEYS: OnceLock<Mutex<Vec<u16>>> = OnceLock::new();

/// Record the keys that were already held when keyboard capture started so the
/// hook can release them locally on their first key-up (prevents a stuck key on
/// the controller for keys pressed before the hook installed).
pub fn set_preheld_keys(keys: Vec<u16>) {
    let slot = PREHELD_KEYS.get_or_init(|| Mutex::new(Vec::new()));
    if let Ok(mut guard) = slot.lock() {
        *guard = keys;
    }
}

/// Decide whether a key event should bypass suppression because it is the first
/// key-up of a pre-held key, consuming it from `preheld` when so. Pure so the
/// consume-once semantics are unit-testable without the Win32 hook.
fn take_preheld_passthrough(preheld: &mut Vec<u16>, vk: u16, up: bool) -> bool {
    if !up {
        return false;
    }
    if let Some(pos) = preheld.iter().position(|&k| k == vk) {
        preheld.swap_remove(pos);
        true
    } else {
        false
    }
}

/// Count of self-injected health markers this hook has observed. The
/// forwarding loop compares it across marker injections: a marker that never
/// arrives means Windows silently removed the hook (LowLevelHooksTimeout).
static HEALTH_MARKER_SEEN: AtomicU64 = AtomicU64::new(0);

pub fn health_marker_seen() -> u64 {
    HEALTH_MARKER_SEEN.load(Ordering::Relaxed)
}

/// IME composition pass-through (issue #10). While enabled the hook still
/// *observes* every key (so the forwarding loop can see the IME toggle key and
/// the failsafe/health paths keep working) but no longer *suppresses* local
/// input — keystrokes reach the focused IME capture window where the real
/// local IME composes. The forwarding loop ignores observed keys in this mode
/// and forwards only committed composition text.
static PASSTHROUGH: AtomicBool = AtomicBool::new(false);

pub fn set_passthrough(enabled: bool) {
    PASSTHROUGH.store(enabled, Ordering::SeqCst);
}

pub struct KeyboardHookHandle {
    stop_tx: Option<mpsc::Sender<()>>,
    join_handle: Option<thread::JoinHandle<()>>,
}

impl Drop for KeyboardHookHandle {
    fn drop(&mut self) {
        if let Some(stop_tx) = self.stop_tx.take() {
            let _ = stop_tx.send(());
        }

        if let Some(join_handle) = self.join_handle.take() {
            let _ = join_handle.join();
        }
    }
}

pub fn start_keyboard_hook(
    event_tx: Sender<KeyboardHookEvent>,
) -> Result<KeyboardHookHandle, String> {
    let sender_slot = EVENT_SENDER.get_or_init(|| Mutex::new(None));

    {
        let mut guard = sender_slot
            .lock()
            .map_err(|_| "keyboard hook sender mutex poisoned".to_string())?;

        if guard.is_some() {
            return Err("keyboard hook is already running".to_string());
        }

        *guard = Some(event_tx);
    }

    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();

    let join_handle = thread::spawn(move || {
        let hook = unsafe {
            SetWindowsHookExW(WH_KEYBOARD_LL, Some(low_level_keyboard_proc), null_mut(), 0)
        };

        if hook.is_null() {
            let _ = ready_tx.send(Err("SetWindowsHookExW(WH_KEYBOARD_LL) failed".to_string()));
            clear_sender();
            return;
        }

        let _ = ready_tx.send(Ok(()));

        let mut msg = MSG {
            hwnd: null_mut(),
            message: 0,
            wParam: 0,
            lParam: 0,
            time: 0,
            pt: windows_sys::Win32::Foundation::POINT { x: 0, y: 0 },
        };

        loop {
            while unsafe { PeekMessageW(&mut msg, null_mut(), 0, 0, PM_REMOVE) } != 0 {
                unsafe {
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }

            if stop_rx.try_recv().is_ok() {
                break;
            }

            thread::sleep(Duration::from_millis(5));
        }

        unsafe {
            UnhookWindowsHookEx(hook);
        }

        clear_sender();
    });

    match ready_rx.recv_timeout(Duration::from_secs(2)) {
        Ok(Ok(())) => Ok(KeyboardHookHandle {
            stop_tx: Some(stop_tx),
            join_handle: Some(join_handle),
        }),
        Ok(Err(err)) => {
            let _ = stop_tx.send(());
            let _ = join_handle.join();
            Err(err)
        }
        Err(err) => {
            let _ = stop_tx.send(());
            let _ = join_handle.join();
            clear_sender();
            Err(format!("keyboard hook did not become ready: {err}"))
        }
    }
}

unsafe extern "system" fn low_level_keyboard_proc(
    n_code: i32,
    w_param: WPARAM,
    l_param: LPARAM,
) -> isize {
    if n_code < 0 {
        return CallNextHookEx(null_mut(), n_code, w_param, l_param);
    }

    let info = &*(l_param as *const KbdllHookStruct);

    // Our own health marker: count it and swallow it so neither applications
    // nor the forwarding channel ever see it.
    if info.dw_extra_info == HEALTH_MARKER_EXTRA_INFO {
        HEALTH_MARKER_SEEN.fetch_add(1, Ordering::Relaxed);
        return 1;
    }

    let message = w_param as u32;
    let down = matches!(message, WM_KEYDOWN | WM_SYSKEYDOWN);
    let up = matches!(message, WM_KEYUP | WM_SYSKEYUP);

    if !down && !up {
        return CallNextHookEx(null_mut(), n_code, w_param, l_param);
    }

    if down
        && matches!(info.vk_code, VK_PAUSE | VK_CANCEL)
        && is_key_down(VK_CONTROL)
        && is_key_down(VK_MENU)
    {
        let _ = send_event(KeyboardHookEvent::Failsafe);
        return CallNextHookEx(null_mut(), n_code, w_param, l_param);
    }

    let event = KeyboardHookEvent::Key {
        vk: info.vk_code as u16,
        scan_code: info.scan_code as u16,
        down,
        extended: (info.flags & LLKHF_EXTENDED) != 0,
    };

    // A key held before the hook installed: let its first key-up reach the local
    // app so it isn't left stuck down. Still observe it (send_event) so the
    // forwarding loop's modifier tracking stays consistent, but do not suppress.
    if up {
        if let Some(slot) = PREHELD_KEYS.get() {
            if let Ok(mut preheld) = slot.lock() {
                if take_preheld_passthrough(&mut preheld, info.vk_code as u16, up) {
                    drop(preheld);
                    let _ = send_event(event);
                    return CallNextHookEx(null_mut(), n_code, w_param, l_param);
                }
            }
        }
    }

    if send_event(event) && !PASSTHROUGH.load(Ordering::SeqCst) {
        // Suppress local keyboard input while hook capture is active. In IME
        // pass-through the event is still observed above but flows on to the
        // local composition window instead of being swallowed.
        return 1;
    }

    CallNextHookEx(null_mut(), n_code, w_param, l_param)
}

fn is_key_down(vk: i32) -> bool {
    let state = unsafe { GetAsyncKeyState(vk) };
    (state as u16 & 0x8000) != 0
}

fn send_event(event: KeyboardHookEvent) -> bool {
    let Some(slot) = EVENT_SENDER.get() else {
        return false;
    };

    let Ok(guard) = slot.lock() else {
        return false;
    };

    let Some(sender) = guard.as_ref() else {
        return false;
    };

    sender.send(event).is_ok()
}

fn clear_sender() {
    if let Some(slot) = EVENT_SENDER.get() {
        if let Ok(mut guard) = slot.lock() {
            *guard = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::take_preheld_passthrough;

    #[test]
    fn preheld_passes_first_keyup_then_resumes_suppression() {
        let mut preheld = vec![0x11u16, 0x41]; // Ctrl and 'A' held before capture

        // A key-down is never passed through as pre-held (only the up matters).
        assert!(!take_preheld_passthrough(&mut preheld, 0x11, false));
        // First key-up of a pre-held key passes through and is consumed.
        assert!(take_preheld_passthrough(&mut preheld, 0x11, true));
        // A second key-up of the same key is now suppressed normally.
        assert!(!take_preheld_passthrough(&mut preheld, 0x11, true));
        // The other pre-held key is still pending.
        assert!(take_preheld_passthrough(&mut preheld, 0x41, true));
        assert!(preheld.is_empty());
        // A key that was never pre-held is suppressed as usual.
        assert!(!take_preheld_passthrough(&mut preheld, 0x42, true));
    }
}
