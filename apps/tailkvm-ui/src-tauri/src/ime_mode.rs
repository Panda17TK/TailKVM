//! Explicit IME composition-mode state machine (requirements §9.1).
//!
//! Replaces the implicit `ime_capture.is_some()` × `is_composing()` pair
//! with four named states so routing, status display, and safety logic all
//! agree on what mode the controller is in. Pure logic, no FFI — the
//! keyboard forwarding loop drives it and owns the side effects (starting /
//! dropping the capture window, pass-through, stuck-key release).

/// TailKVM's composition-mode state (NOT the Windows IME open state — that
/// lives in `tailkvm_win32::ime_capture::ImeStateSnapshot`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ImeModeState {
    /// Composition mode disabled; keys follow the normal forwarding rules.
    Off,
    /// Mode enabled, no open composition: printable keys flow to the capture
    /// window, control/nav/shortcut keys go to the receiver physically.
    Armed,
    /// An uncommitted composition is open: keys flow to the local IME and
    /// only committed text reaches the receiver.
    Composing,
    /// Entry failed (capture window / focus): mode is parked until the next
    /// toggle retries.
    Suspended,
}

impl ImeModeState {
    /// Snake-case label for status display (`TcpSessionSnapshot.ime_mode`).
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            ImeModeState::Off => "off",
            ImeModeState::Armed => "armed",
            ImeModeState::Composing => "composing",
            ImeModeState::Suspended => "suspended",
        }
    }

    /// Whether the capture window should exist in this state.
    pub(crate) fn is_on(self) -> bool {
        matches!(self, ImeModeState::Armed | ImeModeState::Composing)
    }

    /// State requested by a toggle-key press (IME-MODE-001): Off and
    /// Suspended arm the mode (Suspended retries entry); Armed/Composing
    /// turn it off.
    pub(crate) fn on_toggle(self) -> ImeModeState {
        match self {
            ImeModeState::Off | ImeModeState::Suspended => ImeModeState::Armed,
            ImeModeState::Armed | ImeModeState::Composing => ImeModeState::Off,
        }
    }

    /// Track the live composition flag: Armed⇄Composing while the mode is
    /// on (IME-MODE-012/024 — ending a composition returns to Armed, never
    /// to Off). Off/Suspended are unaffected.
    pub(crate) fn observe_composition(self, composing: bool) -> ImeModeState {
        match (self, composing) {
            (ImeModeState::Armed, true) => ImeModeState::Composing,
            (ImeModeState::Composing, false) => ImeModeState::Armed,
            (state, _) => state,
        }
    }

    /// Entry into the mode failed (capture window error, focus abort).
    pub(crate) fn on_entry_failure(self) -> ImeModeState {
        ImeModeState::Suspended
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toggle_cycles_off_armed_off() {
        assert_eq!(ImeModeState::Off.on_toggle(), ImeModeState::Armed);
        assert_eq!(ImeModeState::Armed.on_toggle(), ImeModeState::Off);
        // Toggling out of an open composition also turns the mode off.
        assert_eq!(ImeModeState::Composing.on_toggle(), ImeModeState::Off);
        // A suspended mode retries entry on the next toggle.
        assert_eq!(ImeModeState::Suspended.on_toggle(), ImeModeState::Armed);
    }

    #[test]
    fn composition_moves_between_armed_and_composing_only() {
        // First printable opens a composition (IME-MODE-012).
        assert_eq!(
            ImeModeState::Armed.observe_composition(true),
            ImeModeState::Composing
        );
        // Commit/cancel returns to Armed, not Off (IME-MODE-024).
        assert_eq!(
            ImeModeState::Composing.observe_composition(false),
            ImeModeState::Armed
        );
        // No-ops when nothing changed.
        assert_eq!(
            ImeModeState::Armed.observe_composition(false),
            ImeModeState::Armed
        );
        assert_eq!(
            ImeModeState::Composing.observe_composition(true),
            ImeModeState::Composing
        );
        // Off/Suspended never react to stray composition flags.
        assert_eq!(
            ImeModeState::Off.observe_composition(true),
            ImeModeState::Off
        );
        assert_eq!(
            ImeModeState::Suspended.observe_composition(true),
            ImeModeState::Suspended
        );
    }

    #[test]
    fn entry_failure_parks_in_suspended() {
        assert_eq!(
            ImeModeState::Armed.on_entry_failure(),
            ImeModeState::Suspended
        );
    }

    #[test]
    fn capture_window_lifetime_matches_is_on() {
        assert!(!ImeModeState::Off.is_on());
        assert!(ImeModeState::Armed.is_on());
        assert!(ImeModeState::Composing.is_on());
        assert!(!ImeModeState::Suspended.is_on());
    }

    #[test]
    fn status_labels_are_snake_case() {
        assert_eq!(ImeModeState::Off.as_str(), "off");
        assert_eq!(ImeModeState::Armed.as_str(), "armed");
        assert_eq!(ImeModeState::Composing.as_str(), "composing");
        assert_eq!(ImeModeState::Suspended.as_str(), "suspended");
    }
}
