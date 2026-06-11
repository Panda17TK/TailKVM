use crate::input::{send_input, Input, InputUnion, MouseInput, INPUT_MOUSE};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN,
};

const MOUSEEVENTF_MOVE: u32 = 0x0001;
const MOUSEEVENTF_LEFTDOWN: u32 = 0x0002;
const MOUSEEVENTF_LEFTUP: u32 = 0x0004;
const MOUSEEVENTF_RIGHTDOWN: u32 = 0x0008;
const MOUSEEVENTF_RIGHTUP: u32 = 0x0010;
const MOUSEEVENTF_MIDDLEDOWN: u32 = 0x0020;
const MOUSEEVENTF_MIDDLEUP: u32 = 0x0040;
const MOUSEEVENTF_XDOWN: u32 = 0x0080;
const MOUSEEVENTF_XUP: u32 = 0x0100;
const MOUSEEVENTF_WHEEL: u32 = 0x0800;
const MOUSEEVENTF_HWHEEL: u32 = 0x01000;
const MOUSEEVENTF_VIRTUALDESK: u32 = 0x4000;
const MOUSEEVENTF_ABSOLUTE: u32 = 0x8000;

const XBUTTON1: u32 = 0x0001;
const XBUTTON2: u32 = 0x0002;

pub fn send_relative_mouse_move(dx: i32, dy: i32) -> Result<(), String> {
    send_mouse_input(dx, dy, 0, MOUSEEVENTF_MOVE)
}

/// Move the cursor to the virtual-desktop pixel at offset `(x, y)` from the
/// virtual-screen origin by injecting a *real* mouse input event.
///
/// Unlike `SetCursorPos`, an injected `SendInput` move counts as mouse input,
/// so it un-suppresses a hidden cursor (no physical mouse attached, touch/pen
/// suppression, hide-pointer-while-typing) and resets the system idle timer.
/// `SetCursorPos` only rewrites the position and leaves a suppressed cursor
/// invisible — the remote-side "cursor disappears" bug.
///
/// Offsets are relative to the virtual-screen origin (`SM_XVIRTUALSCREEN`,
/// `SM_YVIRTUALSCREEN`), matching the controller's model of the remote screen
/// as `(0, 0, width, height)`. This also fixes multi-monitor layouts whose
/// virtual origin is negative, which plain `SetCursorPos(x, y)` could not reach.
pub fn send_absolute_mouse_move(x: i32, y: i32) -> Result<(), String> {
    // The virtual-screen origin (SM_X/YVIRTUALSCREEN) is intentionally not
    // queried: MOUSEEVENTF_VIRTUALDESK already anchors normalized (0, 0) at
    // the virtual desktop's top-left, so origin-relative offsets map directly.
    let (virtual_width, virtual_height) = unsafe {
        (
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    };

    if virtual_width <= 1 || virtual_height <= 1 {
        return Err(format!(
            "invalid virtual screen size: {virtual_width}x{virtual_height}"
        ));
    }

    let (nx, ny) = normalize_to_virtual_desk(x, y, virtual_width, virtual_height);

    send_mouse_input(
        nx,
        ny,
        0,
        MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
    )
}

/// Map a pixel offset from the virtual-screen origin onto the 0..=65535
/// normalized grid that `MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK`
/// expects. Input is clamped into the virtual desktop first so out-of-range
/// coordinates from the wire cannot inject garbage positions.
fn normalize_to_virtual_desk(x: i32, y: i32, width: i32, height: i32) -> (i32, i32) {
    let cx = x.clamp(0, width - 1);
    let cy = y.clamp(0, height - 1);
    let nx = ((cx as f64) * 65535.0 / ((width - 1) as f64)).round() as i32;
    let ny = ((cy as f64) * 65535.0 / ((height - 1) as f64)).round() as i32;
    (nx, ny)
}

#[cfg(test)]
mod tests {
    use super::normalize_to_virtual_desk;

    #[test]
    fn maps_origin_to_zero() {
        assert_eq!(normalize_to_virtual_desk(0, 0, 1920, 1080), (0, 0));
    }

    #[test]
    fn maps_last_pixel_to_full_scale() {
        assert_eq!(
            normalize_to_virtual_desk(1919, 1079, 1920, 1080),
            (65535, 65535)
        );
    }

    #[test]
    fn maps_center_to_half_scale() {
        let (nx, ny) = normalize_to_virtual_desk(960, 540, 1920, 1080);
        // Center of a 0..=size-1 grid lands within a rounding step of 65535/2.
        assert!((nx - 32768).abs() <= 18, "nx={nx}");
        assert!((ny - 32768).abs() <= 31, "ny={ny}");
    }

    #[test]
    fn clamps_out_of_range_coordinates() {
        assert_eq!(normalize_to_virtual_desk(-50, -1, 1920, 1080), (0, 0));
        assert_eq!(
            normalize_to_virtual_desk(99_999, 99_999, 1920, 1080),
            (65535, 65535)
        );
    }

    #[test]
    fn handles_wide_multi_monitor_span() {
        // 3840x1080 dual-monitor span: x in the second monitor still maps
        // proportionally onto the normalized grid.
        let (nx, _) = normalize_to_virtual_desk(2880, 0, 3840, 1080);
        let expected = ((2880.0_f64 * 65535.0) / 3839.0).round() as i32;
        assert_eq!(nx, expected);
    }
}

pub fn send_mouse_button(button: &str, down: bool) -> Result<(), String> {
    let normalized = button.trim().to_lowercase();

    let (mouse_data, flags) = match (normalized.as_str(), down) {
        ("left", true) => (0, MOUSEEVENTF_LEFTDOWN),
        ("left", false) => (0, MOUSEEVENTF_LEFTUP),

        ("right", true) => (0, MOUSEEVENTF_RIGHTDOWN),
        ("right", false) => (0, MOUSEEVENTF_RIGHTUP),

        ("middle", true) => (0, MOUSEEVENTF_MIDDLEDOWN),
        ("middle", false) => (0, MOUSEEVENTF_MIDDLEUP),

        ("x1", true) | ("xbutton1", true) | ("back", true) => (XBUTTON1, MOUSEEVENTF_XDOWN),
        ("x1", false) | ("xbutton1", false) | ("back", false) => (XBUTTON1, MOUSEEVENTF_XUP),

        ("x2", true) | ("xbutton2", true) | ("forward", true) => (XBUTTON2, MOUSEEVENTF_XDOWN),
        ("x2", false) | ("xbutton2", false) | ("forward", false) => (XBUTTON2, MOUSEEVENTF_XUP),

        _ => return Err(format!("unsupported mouse button: {button}")),
    };

    send_mouse_input(0, 0, mouse_data, flags)
}

pub fn send_mouse_wheel(delta: i32, horizontal: bool) -> Result<(), String> {
    let flags = if horizontal {
        MOUSEEVENTF_HWHEEL
    } else {
        MOUSEEVENTF_WHEEL
    };

    send_mouse_input(0, 0, delta as u32, flags)
}

fn send_mouse_input(dx: i32, dy: i32, mouse_data: u32, flags: u32) -> Result<(), String> {
    let input = Input {
        input_type: INPUT_MOUSE,
        anonymous: InputUnion {
            mi: MouseInput {
                dx,
                dy,
                mouse_data,
                dw_flags: flags,
                time: 0,
                dw_extra_info: 0,
            },
        },
    };

    if send_input(&input) == 1 {
        Ok(())
    } else {
        Err(format!(
            "SendInput failed. flags=0x{flags:04x}, mouse_data={mouse_data}"
        ))
    }
}
