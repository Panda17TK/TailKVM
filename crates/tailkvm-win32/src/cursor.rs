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

#[link(name = "user32")]
unsafe extern "system" {
    fn GetCursorPos(lp_point: *mut Point) -> i32;
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
