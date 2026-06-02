//! Workstation lock / secure-desktop detection (issue 3).
//!
//! On the lock screen, UAC prompt, or other secure desktop, a normal
//! interactive process cannot open the *input* desktop. We use that as a
//! heuristic for "this machine is locked / input is unavailable", which the UI
//! surfaces so the user understands when sharing is suspended. (Input injection
//! and low-level hooks are disabled by the OS on the secure desktop anyway.)

use windows_sys::Win32::System::StationsAndDesktops::{CloseDesktop, OpenInputDesktop};

const DESKTOP_READOBJECTS: u32 = 0x0001;

/// Returns `true` when the interactive input desktop cannot be opened, i.e. the
/// workstation is locked or a secure desktop (UAC) is active.
pub fn is_workstation_locked() -> bool {
    unsafe {
        let desktop = OpenInputDesktop(0, 0, DESKTOP_READOBJECTS);
        if desktop.is_null() {
            true
        } else {
            CloseDesktop(desktop);
            false
        }
    }
}
