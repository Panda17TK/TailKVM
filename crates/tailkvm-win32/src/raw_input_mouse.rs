//! Raw Input (`WM_INPUT`) mouse capture — phase-A PoC.
//!
//! Reads HID-level *relative* mouse deltas (`RAWMOUSE.lLastX/lLastY`) via a
//! message-only window, before OS pointer acceleration / clipping / DPI
//! scaling. This is the groundwork for replacing the
//! `GetCursorPos`/`SetCursorPos` warp loop's delta source (see
//! `docs/raw-input-mouse-design.md`). This module only *observes* deltas; it
//! does not move the cursor or inject anything. Wiring it into remote mode is a
//! later step after on-hardware validation.

use std::{
    ffi::c_void,
    mem::size_of,
    ptr::null_mut,
    sync::{
        mpsc::{self, Sender},
        Mutex, OnceLock,
    },
    thread,
    time::Duration,
};
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, WPARAM},
    System::LibraryLoader::GetModuleHandleW,
    UI::{
        Input::{
            GetRawInputData, RegisterRawInputDevices, HRAWINPUT, RAWINPUT, RAWINPUTDEVICE,
            RAWINPUTHEADER, RIDEV_INPUTSINK, RID_INPUT, RIM_TYPEMOUSE,
        },
        WindowsAndMessaging::{
            CreateWindowExW, DefWindowProcW, DispatchMessageW, PeekMessageW, RegisterClassW,
            TranslateMessage, MSG, PM_REMOVE, WM_INPUT, WNDCLASSW,
        },
    },
};

/// `RAWMOUSE.usFlags`: set when the device reports absolute coordinates
/// (tablets, some RDP/VM mice). Relative mice leave this bit clear.
const MOUSE_MOVE_ABSOLUTE: u16 = 0x0001;
/// HID usage page / usage identifying a generic mouse.
const HID_USAGE_PAGE_GENERIC: u16 = 0x01;
const HID_USAGE_GENERIC_MOUSE: u16 = 0x02;
/// `HWND_MESSAGE` — parent for a message-only window.
const HWND_MESSAGE: HWND = -3isize as HWND;

/// Decide the relative movement to forward from a raw mouse report.
///
/// Returns `Some((dx, dy))` for a non-zero *relative* movement, and `None` for
/// absolute-coordinate devices or zero movement. Pure logic, unit-tested.
pub fn relative_delta(us_flags: u16, last_x: i32, last_y: i32) -> Option<(i32, i32)> {
    if (us_flags & MOUSE_MOVE_ABSOLUTE) != 0 {
        return None;
    }
    if last_x == 0 && last_y == 0 {
        return None;
    }
    Some((last_x, last_y))
}

static DELTA_SENDER: OnceLock<Mutex<Option<Sender<(i32, i32)>>>> = OnceLock::new();

pub struct RawMouseHandle {
    stop_tx: Option<mpsc::Sender<()>>,
    join_handle: Option<thread::JoinHandle<()>>,
}

impl Drop for RawMouseHandle {
    fn drop(&mut self) {
        if let Some(stop_tx) = self.stop_tx.take() {
            let _ = stop_tx.send(());
        }
        if let Some(join_handle) = self.join_handle.take() {
            let _ = join_handle.join();
        }
    }
}

/// Start raw-input mouse capture. Relative deltas are sent on `delta_tx` until
/// the returned handle is dropped.
pub fn start_raw_mouse_capture(delta_tx: Sender<(i32, i32)>) -> Result<RawMouseHandle, String> {
    let sender_slot = DELTA_SENDER.get_or_init(|| Mutex::new(None));

    {
        let mut guard = sender_slot
            .lock()
            .map_err(|_| "raw mouse sender mutex poisoned".to_string())?;
        if guard.is_some() {
            return Err("raw mouse capture is already running".to_string());
        }
        *guard = Some(delta_tx);
    }

    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();

    let join_handle = thread::spawn(move || {
        let hwnd = match create_message_window() {
            Ok(hwnd) => hwnd,
            Err(err) => {
                let _ = ready_tx.send(Err(err));
                clear_sender();
                return;
            }
        };

        let device = RAWINPUTDEVICE {
            usUsagePage: HID_USAGE_PAGE_GENERIC,
            usUsage: HID_USAGE_GENERIC_MOUSE,
            dwFlags: RIDEV_INPUTSINK,
            hwndTarget: hwnd,
        };

        let registered =
            unsafe { RegisterRawInputDevices(&device, 1, size_of::<RAWINPUTDEVICE>() as u32) };
        if registered == 0 {
            let _ = ready_tx.send(Err("RegisterRawInputDevices failed".to_string()));
            unsafe {
                windows_sys::Win32::UI::WindowsAndMessaging::DestroyWindow(hwnd);
            }
            clear_sender();
            return;
        }

        let _ = ready_tx.send(Ok(()));

        let mut msg = MSG {
            hwnd: null_mut(),
            message: 0,
            wParam: 0,
            lParam: 0,
            time: 0,
            pt: windows_sys::Win32::Foundation::POINT { x: 0, y: 0 },
        };

        loop {
            while unsafe { PeekMessageW(&mut msg, hwnd, 0, 0, PM_REMOVE) } != 0 {
                unsafe {
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }

            if stop_rx.try_recv().is_ok() {
                break;
            }

            thread::sleep(Duration::from_millis(2));
        }

        // Unregister the device and tear down the window.
        let remove = RAWINPUTDEVICE {
            usUsagePage: HID_USAGE_PAGE_GENERIC,
            usUsage: HID_USAGE_GENERIC_MOUSE,
            dwFlags: windows_sys::Win32::UI::Input::RIDEV_REMOVE,
            hwndTarget: null_mut(),
        };
        unsafe {
            RegisterRawInputDevices(&remove, 1, size_of::<RAWINPUTDEVICE>() as u32);
            windows_sys::Win32::UI::WindowsAndMessaging::DestroyWindow(hwnd);
        }

        clear_sender();
    });

    match ready_rx.recv_timeout(Duration::from_secs(2)) {
        Ok(Ok(())) => Ok(RawMouseHandle {
            stop_tx: Some(stop_tx),
            join_handle: Some(join_handle),
        }),
        Ok(Err(err)) => {
            let _ = stop_tx.send(());
            let _ = join_handle.join();
            Err(err)
        }
        Err(err) => {
            let _ = stop_tx.send(());
            let _ = join_handle.join();
            clear_sender();
            Err(format!("raw mouse capture did not become ready: {err}"))
        }
    }
}

fn create_message_window() -> Result<HWND, String> {
    let class_name: Vec<u16> = "TailKVMRawInputWindow\0".encode_utf16().collect();

    let hinstance = unsafe { GetModuleHandleW(null_mut()) };

    let wnd_class = WNDCLASSW {
        style: 0,
        lpfnWndProc: Some(raw_input_wnd_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: hinstance,
        hIcon: null_mut(),
        hCursor: null_mut(),
        hbrBackground: null_mut(),
        lpszMenuName: null_mut(),
        lpszClassName: class_name.as_ptr(),
    };

    // Ignore the result: a previous start may have already registered the class,
    // in which case CreateWindowExW below still succeeds with the class name.
    unsafe {
        RegisterClassW(&wnd_class);
    }

    let hwnd = unsafe {
        CreateWindowExW(
            0,
            class_name.as_ptr(),
            class_name.as_ptr(),
            0,
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            null_mut(),
            hinstance,
            null_mut(),
        )
    };

    if hwnd.is_null() {
        Err("CreateWindowExW (message-only) failed".to_string())
    } else {
        Ok(hwnd)
    }
}

unsafe extern "system" fn raw_input_wnd_proc(
    hwnd: HWND,
    msg: u32,
    w_param: WPARAM,
    l_param: LPARAM,
) -> LRESULT {
    if msg == WM_INPUT {
        handle_wm_input(l_param);
    }
    DefWindowProcW(hwnd, msg, w_param, l_param)
}

unsafe fn handle_wm_input(l_param: LPARAM) {
    let h_raw = l_param as HRAWINPUT;
    let header_size = size_of::<RAWINPUTHEADER>() as u32;

    // First query the required buffer size.
    let mut size: u32 = 0;
    let query = GetRawInputData(h_raw, RID_INPUT, null_mut(), &mut size, header_size);
    if query != 0 || size == 0 {
        return;
    }

    let mut buffer = vec![0u8; size as usize];
    let copied = GetRawInputData(
        h_raw,
        RID_INPUT,
        buffer.as_mut_ptr() as *mut c_void,
        &mut size,
        header_size,
    );
    if copied == 0 || copied == u32::MAX {
        return;
    }

    let raw = &*(buffer.as_ptr() as *const RAWINPUT);
    if raw.header.dwType != RIM_TYPEMOUSE as u32 {
        return;
    }

    let mouse = &raw.data.mouse;
    if let Some((dx, dy)) = relative_delta(mouse.usFlags, mouse.lLastX, mouse.lLastY) {
        send_delta(dx, dy);
    }
}

fn send_delta(dx: i32, dy: i32) -> bool {
    let Some(slot) = DELTA_SENDER.get() else {
        return false;
    };
    let Ok(guard) = slot.lock() else {
        return false;
    };
    let Some(sender) = guard.as_ref() else {
        return false;
    };
    sender.send((dx, dy)).is_ok()
}

fn clear_sender() {
    if let Some(slot) = DELTA_SENDER.get() {
        if let Ok(mut guard) = slot.lock() {
            *guard = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_delta_forwards_nonzero_relative_movement() {
        assert_eq!(relative_delta(0, 5, -3), Some((5, -3)));
        assert_eq!(relative_delta(0, -1, 0), Some((-1, 0)));
    }

    #[test]
    fn relative_delta_ignores_zero_movement() {
        assert_eq!(relative_delta(0, 0, 0), None);
    }

    #[test]
    fn relative_delta_ignores_absolute_devices() {
        // Absolute-coordinate report (tablet / some VM mice) is not a delta.
        assert_eq!(relative_delta(MOUSE_MOVE_ABSOLUTE, 100, 200), None);
        assert_eq!(relative_delta(MOUSE_MOVE_ABSOLUTE | 0x02, 100, 200), None);
    }
}
