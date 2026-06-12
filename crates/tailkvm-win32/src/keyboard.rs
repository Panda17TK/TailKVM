use crate::input::{send_input, Input, InputUnion, KeyboardInput, INPUT_KEYBOARD};
use std::ptr::null_mut;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetKeyboardLayout, ToUnicodeEx};
use windows_sys::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

const KEYEVENTF_EXTENDEDKEY: u32 = 0x0001;
const KEYEVENTF_KEYUP: u32 = 0x0002;
const KEYEVENTF_UNICODE: u32 = 0x0004;
const KEYEVENTF_SCANCODE: u32 = 0x0008;

/// VK_SHIFT high-bit index in the 256-entry key-state array used by
/// `ToUnicodeEx`.
const VK_SHIFT: usize = 0x10;
const VK_CAPITAL: usize = 0x14;
/// `ToUnicodeEx` flag (Win10 1607+): do not change the kernel keyboard state,
/// so resolving a character has no side effect on dead-key state.
const TU_NOSTATECHANGE: u32 = 0x4;

/// Resolve the character(s) a key produces under the *controller's* active
/// keyboard layout, so JIS/US symbol-position differences are bridged before
/// the text is injected on the receiver as layout-independent Unicode.
///
/// `shift` / `caps` fold the relevant modifier state into the resolution
/// (built explicitly rather than read from the unreliable thread keyboard
/// state inside a hook). Returns `None` for dead keys or keys that produce no
/// character. This is IME-*off* resolution: kana→kanji composition is not
/// handled here (see TASK_LOG Task 9D phase 3).
pub fn resolve_key_text(vk: u16, scan_code: u16, shift: bool, caps: bool) -> Option<String> {
    let mut key_state = [0u8; 256];
    if shift {
        key_state[VK_SHIFT] = 0x80;
    }
    if caps {
        key_state[VK_CAPITAL] = 0x01; // toggle bit
    }

    let hkl = unsafe {
        let hwnd = GetForegroundWindow();
        let thread_id = if hwnd.is_null() {
            0
        } else {
            GetWindowThreadProcessId(hwnd, null_mut())
        };
        GetKeyboardLayout(thread_id)
    };

    let mut buf = [0u16; 8];
    let written = unsafe {
        ToUnicodeEx(
            vk as u32,
            scan_code as u32,
            key_state.as_ptr(),
            buf.as_mut_ptr(),
            buf.len() as i32,
            TU_NOSTATECHANGE,
            hkl,
        )
    };

    if written <= 0 {
        // 0 = no translation, -1 = dead key (no committed character yet).
        return None;
    }

    Some(String::from_utf16_lossy(&buf[..written as usize]))
}

pub fn send_keyboard_text(text: &str) -> Result<(), String> {
    for unit in text.encode_utf16() {
        send_unicode_unit(unit, false)?;
        send_unicode_unit(unit, true)?;
    }

    Ok(())
}

pub fn send_key_event(vk: u16, scan_code: u16, down: bool, extended: bool) -> Result<(), String> {
    let mut flags = 0u32;

    if !down {
        flags |= KEYEVENTF_KEYUP;
    }

    let (w_vk, w_scan) = if scan_code != 0 {
        flags |= KEYEVENTF_SCANCODE;
        (0, scan_code)
    } else {
        (vk, 0)
    };

    if extended {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }

    send_keyboard_input(w_vk, w_scan, flags)
}

fn send_unicode_unit(unit: u16, key_up: bool) -> Result<(), String> {
    let mut flags = KEYEVENTF_UNICODE;

    if key_up {
        flags |= KEYEVENTF_KEYUP;
    }

    send_keyboard_input(0, unit, flags)
}

/// `VK__none_` (0xFF, "no mapping") used for the hook health marker: a key-up
/// of an unassigned virtual key is a no-op for every application.
const VK_NONE: u16 = 0xFF;

/// Inject the keyboard-hook health marker (see
/// [`crate::input::HEALTH_MARKER_EXTRA_INFO`]): a tagged key-up of an unused
/// VK. A live hook swallows it before any application sees it; an unseen
/// marker means the hook was silently removed by the OS.
pub fn send_hook_health_marker() -> Result<(), String> {
    send_keyboard_input_tagged(
        VK_NONE,
        0,
        KEYEVENTF_KEYUP,
        crate::input::HEALTH_MARKER_EXTRA_INFO,
    )
}

fn send_keyboard_input(w_vk: u16, w_scan: u16, flags: u32) -> Result<(), String> {
    send_keyboard_input_tagged(w_vk, w_scan, flags, 0)
}

fn send_keyboard_input_tagged(
    w_vk: u16,
    w_scan: u16,
    flags: u32,
    extra_info: usize,
) -> Result<(), String> {
    let input = Input {
        input_type: INPUT_KEYBOARD,
        anonymous: InputUnion {
            ki: KeyboardInput {
                w_vk,
                w_scan,
                dw_flags: flags,
                time: 0,
                dw_extra_info: extra_info,
            },
        },
    };

    if send_input(&input) == 1 {
        Ok(())
    } else {
        Err(format!(
            "SendInput keyboard failed. vk=0x{w_vk:02x}, scan=0x{w_scan:02x}, flags=0x{flags:04x}"
        ))
    }
}
