use std::{
    ptr::null_mut,
    sync::{
        mpsc::{self, Sender},
        Mutex, OnceLock,
    },
    thread,
    time::Duration,
};
use windows_sys::Win32::{
    Foundation::{LPARAM, WPARAM},
    UI::{
        Input::KeyboardAndMouse::GetAsyncKeyState,
        WindowsAndMessaging::{
            CallNextHookEx, DispatchMessageW, PeekMessageW, SetWindowsHookExW, TranslateMessage,
            UnhookWindowsHookEx, MSG, PM_REMOVE, WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP,
            WM_SYSKEYDOWN, WM_SYSKEYUP,
        },
    },
};

const LLKHF_EXTENDED: u32 = 0x01;

const VK_CONTROL: i32 = 0x11;
const VK_MENU: i32 = 0x12;
const VK_PAUSE: u32 = 0x13;

#[derive(Debug, Clone)]
pub enum KeyboardHookEvent {
    Key {
        vk: u16,
        scan_code: u16,
        down: bool,
        extended: bool,
    },
    Failsafe,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct KbdllHookStruct {
    vk_code: u32,
    scan_code: u32,
    flags: u32,
    time: u32,
    dw_extra_info: usize,
}

static EVENT_SENDER: OnceLock<Mutex<Option<Sender<KeyboardHookEvent>>>> = OnceLock::new();

pub struct KeyboardHookHandle {
    stop_tx: Option<mpsc::Sender<()>>,
    join_handle: Option<thread::JoinHandle<()>>,
}

impl Drop for KeyboardHookHandle {
    fn drop(&mut self) {
        if let Some(stop_tx) = self.stop_tx.take() {
            let _ = stop_tx.send(());
        }

        if let Some(join_handle) = self.join_handle.take() {
            let _ = join_handle.join();
        }
    }
}

pub fn start_keyboard_hook(
    event_tx: Sender<KeyboardHookEvent>,
) -> Result<KeyboardHookHandle, String> {
    let sender_slot = EVENT_SENDER.get_or_init(|| Mutex::new(None));

    {
        let mut guard = sender_slot
            .lock()
            .map_err(|_| "keyboard hook sender mutex poisoned".to_string())?;

        if guard.is_some() {
            return Err("keyboard hook is already running".to_string());
        }

        *guard = Some(event_tx);
    }

    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();

    let join_handle = thread::spawn(move || {
        let hook = unsafe {
            SetWindowsHookExW(WH_KEYBOARD_LL, Some(low_level_keyboard_proc), null_mut(), 0)
        };

        if hook.is_null() {
            let _ = ready_tx.send(Err("SetWindowsHookExW(WH_KEYBOARD_LL) failed".to_string()));
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
            while unsafe { PeekMessageW(&mut msg, null_mut(), 0, 0, PM_REMOVE) } != 0 {
                unsafe {
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }

            if stop_rx.try_recv().is_ok() {
                break;
            }

            thread::sleep(Duration::from_millis(5));
        }

        unsafe {
            UnhookWindowsHookEx(hook);
        }

        clear_sender();
    });

    match ready_rx.recv_timeout(Duration::from_secs(2)) {
        Ok(Ok(())) => Ok(KeyboardHookHandle {
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
            Err(format!("keyboard hook did not become ready: {err}"))
        }
    }
}

unsafe extern "system" fn low_level_keyboard_proc(
    n_code: i32,
    w_param: WPARAM,
    l_param: LPARAM,
) -> isize {
    if n_code < 0 {
        return CallNextHookEx(null_mut(), n_code, w_param, l_param);
    }

    let info = &*(l_param as *const KbdllHookStruct);

    let message = w_param as u32;
    let down = matches!(message, WM_KEYDOWN | WM_SYSKEYDOWN);
    let up = matches!(message, WM_KEYUP | WM_SYSKEYUP);

    if !down && !up {
        return CallNextHookEx(null_mut(), n_code, w_param, l_param);
    }

    if down && info.vk_code == VK_PAUSE && is_key_down(VK_CONTROL) && is_key_down(VK_MENU) {
        let _ = send_event(KeyboardHookEvent::Failsafe);
        return CallNextHookEx(null_mut(), n_code, w_param, l_param);
    }

    let event = KeyboardHookEvent::Key {
        vk: info.vk_code as u16,
        scan_code: info.scan_code as u16,
        down,
        extended: (info.flags & LLKHF_EXTENDED) != 0,
    };

    if send_event(event) {
        // Suppress local keyboard input while hook capture is active.
        return 1;
    }

    CallNextHookEx(null_mut(), n_code, w_param, l_param)
}

fn is_key_down(vk: i32) -> bool {
    let state = unsafe { GetAsyncKeyState(vk) };
    (state as u16 & 0x8000) != 0
}

fn send_event(event: KeyboardHookEvent) -> bool {
    let Some(slot) = EVENT_SENDER.get() else {
        return false;
    };

    let Ok(guard) = slot.lock() else {
        return false;
    };

    let Some(sender) = guard.as_ref() else {
        return false;
    };

    sender.send(event).is_ok()
}

fn clear_sender() {
    if let Some(slot) = EVENT_SENDER.get() {
        if let Ok(mut guard) = slot.lock() {
            *guard = None;
        }
    }
}
