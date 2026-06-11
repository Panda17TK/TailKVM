#[derive(Debug, Clone, Copy)]
pub struct CursorPosition {
    pub x: i32,
    pub y: i32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct Point {
    x: i32,
    y: i32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct Rect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

const VK_CONTROL: i32 = 0x11;
const VK_MENU: i32 = 0x12;
const VK_PAUSE: i32 = 0x13;

#[link(name = "user32")]
unsafe extern "system" {
    fn GetCursorPos(lp_point: *mut Point) -> i32;
    fn SetCursorPos(x: i32, y: i32) -> i32;
    fn GetAsyncKeyState(v_key: i32) -> i16;
    fn ClipCursor(lp_rect: *const Rect) -> i32;
}

/// Confine the cursor to a 1x1 region at `(x, y)` so it cannot interact with
/// local UI while the remote is being controlled (roadmap C3). MUST be paired
/// with [`release_cursor_confine`] on every stop path; the OS also releases the
/// clip automatically when the process exits.
pub fn confine_cursor(x: i32, y: i32) -> Result<(), String> {
    let rect = Rect {
        left: x,
        top: y,
        right: x + 1,
        bottom: y + 1,
    };
    let ok = unsafe { ClipCursor(&rect as *const Rect) };
    if ok == 0 {
        Err("ClipCursor failed.".to_string())
    } else {
        Ok(())
    }
}

/// Release any cursor confinement set by [`confine_cursor`].
pub fn release_cursor_confine() {
    unsafe {
        ClipCursor(std::ptr::null());
    }
}

pub fn get_cursor_position() -> Result<CursorPosition, String> {
    let mut point = Point { x: 0, y: 0 };

    let ok = unsafe { GetCursorPos(&mut point as *mut Point) };

    if ok == 0 {
        Err("GetCursorPos failed.".to_string())
    } else {
        Ok(CursorPosition {
            x: point.x,
            y: point.y,
        })
    }
}

pub fn set_cursor_position(x: i32, y: i32) -> Result<(), String> {
    let ok = unsafe { SetCursorPos(x, y) };

    if ok == 0 {
        Err("SetCursorPos failed.".to_string())
    } else {
        Ok(())
    }
}

pub fn is_ctrl_alt_pause_pressed() -> bool {
    is_key_down(VK_CONTROL) && is_key_down(VK_MENU) && is_key_down(VK_PAUSE)
}

/// Whether the given virtual key is currently held (async key state). Used to
/// seed modifier tracking when keyboard forwarding starts: keys already held
/// *before* the hook was installed never appear in the event stream, so
/// stream-only tracking would misclassify e.g. a Ctrl+drag edge crossing.
pub fn is_vk_down(v_key: i32) -> bool {
    is_key_down(v_key)
}

fn is_key_down(v_key: i32) -> bool {
    let state = unsafe { GetAsyncKeyState(v_key) };
    (state as u16 & 0x8000) != 0
}
