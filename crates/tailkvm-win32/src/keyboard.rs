use std::mem::size_of;

const INPUT_KEYBOARD: u32 = 1;

const KEYEVENTF_EXTENDEDKEY: u32 = 0x0001;
const KEYEVENTF_KEYUP: u32 = 0x0002;
const KEYEVENTF_UNICODE: u32 = 0x0004;
const KEYEVENTF_SCANCODE: u32 = 0x0008;

#[repr(C)]
#[derive(Clone, Copy)]
struct KeyboardInput {
    w_vk: u16,
    w_scan: u16,
    dw_flags: u32,
    time: u32,
    dw_extra_info: usize,
}

#[repr(C)]
union InputUnion {
    ki: KeyboardInput,
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

fn send_keyboard_input(w_vk: u16, w_scan: u16, flags: u32) -> Result<(), String> {
    let input = Input {
        input_type: INPUT_KEYBOARD,
        anonymous: InputUnion {
            ki: KeyboardInput {
                w_vk,
                w_scan,
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
            "SendInput keyboard failed. vk=0x{w_vk:02x}, scan=0x{w_scan:02x}, flags=0x{flags:04x}"
        ))
    }
}
