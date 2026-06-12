//! IME composition capture (issue #10): a tiny focusable window that hosts the
//! controller's IME while a remote is being controlled.
//!
//! Per-key Unicode resolution (`keyboard::resolve_key_text`) is IME-*off* by
//! design — it cannot represent kana→kanji composition. In composition mode
//! the keyboard hook switches to pass-through, keystrokes flow into this
//! window where the real local IME composes, and only the *committed* result
//! string (`WM_IME_COMPOSITION` / `GCS_RESULTSTR`) is emitted, to be forwarded
//! to the peer as a layout-independent `KeyboardText`. Non-IME characters
//! typed while the mode is active arrive via `WM_CHAR` and are forwarded the
//! same way.
//!
//! The window is a 1x1 popup parked far off-screen: it must be a *real*
//! activatable window (message-only windows cannot take focus or host an IME).
//! The previous foreground window is restored when capture stops.
//!
//! NOTE: foreground-activation rules (`SetForegroundWindow`) are best-effort;
//! the `AttachThreadInput` bridge below is the standard workaround, but this
//! path needs on-hardware validation like the other input features.

use std::{
    ptr::null_mut,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Sender},
        Mutex, OnceLock,
    },
    thread,
    time::Duration,
};
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, WPARAM},
    System::{
        LibraryLoader::GetModuleHandleW,
        Threading::{AttachThreadInput, GetCurrentThreadId},
    },
    UI::{
        Input::{
            Ime::{
                ImmGetCompositionStringW, ImmGetContext, ImmGetConversionStatus, ImmReleaseContext,
                ImmSetConversionStatus, ImmSetOpenStatus, GCS_RESULTSTR, IME_CMODE_FULLSHAPE,
                IME_CMODE_NATIVE,
            },
            KeyboardAndMouse::SetFocus,
        },
        WindowsAndMessaging::{
            CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetForegroundWindow,
            GetWindowThreadProcessId, PeekMessageW, RegisterClassW, SetForegroundWindow,
            ShowWindow, TranslateMessage, MSG, PM_REMOVE, SW_SHOW, WNDCLASSW, WS_EX_TOOLWINDOW,
            WS_EX_TOPMOST, WS_POPUP,
        },
    },
};

const WM_CHAR: u32 = 0x0102;
const WM_IME_CHAR: u32 = 0x0286;
const WM_IME_STARTCOMPOSITION: u32 = 0x010D;
const WM_IME_ENDCOMPOSITION: u32 = 0x010E;
const WM_IME_COMPOSITION: u32 = 0x010F;

type CommitSender = Sender<String>;

static COMMIT_SENDER: OnceLock<Mutex<Option<CommitSender>>> = OnceLock::new();
/// Pending high surrogate from `WM_CHAR` (astral-plane characters arrive as
/// two messages).
static PENDING_HIGH_SURROGATE: Mutex<Option<u16>> = Mutex::new(None);
/// True while the IME has an open composition in the capture window
/// (`WM_IME_STARTCOMPOSITION`..`WM_IME_ENDCOMPOSITION`). The forwarding loop
/// reads this to forward non-composing control keys (Enter, Backspace, arrows)
/// physically instead of dropping them while composition mode is on.
static COMPOSING: AtomicBool = AtomicBool::new(false);

/// Whether the local IME currently has an open (uncommitted) composition.
pub fn is_composing() -> bool {
    COMPOSING.load(Ordering::SeqCst)
}

pub struct ImeCaptureHandle {
    stop_tx: Option<mpsc::Sender<()>>,
    join_handle: Option<thread::JoinHandle<()>>,
}

impl Drop for ImeCaptureHandle {
    fn drop(&mut self) {
        if let Some(stop_tx) = self.stop_tx.take() {
            let _ = stop_tx.send(());
        }
        if let Some(join_handle) = self.join_handle.take() {
            let _ = join_handle.join();
        }
    }
}

/// Start IME composition capture. Committed composition strings (and plain
/// `WM_CHAR` text) are sent on `commit_tx` until the returned handle is
/// dropped, which also restores focus to the previously foreground window.
pub fn start_ime_capture(commit_tx: CommitSender) -> Result<ImeCaptureHandle, String> {
    let sender_slot = COMMIT_SENDER.get_or_init(|| Mutex::new(None));

    {
        let mut guard = sender_slot
            .lock()
            .map_err(|_| "ime capture sender mutex poisoned".to_string())?;
        if guard.is_some() {
            return Err("ime capture is already running".to_string());
        }
        *guard = Some(commit_tx);
    }

    if let Ok(mut pending) = PENDING_HIGH_SURROGATE.lock() {
        *pending = None;
    }
    COMPOSING.store(false, Ordering::SeqCst);

    // Captured before the window exists so focus can be handed back on stop.
    let prev_foreground = unsafe { GetForegroundWindow() } as isize;

    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();

    let join_handle = thread::spawn(move || {
        let hwnd = match create_capture_window() {
            Ok(hwnd) => hwnd,
            Err(err) => {
                let _ = ready_tx.send(Err(err));
                clear_sender();
                return;
            }
        };

        enable_ime(hwnd);
        grab_focus(hwnd, prev_foreground as HWND);
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
            // Pump ALL windows on this thread (null filter), not just the
            // capture window: the IME's own composition/candidate UI windows
            // live on this thread and starve without their messages.
            while unsafe { PeekMessageW(&mut msg, null_mut(), 0, 0, PM_REMOVE) } != 0 {
                unsafe {
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }

            if stop_rx.try_recv().is_ok() {
                break;
            }

            thread::sleep(Duration::from_millis(10));
        }

        unsafe {
            DestroyWindow(hwnd);
            // Hand focus back to wherever the user was before composing.
            let prev = prev_foreground as HWND;
            if !prev.is_null() {
                SetForegroundWindow(prev);
            }
        }
        clear_sender();
    });

    match ready_rx.recv_timeout(Duration::from_secs(2)) {
        Ok(Ok(())) => Ok(ImeCaptureHandle {
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
            Err(format!("ime capture did not become ready: {err}"))
        }
    }
}

fn create_capture_window() -> Result<HWND, String> {
    let class_name: Vec<u16> = "TailKVMImeCaptureWindow\0".encode_utf16().collect();
    let hinstance = unsafe { GetModuleHandleW(null_mut()) };

    let wnd_class = WNDCLASSW {
        style: 0,
        lpfnWndProc: Some(ime_capture_wnd_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: hinstance,
        hIcon: null_mut(),
        hCursor: null_mut(),
        hbrBackground: null_mut(),
        lpszMenuName: null_mut(),
        lpszClassName: class_name.as_ptr(),
    };

    // Ignore the result: a previous start may have registered the class.
    unsafe {
        RegisterClassW(&wnd_class);
    }

    // A real (activatable) 1x1 popup at the primary monitor's origin:
    // effectively invisible, but able to take focus and host the thread's
    // default IME context, unlike a message-only window. It must stay
    // ON-SCREEN — the IME anchors its composition/candidate UI to this
    // window, and an off-screen anchor would hide the conversion candidates
    // from the user.
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
            class_name.as_ptr(),
            class_name.as_ptr(),
            WS_POPUP,
            0,
            0,
            1,
            1,
            null_mut(),
            null_mut(),
            hinstance,
            null_mut(),
        )
    };

    if hwnd.is_null() {
        Err("CreateWindowExW (ime capture) failed".to_string())
    } else {
        unsafe {
            ShowWindow(hwnd, SW_SHOW);
        }
        Ok(hwnd)
    }
}

/// Explicitly open the IME on the capture window in native (kana) full-shape
/// mode. The toggle keystroke that *entered* composition mode was suppressed
/// by the hook (pass-through turns on only after the toggle), so it never
/// reached this window — without this the fresh thread's IME context can stay
/// in alphanumeric mode and romaji would pass through as plain ASCII.
fn enable_ime(hwnd: HWND) {
    unsafe {
        let himc = ImmGetContext(hwnd);
        if himc.is_null() {
            return;
        }
        ImmSetOpenStatus(himc, 1);
        let mut conversion = 0u32;
        let mut sentence = 0u32;
        if ImmGetConversionStatus(himc, &mut conversion, &mut sentence) != 0 {
            ImmSetConversionStatus(
                himc,
                conversion | IME_CMODE_NATIVE | IME_CMODE_FULLSHAPE,
                sentence,
            );
        }
        ImmReleaseContext(hwnd, himc);
    }
}

/// Best-effort focus steal: bridge our thread input queue with the current
/// foreground window's so `SetForegroundWindow` is permitted (the OS otherwise
/// blocks background processes from stealing foreground).
fn grab_focus(hwnd: HWND, prev_foreground: HWND) {
    unsafe {
        let my_thread = GetCurrentThreadId();
        let fg_thread = if prev_foreground.is_null() {
            0
        } else {
            GetWindowThreadProcessId(prev_foreground, null_mut())
        };

        let attached = fg_thread != 0
            && fg_thread != my_thread
            && AttachThreadInput(my_thread, fg_thread, 1) != 0;

        SetForegroundWindow(hwnd);
        SetFocus(hwnd);

        if attached {
            AttachThreadInput(my_thread, fg_thread, 0);
        }
    }
}

unsafe extern "system" fn ime_capture_wnd_proc(
    hwnd: HWND,
    msg: u32,
    w_param: WPARAM,
    l_param: LPARAM,
) -> LRESULT {
    match msg {
        // Track composition state for the forwarding loop. Fall through to
        // DefWindowProc so the IME still creates its default composition UI.
        WM_IME_STARTCOMPOSITION => {
            COMPOSING.store(true, Ordering::SeqCst);
            DefWindowProcW(hwnd, msg, w_param, l_param)
        }
        WM_IME_ENDCOMPOSITION => {
            COMPOSING.store(false, Ordering::SeqCst);
            DefWindowProcW(hwnd, msg, w_param, l_param)
        }
        WM_IME_COMPOSITION if (l_param as u32) & GCS_RESULTSTR != 0 => {
            forward_composition_result(hwnd);
            // Skip DefWindowProc for this message: it would re-deliver the
            // result string as WM_IME_CHAR messages and double the text.
            0
        }
        // Defensive: swallow any result chars that still arrive this way.
        WM_IME_CHAR => 0,
        WM_CHAR => {
            forward_char_unit(w_param as u16);
            0
        }
        _ => DefWindowProcW(hwnd, msg, w_param, l_param),
    }
}

/// Read the committed composition string (`GCS_RESULTSTR`) and emit it.
unsafe fn forward_composition_result(hwnd: HWND) {
    let himc = ImmGetContext(hwnd);
    if himc.is_null() {
        return;
    }

    // First call returns the byte length of the UTF-16 result string.
    let byte_len = ImmGetCompositionStringW(himc, GCS_RESULTSTR, null_mut(), 0);
    if byte_len > 0 {
        let unit_len = (byte_len as usize) / std::mem::size_of::<u16>();
        let mut buf = vec![0u16; unit_len];
        let copied = ImmGetCompositionStringW(
            himc,
            GCS_RESULTSTR,
            buf.as_mut_ptr() as *mut core::ffi::c_void,
            byte_len as u32,
        );
        if copied > 0 {
            let copied_units = (copied as usize) / std::mem::size_of::<u16>();
            let text = String::from_utf16_lossy(&buf[..copied_units.min(buf.len())]);
            if !text.is_empty() {
                send_commit(text);
            }
        }
    }

    ImmReleaseContext(hwnd, himc);
}

/// Forward a `WM_CHAR` UTF-16 unit, pairing surrogates across messages.
fn forward_char_unit(unit: u16) {
    // Control characters (backspace, enter, …) are not text commits; while
    // composing the IME consumes them itself, and outside composition the
    // physical-key path handles them.
    if unit < 0x20 {
        return;
    }

    let text = {
        let Ok(mut pending) = PENDING_HIGH_SURROGATE.lock() else {
            return;
        };
        match (*pending, unit) {
            (None, 0xD800..=0xDBFF) => {
                *pending = Some(unit);
                return;
            }
            (Some(high), 0xDC00..=0xDFFF) => {
                *pending = None;
                String::from_utf16_lossy(&[high, unit])
            }
            (Some(_), _) => {
                // Orphaned high surrogate: drop it, treat this unit normally.
                *pending = None;
                String::from_utf16_lossy(&[unit])
            }
            (None, _) => String::from_utf16_lossy(&[unit]),
        }
    };

    send_commit(text);
}

fn send_commit(text: String) {
    let Some(slot) = COMMIT_SENDER.get() else {
        return;
    };
    let Ok(guard) = slot.lock() else {
        return;
    };
    if let Some(sender) = guard.as_ref() {
        let _ = sender.send(text);
    }
}

fn clear_sender() {
    if let Some(slot) = COMMIT_SENDER.get() {
        if let Ok(mut guard) = slot.lock() {
            *guard = None;
        }
    }
}
