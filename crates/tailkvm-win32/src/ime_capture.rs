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
//! The window is a 1x1 popup placed at the caller-provided candidate anchor
//! (IME-POS-001): it must be a *real* activatable window (message-only
//! windows cannot take focus or host an IME), and it must stay ON-SCREEN
//! because the IME anchors its composition/candidate UI to it. The previous
//! foreground window — and the pre-capture IME state — are restored when
//! capture stops (IME-STATE-003/004).
//!
//! NOTE: foreground-activation rules (`SetForegroundWindow`) are best-effort;
//! the `AttachThreadInput` bridge below is the standard workaround, but this
//! path needs on-hardware validation like the other input features.

use std::{
    ptr::null_mut,
    sync::{
        atomic::{AtomicBool, AtomicIsize, Ordering},
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
                ImmGetCompositionStringW, ImmGetContext, ImmGetConversionStatus, ImmGetOpenStatus,
                ImmReleaseContext, ImmSetConversionStatus, ImmSetOpenStatus, GCS_RESULTSTR,
                IME_CMODE_FULLSHAPE, IME_CMODE_NATIVE,
            },
            KeyboardAndMouse::SetFocus,
        },
        WindowsAndMessaging::{
            CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetForegroundWindow,
            GetWindowThreadProcessId, PeekMessageW, RegisterClassW, SetForegroundWindow,
            SetWindowPos, ShowWindow, TranslateMessage, MSG, PM_REMOVE, SWP_NOACTIVATE, SWP_NOSIZE,
            SWP_NOZORDER, SW_SHOW, WNDCLASSW, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
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
/// Whether the current composition's result was already taken via
/// `GCS_RESULTSTR`. Google 日本語入力 compatibility (P2): IMEs that bypass
/// the handled result path deliver commits as `WM_IME_CHAR` instead — those
/// are forwarded only when this flag is still false, so the normal path
/// never double-sends.
static RESULT_HANDLED: AtomicBool = AtomicBool::new(false);
/// HWND of the live capture window (0 = none), for cross-thread
/// repositioning between compositions (IME-POS-021).
static CAPTURE_HWND: AtomicIsize = AtomicIsize::new(0);

/// Whether the local IME currently has an open (uncommitted) composition.
pub fn is_composing() -> bool {
    COMPOSING.load(Ordering::SeqCst)
}

/// Move the live capture window to a new candidate anchor (IME-POS-021).
/// No-op when no capture is active. Callers must never invoke this during an
/// open composition (IME-POS-022) — the forwarding loop guards on
/// [`is_composing`]. `SetWindowPos` is safe cross-thread.
pub fn reposition_capture_window(x: i32, y: i32) {
    let hwnd = CAPTURE_HWND.load(Ordering::SeqCst);
    if hwnd == 0 {
        return;
    }
    unsafe {
        SetWindowPos(
            hwnd as HWND,
            null_mut(),
            x,
            y,
            0,
            0,
            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
}

/// IME open/conversion/sentence state captured from an input context
/// (IME-STATE-001). With the Windows-default session-shared IME mode this is
/// effectively the user's IME state at capture time, so restoring it on exit
/// keeps composition mode from permanently flipping the user's IME.
#[derive(Debug, Clone, Copy)]
pub struct ImeStateSnapshot {
    pub open: bool,
    pub conversion: u32,
    pub sentence: u32,
}

/// What to do with the IME open state when composition mode starts
/// (IME-STATE-010).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImeOpenPolicy {
    /// Turn Japanese input on (default: entering composition mode means the
    /// user wants to type Japanese right away).
    ForceJapanese,
    /// Leave the open state as-is.
    PreserveCurrent,
    /// Reuse the open state the user last had while composing via TailKVM.
    RestoreLastTailkvm,
    /// Leave everything to the user.
    Manual,
}

/// What to do with the IME conversion mode (IME-STATE-020).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImeConversionPolicy {
    /// Enable native Japanese, keep the rest of the mode bits (default).
    NativeDefault,
    /// Enable native + full-shape (legacy-compatible; may pin full-width).
    NativeFullshape,
    /// Keep the current conversion mode untouched.
    Preserve,
    /// Reuse the conversion mode last used while composing via TailKVM.
    LastUsed,
}

/// Behavior when the capture window cannot take focus (IME-ERR-004).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusFailurePolicy {
    /// Abort composition-mode entry.
    Abort,
    /// Continue; the caller surfaces a warning.
    WarnContinue,
    /// Retry the focus grab a few times before continuing (default).
    Retry,
}

/// Options for [`start_ime_capture`]: the candidate anchor position (caller
/// is responsible for clamping it on-screen, see `ime_anchor`) and the IME
/// policies.
#[derive(Debug, Clone, Copy)]
pub struct ImeCaptureOptions {
    pub anchor_x: i32,
    pub anchor_y: i32,
    /// Capture-window edge length in px (IME-POS-004): 1 normally; 2 or 8 as
    /// an escape hatch for IMEs that misbehave with a 1x1 host window.
    pub window_size: i32,
    pub open_policy: ImeOpenPolicy,
    pub conversion_policy: ImeConversionPolicy,
    pub focus_failure_policy: FocusFailurePolicy,
}

impl Default for ImeCaptureOptions {
    fn default() -> Self {
        Self {
            anchor_x: 0,
            anchor_y: 0,
            window_size: 1,
            open_policy: ImeOpenPolicy::ForceJapanese,
            conversion_policy: ImeConversionPolicy::NativeDefault,
            focus_failure_policy: FocusFailurePolicy::Retry,
        }
    }
}

/// IME state the user last had while composing through TailKVM, saved on
/// every capture exit (for the `restore_last_tailkvm` / `last_used` policies).
static LAST_TAILKVM_STATE: Mutex<Option<ImeStateSnapshot>> = Mutex::new(None);

pub struct ImeCaptureHandle {
    stop_tx: Option<mpsc::Sender<()>>,
    join_handle: Option<thread::JoinHandle<()>>,
    focus_acquired: bool,
    state_saved: bool,
}

impl ImeCaptureHandle {
    /// Whether the capture window verifiably became the foreground window
    /// (IME-ERR-003). False means keystrokes may not reach the IME.
    pub fn focus_acquired(&self) -> bool {
        self.focus_acquired
    }

    /// Whether the pre-capture IME state could be snapshotted for restore
    /// (IME-STATE-002).
    pub fn state_saved(&self) -> bool {
        self.state_saved
    }
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
/// dropped, which also restores the pre-capture IME state and hands focus
/// back to the previously foreground window.
pub fn start_ime_capture(
    commit_tx: CommitSender,
    options: ImeCaptureOptions,
) -> Result<ImeCaptureHandle, String> {
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
    RESULT_HANDLED.store(false, Ordering::SeqCst);

    // Captured before the window exists so focus can be handed back on stop.
    let prev_foreground = unsafe { GetForegroundWindow() } as isize;

    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let (ready_tx, ready_rx) = mpsc::channel::<Result<(bool, bool), String>>();

    let join_handle = thread::spawn(move || {
        let hwnd =
            match create_capture_window(options.anchor_x, options.anchor_y, options.window_size) {
                Ok(hwnd) => hwnd,
                Err(err) => {
                    let _ = ready_tx.send(Err(err));
                    clear_sender();
                    return;
                }
            };
        CAPTURE_HWND.store(hwnd as isize, Ordering::SeqCst);

        // Snapshot the IME state BEFORE any policy touches it: with the
        // session-shared IME mode (the Windows default), changing this
        // context changes what the user's other windows see, so the original
        // state must be restored on every exit path (IME-STATE-001/003).
        let snapshot = capture_ime_state(hwnd);
        apply_ime_policies(hwnd, &options, snapshot.as_ref());

        // The IME composes only in the foreground window: verify the grab
        // and retry per policy (IME-ERR-003/004).
        let attempts = match options.focus_failure_policy {
            FocusFailurePolicy::Retry => 5,
            _ => 1,
        };
        let mut focus_acquired = false;
        for _ in 0..attempts {
            grab_focus(hwnd, prev_foreground as HWND);
            thread::sleep(Duration::from_millis(30));
            if unsafe { GetForegroundWindow() } == hwnd {
                focus_acquired = true;
                break;
            }
        }

        if !focus_acquired && options.focus_failure_policy == FocusFailurePolicy::Abort {
            if let Some(snapshot) = snapshot.as_ref() {
                restore_ime_state(hwnd, snapshot);
            }
            CAPTURE_HWND.store(0, Ordering::SeqCst);
            unsafe {
                DestroyWindow(hwnd);
                let prev = prev_foreground as HWND;
                if !prev.is_null() {
                    SetForegroundWindow(prev);
                }
            }
            clear_sender();
            let _ = ready_tx.send(Err(
                "ime capture window could not take focus (policy: abort)".to_string(),
            ));
            return;
        }

        let _ = ready_tx.send(Ok((focus_acquired, snapshot.is_some())));

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

        // Remember the state the user left the IME in (for the
        // restore_last_tailkvm / last_used policies), then restore the
        // pre-capture snapshot. The handle drop joins this thread, so this
        // runs on EVERY exit path: toggle-off, forwarding stop, failsafe,
        // link lost, thread/Drop teardown (IME-STATE-004).
        if let Some(current) = capture_ime_state(hwnd) {
            if let Ok(mut last) = LAST_TAILKVM_STATE.lock() {
                *last = Some(current);
            }
        }
        if let Some(snapshot) = snapshot.as_ref() {
            restore_ime_state(hwnd, snapshot);
        }

        CAPTURE_HWND.store(0, Ordering::SeqCst);
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
        Ok(Ok((focus_acquired, state_saved))) => Ok(ImeCaptureHandle {
            stop_tx: Some(stop_tx),
            join_handle: Some(join_handle),
            focus_acquired,
            state_saved,
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

fn create_capture_window(x: i32, y: i32, size: i32) -> Result<HWND, String> {
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

    // A real (activatable) 1x1 popup at the candidate anchor: effectively
    // invisible, but able to take focus and host the thread's default IME
    // context, unlike a message-only window. It must stay ON-SCREEN — the
    // IME anchors its composition/candidate UI to this window, and an
    // off-screen anchor would hide the conversion candidates from the user
    // (the caller clamps the anchor into a visible monitor, IME-POS-003).
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
            class_name.as_ptr(),
            class_name.as_ptr(),
            WS_POPUP,
            x,
            y,
            size.clamp(1, 16),
            size.clamp(1, 16),
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

/// Read the current IME open/conversion/sentence state (IME-STATE-001).
fn capture_ime_state(hwnd: HWND) -> Option<ImeStateSnapshot> {
    unsafe {
        let himc = ImmGetContext(hwnd);
        if himc.is_null() {
            return None;
        }
        let open = ImmGetOpenStatus(himc) != 0;
        let mut conversion = 0u32;
        let mut sentence = 0u32;
        let got = ImmGetConversionStatus(himc, &mut conversion, &mut sentence) != 0;
        ImmReleaseContext(hwnd, himc);
        got.then_some(ImeStateSnapshot {
            open,
            conversion,
            sentence,
        })
    }
}

/// Restore a previously captured IME state. Best-effort: failures are never
/// fatal (IME-ERR-005) — IMEs vary in which mode bits they honor.
fn restore_ime_state(hwnd: HWND, snapshot: &ImeStateSnapshot) {
    unsafe {
        let himc = ImmGetContext(hwnd);
        if himc.is_null() {
            return;
        }
        ImmSetConversionStatus(himc, snapshot.conversion, snapshot.sentence);
        ImmSetOpenStatus(himc, if snapshot.open { 1 } else { 0 });
        ImmReleaseContext(hwnd, himc);
    }
}

/// Apply the configured open/conversion policies to the capture window's IME
/// context. The toggle keystroke that *entered* composition mode was
/// suppressed by the hook (pass-through turns on only after the toggle), so
/// it never reached this window — without an explicit policy the fresh
/// context can stay alphanumeric and romaji would pass through as plain
/// ASCII. Failures here are warnings, never fatal (IME-STATE-022).
fn apply_ime_policies(hwnd: HWND, options: &ImeCaptureOptions, current: Option<&ImeStateSnapshot>) {
    let last = LAST_TAILKVM_STATE.lock().ok().and_then(|guard| *guard);
    unsafe {
        let himc = ImmGetContext(hwnd);
        if himc.is_null() {
            return;
        }

        match options.open_policy {
            ImeOpenPolicy::ForceJapanese => {
                ImmSetOpenStatus(himc, 1);
            }
            ImeOpenPolicy::PreserveCurrent | ImeOpenPolicy::Manual => {}
            ImeOpenPolicy::RestoreLastTailkvm => {
                // No previous TailKVM state yet: behave like force_japanese
                // so first-time composition still works.
                let open = last.map(|state| state.open).unwrap_or(true);
                ImmSetOpenStatus(himc, if open { 1 } else { 0 });
            }
        }

        let (conversion, sentence) = match current {
            Some(snapshot) => (snapshot.conversion, snapshot.sentence),
            None => (0, 0),
        };
        match options.conversion_policy {
            ImeConversionPolicy::NativeDefault => {
                ImmSetConversionStatus(himc, conversion | IME_CMODE_NATIVE, sentence);
            }
            ImeConversionPolicy::NativeFullshape => {
                ImmSetConversionStatus(
                    himc,
                    conversion | IME_CMODE_NATIVE | IME_CMODE_FULLSHAPE,
                    sentence,
                );
            }
            ImeConversionPolicy::Preserve => {}
            ImeConversionPolicy::LastUsed => match last {
                Some(last) => {
                    ImmSetConversionStatus(himc, last.conversion, last.sentence);
                }
                None => {
                    ImmSetConversionStatus(himc, conversion | IME_CMODE_NATIVE, sentence);
                }
            },
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
            RESULT_HANDLED.store(false, Ordering::SeqCst);
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
        // Google 日本語入力 compatibility (P2): an IME that bypasses the
        // GCS_RESULTSTR path above delivers its commit as WM_IME_CHAR
        // instead — forward those as text. When the result WAS already taken
        // via GCS_RESULTSTR, any IME chars are duplicates: swallow them.
        WM_IME_CHAR => {
            if !RESULT_HANDLED.load(Ordering::SeqCst) {
                forward_char_unit(w_param as u16);
            }
            0
        }
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
                RESULT_HANDLED.store(true, Ordering::SeqCst);
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
