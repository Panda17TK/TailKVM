use std::mem::size_of;

const INPUT_MOUSE: u32 = 0;

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

const XBUTTON1: u32 = 0x0001;
const XBUTTON2: u32 = 0x0002;

#[repr(C)]
#[derive(Clone, Copy)]
struct MouseInput {
    dx: i32,
    dy: i32,
    mouse_data: u32,
    dw_flags: u32,
    time: u32,
    dw_extra_info: usize,
}

#[repr(C)]
union InputUnion {
    mi: MouseInput,
}

#[repr(C)]
struct Input {
    input_type: u32,
    anonymous: InputUnion,
}

#[link(name = "user32")]
unsafe extern "system" {
    fn SendInput(c_inputs: u32, p_inputs: *const Input, cb_size: i32) -> u32;
}

pub fn send_relative_mouse_move(dx: i32, dy: i32) -> Result<(), String> {
    send_mouse_input(dx, dy, 0, MOUSEEVENTF_MOVE)
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

    let sent = unsafe { SendInput(1, &input as *const Input, size_of::<Input>() as i32) };

    if sent == 1 {
        Ok(())
    } else {
        Err(format!(
            "SendInput failed. flags=0x{flags:04x}, mouse_data={mouse_data}"
        ))
    }
}
