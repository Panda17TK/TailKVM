//! Pure key-routing classification for the hybrid keyboard path (Task 9D
//! phase 2). Decides, per key, whether to reproduce the *physical* key (scan/vk
//! over `KeyboardKey`) or the *character* it produces (Unicode over
//! `KeyboardText`), or to handle it locally (IME toggles).
//!
//! This module has no FFI and is fully unit-tested; the actual character
//! resolution (`ToUnicodeEx`) and wiring live elsewhere.

/// Which transport path a key should take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyRoute {
    /// Reproduce the physical key (scan code / vk). Used for modifiers,
    /// control/navigation/function keys, and any combo with Ctrl/Alt/Win
    /// (shortcuts like Ctrl+C, Win+X, Alt+Tab).
    Physical,
    /// Reproduce the produced character via layout-independent Unicode
    /// injection. Used for printable keys (incl. Shift for caps/symbols) so
    /// JIS/US symbol-position differences are resolved on the controller.
    Character,
    /// Handle locally only; do not forward. Used for IME toggle / conversion
    /// keys (半角/全角, 変換, 無変換, かな, Kanji) so the receiver's IME state is
    /// never flipped (the receiver is driven as direct Unicode input).
    ImeLocal,
}

/// Modifier categories tracked to decide routing. Shift is intentionally not a
/// "command" modifier: Shift+letter is still a character (uppercase).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Modifier {
    Ctrl,
    Alt,
    Win,
    Shift,
}

/// Map a virtual-key code to the modifier it represents, if any. The hook
/// forwarding loop uses this to maintain modifier state across events.
pub fn modifier_kind(vk: u16) -> Option<Modifier> {
    match vk {
        0x10 | 0xA0 | 0xA1 => Some(Modifier::Shift), // VK_SHIFT / L / R
        0x11 | 0xA2 | 0xA3 => Some(Modifier::Ctrl),  // VK_CONTROL / L / R
        0x12 | 0xA4 | 0xA5 => Some(Modifier::Alt),   // VK_MENU / L / R
        0x5B | 0x5C => Some(Modifier::Win),          // VK_LWIN / VK_RWIN
        _ => None,
    }
}

/// IME toggle / conversion keys that must not be forwarded.
fn is_ime_key(vk: u16) -> bool {
    matches!(
        vk,
        0x15 // VK_KANA / VK_HANGUL
        | 0x16 // VK_IME_ON
        | 0x17 // VK_IME_OFF
        | 0x19 // VK_KANJI
        | 0x1C // VK_CONVERT
        | 0x1D // VK_NONCONVERT
        | 0x1E // VK_ACCEPT
        | 0x1F // VK_MODECHANGE
    ) || (0xF0..=0xFF).contains(&vk) // VK_OEM_ATTN..VK_DBE_* / IME range (半角/全角 etc.)
}

/// Control, navigation, and function keys that have no layout-dependent
/// character and should always go through the physical path.
fn is_control_nav_or_function(vk: u16) -> bool {
    matches!(
        vk,
        0x08 // Backspace
        | 0x09 // Tab
        | 0x0D // Enter
        | 0x1B // Escape
        | 0x20 // Space (reliable cross-layout as a physical key)
        | 0x13 // Pause
        | 0x14 // CapsLock
        | 0x2C..=0x2E // PrintScreen, Insert, Delete
        | 0x21..=0x28 // PageUp/Down, End, Home, arrows
        | 0x5D // Apps (context menu)
        | 0x90 // NumLock
        | 0x91 // ScrollLock
        | 0x70..=0x87 // F1..F24
    )
}

/// Decide the route for a key press given the currently-held command modifiers.
///
/// `ctrl`/`alt`/`win` reflect whether those modifiers are currently down (Shift
/// is folded into character resolution instead). Precedence: IME keys are
/// always local; modifier keys themselves are physical; any Ctrl/Alt/Win combo
/// is physical (shortcut); control/nav/function keys are physical; everything
/// else is a character.
pub fn classify_key(vk: u16, ctrl: bool, alt: bool, win: bool) -> KeyRoute {
    if is_ime_key(vk) {
        return KeyRoute::ImeLocal;
    }
    if modifier_kind(vk).is_some() {
        return KeyRoute::Physical;
    }
    if ctrl || alt || win {
        return KeyRoute::Physical;
    }
    if is_control_nav_or_function(vk) {
        return KeyRoute::Physical;
    }
    KeyRoute::Character
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modifier_kind_maps_left_right_and_generic() {
        assert_eq!(modifier_kind(0x10), Some(Modifier::Shift));
        assert_eq!(modifier_kind(0xA1), Some(Modifier::Shift));
        assert_eq!(modifier_kind(0x11), Some(Modifier::Ctrl));
        assert_eq!(modifier_kind(0xA5), Some(Modifier::Alt));
        assert_eq!(modifier_kind(0x5B), Some(Modifier::Win));
        assert_eq!(modifier_kind(0x41), None); // 'A'
    }

    #[test]
    fn plain_letters_are_characters() {
        assert_eq!(classify_key(0x41, false, false, false), KeyRoute::Character); // A
        assert_eq!(classify_key(0xBA, false, false, false), KeyRoute::Character);
        // OEM_1 (;: / :*)
        // Shift does not force physical; it is folded into character resolution.
        // (classify_key takes only command modifiers, so Shift+A is still Character.)
    }

    #[test]
    fn ctrl_alt_win_combos_are_physical() {
        assert_eq!(classify_key(0x43, true, false, false), KeyRoute::Physical); // Ctrl+C
        assert_eq!(classify_key(0x09, false, true, false), KeyRoute::Physical); // Alt+Tab
        assert_eq!(classify_key(0x58, false, false, true), KeyRoute::Physical); // Win+X
    }

    #[test]
    fn modifier_control_nav_function_keys_are_physical() {
        assert_eq!(classify_key(0x11, false, false, false), KeyRoute::Physical); // Ctrl key itself
        assert_eq!(classify_key(0x5B, false, false, false), KeyRoute::Physical); // Win key itself
        assert_eq!(classify_key(0x0D, false, false, false), KeyRoute::Physical); // Enter
        assert_eq!(classify_key(0x25, false, false, false), KeyRoute::Physical); // ArrowLeft
        assert_eq!(classify_key(0x70, false, false, false), KeyRoute::Physical); // F1
        assert_eq!(classify_key(0x20, false, false, false), KeyRoute::Physical);
        // Space
    }

    #[test]
    fn ime_keys_are_local_even_without_modifiers() {
        assert_eq!(classify_key(0x19, false, false, false), KeyRoute::ImeLocal); // Kanji
        assert_eq!(classify_key(0x1C, false, false, false), KeyRoute::ImeLocal); // Convert
        assert_eq!(classify_key(0x1D, false, false, false), KeyRoute::ImeLocal); // NonConvert
        assert_eq!(classify_key(0xF3, false, false, false), KeyRoute::ImeLocal); // 半角/全角 (OEM)
        assert_eq!(classify_key(0x15, false, false, false), KeyRoute::ImeLocal);
        // Kana
    }
}
