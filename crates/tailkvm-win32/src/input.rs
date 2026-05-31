//! Shared `SendInput` FFI surface for synthesizing input events.
//!
//! Both mouse and keyboard injection go through the Win32 `SendInput` API with
//! the same `INPUT` structure (tagged union over `MOUSEINPUT`/`KEYBDINPUT`).
//! Declaring `SendInput` and `INPUT` once here avoids the
//! `clashing_extern_declarations` warning that arises when two modules each
//! declare `SendInput` with their own incompatible `Input` pointer types.

use std::mem::size_of;

pub const INPUT_MOUSE: u32 = 0;
pub const INPUT_KEYBOARD: u32 = 1;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct MouseInput {
    pub dx: i32,
    pub dy: i32,
    pub mouse_data: u32,
    pub dw_flags: u32,
    pub time: u32,
    pub dw_extra_info: usize,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct KeyboardInput {
    pub w_vk: u16,
    pub w_scan: u16,
    pub dw_flags: u32,
    pub time: u32,
    pub dw_extra_info: usize,
}

#[repr(C)]
pub union InputUnion {
    pub mi: MouseInput,
    pub ki: KeyboardInput,
}

#[repr(C)]
pub struct Input {
    pub input_type: u32,
    pub anonymous: InputUnion,
}

#[link(name = "user32")]
unsafe extern "system" {
    fn SendInput(c_inputs: u32, p_inputs: *const Input, cb_size: i32) -> u32;
}

/// Inject a single synthesized input event.
///
/// Returns the number of events successfully inserted into the input stream
/// (`1` on success, `0` if the call was blocked, e.g. by UIPI).
pub fn send_input(input: &Input) -> u32 {
    unsafe { SendInput(1, input as *const Input, size_of::<Input>() as i32) }
}
