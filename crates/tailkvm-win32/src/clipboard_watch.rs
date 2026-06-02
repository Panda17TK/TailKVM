//! Clipboard-change watcher (roadmap D1): a message-only window subscribed via
//! `AddClipboardFormatListener` that emits a signal on every `WM_CLIPBOARDUPDATE`.
//! The consumer reads the clipboard text and decides (via an echo guard) whether
//! to forward it, enabling automatic clipboard sync.

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
    Foundation::{HWND, LPARAM, LRESULT, WPARAM},
    System::{
        DataExchange::{AddClipboardFormatListener, RemoveClipboardFormatListener},
        LibraryLoader::GetModuleHandleW,
    },
    UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, PeekMessageW,
        RegisterClassW, TranslateMessage, MSG, PM_REMOVE, WNDCLASSW,
    },
};

const WM_CLIPBOARDUPDATE: u32 = 0x031D;
const HWND_MESSAGE: HWND = -3isize as HWND;

type ChangeSender = Sender<()>;

static CHANGE_SENDER: OnceLock<Mutex<Option<ChangeSender>>> = OnceLock::new();

pub struct ClipboardWatchHandle {
    stop_tx: Option<mpsc::Sender<()>>,
    join_handle: Option<thread::JoinHandle<()>>,
}

impl Drop for ClipboardWatchHandle {
    fn drop(&mut self) {
        if let Some(stop_tx) = self.stop_tx.take() {
            let _ = stop_tx.send(());
        }
        if let Some(join_handle) = self.join_handle.take() {
            let _ = join_handle.join();
        }
    }
}

/// Start watching the clipboard. A `()` is sent on `change_tx` for every
/// clipboard update until the returned handle is dropped.
pub fn start_clipboard_watch(change_tx: Sender<()>) -> Result<ClipboardWatchHandle, String> {
    let sender_slot = CHANGE_SENDER.get_or_init(|| Mutex::new(None));

    {
        let mut guard = sender_slot
            .lock()
            .map_err(|_| "clipboard watch sender mutex poisoned".to_string())?;
        if guard.is_some() {
            return Err("clipboard watch is already running".to_string());
        }
        *guard = Some(change_tx);
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

        if unsafe { AddClipboardFormatListener(hwnd) } == 0 {
            let _ = ready_tx.send(Err("AddClipboardFormatListener failed".to_string()));
            unsafe {
                DestroyWindow(hwnd);
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

            thread::sleep(Duration::from_millis(20));
        }

        unsafe {
            RemoveClipboardFormatListener(hwnd);
            DestroyWindow(hwnd);
        }
        clear_sender();
    });

    match ready_rx.recv_timeout(Duration::from_secs(2)) {
        Ok(Ok(())) => Ok(ClipboardWatchHandle {
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
            Err(format!("clipboard watch did not become ready: {err}"))
        }
    }
}

fn create_message_window() -> Result<HWND, String> {
    let class_name: Vec<u16> = "TailKVMClipboardWatchWindow\0".encode_utf16().collect();
    let hinstance = unsafe { GetModuleHandleW(null_mut()) };

    let wnd_class = WNDCLASSW {
        style: 0,
        lpfnWndProc: Some(clipboard_wnd_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: hinstance,
        hIcon: null_mut(),
        hCursor: null_mut(),
        hbrBackground: null_mut(),
        lpszMenuName: null_mut(),
        lpszClassName: class_name.as_ptr(),
    };

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
        Err("CreateWindowExW (clipboard watch) failed".to_string())
    } else {
        Ok(hwnd)
    }
}

unsafe extern "system" fn clipboard_wnd_proc(
    hwnd: HWND,
    msg: u32,
    w_param: WPARAM,
    l_param: LPARAM,
) -> LRESULT {
    if msg == WM_CLIPBOARDUPDATE {
        send_change();
    }
    DefWindowProcW(hwnd, msg, w_param, l_param)
}

fn send_change() {
    let Some(slot) = CHANGE_SENDER.get() else {
        return;
    };
    let Ok(guard) = slot.lock() else {
        return;
    };
    if let Some(sender) = guard.as_ref() {
        let _ = sender.send(());
    }
}

fn clear_sender() {
    if let Some(slot) = CHANGE_SENDER.get() {
        if let Ok(mut guard) = slot.lock() {
            *guard = None;
        }
    }
}
