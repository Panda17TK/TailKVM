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
    UI::WindowsAndMessaging::{
        CallNextHookEx, DispatchMessageW, PeekMessageW, SetWindowsHookExW, TranslateMessage,
        UnhookWindowsHookEx, MSG, PM_REMOVE, WH_MOUSE_LL, WM_LBUTTONDOWN, WM_LBUTTONUP,
        WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEWHEEL, WM_RBUTTONDOWN, WM_RBUTTONUP,
    },
};

#[derive(Debug, Clone)]
pub enum MouseHookEvent {
    Button { button: String, down: bool },
    Wheel { delta: i32, horizontal: bool },
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct Point {
    x: i32,
    y: i32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct MsllHookStruct {
    pt: Point,
    mouse_data: u32,
    flags: u32,
    time: u32,
    dw_extra_info: usize,
}

static EVENT_SENDER: OnceLock<Mutex<Option<Sender<MouseHookEvent>>>> = OnceLock::new();

pub struct MouseHookHandle {
    stop_tx: Option<mpsc::Sender<()>>,
    join_handle: Option<thread::JoinHandle<()>>,
}

impl Drop for MouseHookHandle {
    fn drop(&mut self) {
        if let Some(stop_tx) = self.stop_tx.take() {
            let _ = stop_tx.send(());
        }

        if let Some(join_handle) = self.join_handle.take() {
            let _ = join_handle.join();
        }
    }
}

pub fn start_mouse_hook(event_tx: Sender<MouseHookEvent>) -> Result<MouseHookHandle, String> {
    let sender_slot = EVENT_SENDER.get_or_init(|| Mutex::new(None));

    {
        let mut guard = sender_slot
            .lock()
            .map_err(|_| "mouse hook sender mutex poisoned".to_string())?;

        if guard.is_some() {
            return Err("mouse hook is already running".to_string());
        }

        *guard = Some(event_tx);
    }

    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();

    let join_handle = thread::spawn(move || {
        let hook =
            unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(low_level_mouse_proc), null_mut(), 0) };

        if hook.is_null() {
            let _ = ready_tx.send(Err("SetWindowsHookExW(WH_MOUSE_LL) failed".to_string()));
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
        Ok(Ok(())) => Ok(MouseHookHandle {
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
            Err(format!("mouse hook did not become ready: {err}"))
        }
    }
}

unsafe extern "system" fn low_level_mouse_proc(
    n_code: i32,
    w_param: WPARAM,
    l_param: LPARAM,
) -> isize {
    if n_code < 0 {
        return CallNextHookEx(null_mut(), n_code, w_param, l_param);
    }

    let event = match w_param as u32 {
        WM_LBUTTONDOWN => Some(MouseHookEvent::Button {
            button: "left".to_string(),
            down: true,
        }),
        WM_LBUTTONUP => Some(MouseHookEvent::Button {
            button: "left".to_string(),
            down: false,
        }),
        WM_RBUTTONDOWN => Some(MouseHookEvent::Button {
            button: "right".to_string(),
            down: true,
        }),
        WM_RBUTTONUP => Some(MouseHookEvent::Button {
            button: "right".to_string(),
            down: false,
        }),
        WM_MBUTTONDOWN => Some(MouseHookEvent::Button {
            button: "middle".to_string(),
            down: true,
        }),
        WM_MBUTTONUP => Some(MouseHookEvent::Button {
            button: "middle".to_string(),
            down: false,
        }),
        WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
            let info = &*(l_param as *const MsllHookStruct);
            let delta = ((info.mouse_data >> 16) & 0xffff) as i16 as i32;

            Some(MouseHookEvent::Wheel {
                delta,
                horizontal: w_param as u32 == WM_MOUSEHWHEEL,
            })
        }
        _ => None,
    };

    if let Some(event) = event {
        if send_event(event) {
            // Suppress local click/wheel while this hook is active.
            return 1;
        }
    }

    CallNextHookEx(null_mut(), n_code, w_param, l_param)
}

fn send_event(event: MouseHookEvent) -> bool {
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
