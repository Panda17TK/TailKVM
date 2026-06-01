//! Keyboard layout / input-locale foundation.
//!
//! This module is the groundwork for handling layout differences between hosts
//! (notably JIS vs US physical keyboards and Japanese input locales). It only
//! *identifies* the active layout for now; the actual key remapping / IME
//! handling design is tracked separately (see TASK_LOG Task 9D).
//!
//! Two independent dimensions matter and are captured here:
//!
//! * **Input locale (HKL)** — `GetKeyboardLayout` for the foreground thread.
//!   The low word is the input-locale (language) identifier, e.g. `0x0411`
//!   for Japanese. This reflects the *software* layout the OS uses to map
//!   scan codes to characters.
//! * **Physical keyboard type** — `GetKeyboardType`. Type `7` indicates a
//!   Japanese (JIS) keyboard. This reflects the *hardware* the user types on,
//!   which determines which physical keys exist (e.g. 変換/無変換, ¥, JIS
//!   bracket positions) independent of the input locale.
//!
//! Note: the IME conversion mode (半角/全角, kana/romaji, conversion on/off) is
//! NOT part of HKL and is intentionally out of scope for this foundation.

use serde::Serialize;
use std::ptr::null_mut;
use windows_sys::Win32::UI::{
    Input::KeyboardAndMouse::{GetKeyboardLayout, GetKeyboardType},
    WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId},
};

/// LANG_JAPANESE primary language identifier.
const LANG_JAPANESE: u16 = 0x11;
/// `GetKeyboardType(0)` value for a Japanese (JIS) keyboard.
const KEYBOARD_TYPE_JAPANESE: i32 = 7;

/// Snapshot of the active keyboard layout and physical keyboard type.
#[derive(Debug, Clone, Serialize)]
pub struct KeyboardLayoutInfo {
    /// Raw HKL handle value (for diagnostics / logging).
    pub hkl: u64,
    /// Input-locale identifier (low word of HKL), e.g. `0x0411`.
    pub language_id: u16,
    /// Primary language identifier (`language_id & 0x3FF`), e.g. `0x11`.
    pub primary_language: u16,
    /// Whether the input locale is Japanese.
    pub is_japanese_locale: bool,
    /// `GetKeyboardType(0)`: physical keyboard type (7 = Japanese/JIS).
    pub keyboard_type: i32,
    /// `GetKeyboardType(1)`: OEM-dependent keyboard subtype.
    pub keyboard_subtype: i32,
    /// `GetKeyboardType(2)`: number of function keys.
    pub function_keys: i32,
    /// Whether the physical keyboard is a JIS keyboard.
    pub is_jis_keyboard: bool,
    /// Human-readable summary label.
    pub label: String,
}

impl KeyboardLayoutInfo {
    /// Compare against a peer's reported layout and return a human-readable
    /// warning when the input locale or physical keyboard type differ.
    ///
    /// Returns `None` when both axes match (no warning needed).
    pub fn mismatch_with(&self, peer_language_id: u16, peer_keyboard_type: i32) -> Option<String> {
        let mut diffs = Vec::new();

        if self.language_id != peer_language_id {
            diffs.push(format!(
                "input locale (local=0x{:04X}, peer=0x{:04X})",
                self.language_id, peer_language_id
            ));
        }

        if self.keyboard_type != peer_keyboard_type {
            diffs.push(format!(
                "physical keyboard type (local={}, peer={})",
                self.keyboard_type, peer_keyboard_type
            ));
        }

        if diffs.is_empty() {
            None
        } else {
            Some(format!(
                "Keyboard layout mismatch: {}. Symbol/key mapping may differ; prefer Keyboard text for reliable input.",
                diffs.join("; ")
            ))
        }
    }
}

/// Read the keyboard layout of the foreground window's thread, plus the
/// physical keyboard type of the current machine.
///
/// Falls back to the calling thread's layout (`GetKeyboardLayout(0)`) when no
/// foreground window is available.
pub fn current_keyboard_layout() -> KeyboardLayoutInfo {
    let thread_id = unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.is_null() {
            0
        } else {
            GetWindowThreadProcessId(hwnd, null_mut())
        }
    };

    let hkl = unsafe { GetKeyboardLayout(thread_id) };
    let hkl_value = hkl as usize as u64;

    let language_id = (hkl_value & 0xFFFF) as u16;
    let primary_language = language_id & 0x03FF;
    let is_japanese_locale = primary_language == LANG_JAPANESE;

    let keyboard_type = unsafe { GetKeyboardType(0) };
    let keyboard_subtype = unsafe { GetKeyboardType(1) };
    let function_keys = unsafe { GetKeyboardType(2) };
    let is_jis_keyboard = keyboard_type == KEYBOARD_TYPE_JAPANESE;

    let label = format!(
        "locale=0x{language_id:04X}{}, keyboard_type={keyboard_type}{}",
        if is_japanese_locale {
            " (Japanese)"
        } else {
            ""
        },
        if is_jis_keyboard { " (JIS)" } else { "" },
    );

    KeyboardLayoutInfo {
        hkl: hkl_value,
        language_id,
        primary_language,
        is_japanese_locale,
        keyboard_type,
        keyboard_subtype,
        function_keys,
        is_jis_keyboard,
        label,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a layout info with the two axes that `mismatch_with` compares.
    /// Other fields are diagnostic-only and irrelevant to the comparison.
    fn layout(language_id: u16, keyboard_type: i32) -> KeyboardLayoutInfo {
        KeyboardLayoutInfo {
            hkl: 0,
            language_id,
            primary_language: language_id & 0x03FF,
            is_japanese_locale: (language_id & 0x03FF) == LANG_JAPANESE,
            keyboard_type,
            keyboard_subtype: 0,
            function_keys: 12,
            is_jis_keyboard: keyboard_type == KEYBOARD_TYPE_JAPANESE,
            label: String::new(),
        }
    }

    #[test]
    fn no_warning_when_both_axes_match() {
        let jis = layout(0x0411, 7);
        assert!(jis.mismatch_with(0x0411, 7).is_none());

        let us = layout(0x0409, 4);
        assert!(us.mismatch_with(0x0409, 4).is_none());
    }

    #[test]
    fn warns_on_input_locale_difference_only() {
        // US host vs JIS peer locale, same physical keyboard type.
        let us = layout(0x0409, 7);
        let warning = us
            .mismatch_with(0x0411, 7)
            .expect("locale difference must warn");
        assert!(warning.contains("input locale"), "warning: {warning}");
        assert!(
            !warning.contains("physical keyboard type"),
            "should not mention keyboard type: {warning}"
        );
        // Both endpoints are reported in the message.
        assert!(warning.contains("0x0409"), "warning: {warning}");
        assert!(warning.contains("0x0411"), "warning: {warning}");
    }

    #[test]
    fn warns_on_keyboard_type_difference_only() {
        // Same locale, different physical keyboard (US 101 vs JIS).
        let us = layout(0x0409, 4);
        let warning = us
            .mismatch_with(0x0409, 7)
            .expect("keyboard type difference must warn");
        assert!(
            warning.contains("physical keyboard type"),
            "warning: {warning}"
        );
        assert!(
            !warning.contains("input locale"),
            "should not mention locale: {warning}"
        );
    }

    #[test]
    fn warns_on_both_axes_and_lists_both() {
        let us = layout(0x0409, 4);
        let warning = us
            .mismatch_with(0x0411, 7)
            .expect("double difference must warn");
        assert!(warning.contains("input locale"), "warning: {warning}");
        assert!(
            warning.contains("physical keyboard type"),
            "warning: {warning}"
        );
        // Guidance to fall back to reliable Unicode text injection.
        assert!(warning.contains("Keyboard text"), "warning: {warning}");
    }
}
