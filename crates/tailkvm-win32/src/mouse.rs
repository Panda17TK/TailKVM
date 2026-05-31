use std::mem::size_of;

const INPUT_MOUSE: u32 = 0;
const MOUSEEVENTF_MOVE: u32 = 0x0001;

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
    let input = Input {
        input_type: INPUT_MOUSE,
        anonymous: InputUnion {
            mi: MouseInput {
                dx,
                dy,
                mouse_data: 0,
                dw_flags: MOUSEEVENTF_MOVE,
                time: 0,
                dw_extra_info: 0,
            },
        },
    };

    let sent = unsafe { SendInput(1, &input as *const Input, size_of::<Input>() as i32) };

    if sent == 1 {
        Ok(())
    } else {
        Err("SendInput failed to move mouse.".to_string())
    }
}
