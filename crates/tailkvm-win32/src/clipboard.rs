//! Minimal Win32 clipboard text access (`CF_UNICODETEXT`) plus a pure
//! echo-loop guard for future clipboard sync.
//!
//! Only Unicode text is handled here. Image/file (`CF_BITMAP`, `CF_HDROP`,
//! `CF_DIB`) sharing is intentionally out of scope for this foundation — see
//! TASK_LOG Task 11 for the staged design.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::ptr::null_mut;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard,
    SetClipboardData,
};
use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};

/// `CF_UNICODETEXT` standard clipboard format identifier.
const CF_UNICODETEXT: u32 = 13;

/// Stable 64-bit hash of clipboard content, used to recognise our own echo.
pub fn content_hash(text: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

/// Prevents infinite clipboard sync echo loops.
///
/// When a clipboard sync feature watches the local clipboard and forwards
/// changes to a peer, applying the peer's content locally would itself trigger
/// the watcher and bounce the same text back — an infinite loop. This guard
/// records a hash of the last content we set or applied; a subsequent local
/// observation equal to that hash is treated as our own echo and suppressed.
#[derive(Debug, Default)]
pub struct ClipboardLoopGuard {
    last_hash: Option<u64>,
}

impl ClipboardLoopGuard {
    pub fn new() -> Self {
        Self { last_hash: None }
    }

    /// Returns `true` if `text` is new and should be broadcast to the peer,
    /// `false` if it matches the last content we set/applied (an echo).
    /// Updates the remembered hash to `text` either way.
    pub fn should_broadcast(&mut self, text: &str) -> bool {
        let hash = content_hash(text);
        if self.last_hash == Some(hash) {
            false
        } else {
            self.last_hash = Some(hash);
            true
        }
    }

    /// Record content we just applied locally (or sent), so the next
    /// observation of that same content is recognised as an echo.
    pub fn mark_applied(&mut self, text: &str) {
        self.last_hash = Some(content_hash(text));
    }
}

/// RAII guard: closes the clipboard on drop so no early-return path leaks it.
struct ClipboardSession;

impl ClipboardSession {
    fn open() -> Result<Self, String> {
        // A null owner associates the clipboard with the current task.
        let ok = unsafe { OpenClipboard(null_mut()) };
        if ok == 0 {
            Err("OpenClipboard failed".to_string())
        } else {
            Ok(ClipboardSession)
        }
    }
}

impl Drop for ClipboardSession {
    fn drop(&mut self) {
        unsafe {
            CloseClipboard();
        }
    }
}

/// Read the clipboard as UTF-16 text. Returns `Ok(None)` when the clipboard
/// holds no text in `CF_UNICODETEXT` form.
pub fn get_clipboard_text() -> Result<Option<String>, String> {
    let _session = ClipboardSession::open()?;

    if unsafe { IsClipboardFormatAvailable(CF_UNICODETEXT) } == 0 {
        return Ok(None);
    }

    let handle = unsafe { GetClipboardData(CF_UNICODETEXT) };
    if handle.is_null() {
        return Err("GetClipboardData(CF_UNICODETEXT) returned null".to_string());
    }

    let ptr = unsafe { GlobalLock(handle) } as *const u16;
    if ptr.is_null() {
        return Err("GlobalLock failed for clipboard data".to_string());
    }

    // Walk the wide string up to its null terminator.
    let mut len = 0usize;
    unsafe {
        while *ptr.add(len) != 0 {
            len += 1;
        }
    }

    let slice = unsafe { std::slice::from_raw_parts(ptr, len) };
    let text = String::from_utf16_lossy(slice);

    unsafe {
        GlobalUnlock(handle);
    }

    Ok(Some(text))
}

/// Replace the clipboard contents with `text` as `CF_UNICODETEXT`.
pub fn set_clipboard_text(text: &str) -> Result<(), String> {
    let mut utf16: Vec<u16> = text.encode_utf16().collect();
    utf16.push(0); // null terminator
    let byte_len = utf16.len() * std::mem::size_of::<u16>();

    let _session = ClipboardSession::open()?;

    unsafe {
        EmptyClipboard();
    }

    let hglobal = unsafe { GlobalAlloc(GMEM_MOVEABLE, byte_len) };
    if hglobal.is_null() {
        return Err("GlobalAlloc failed for clipboard data".to_string());
    }

    let dst = unsafe { GlobalLock(hglobal) } as *mut u16;
    if dst.is_null() {
        return Err("GlobalLock failed for clipboard destination".to_string());
    }

    unsafe {
        std::ptr::copy_nonoverlapping(utf16.as_ptr(), dst, utf16.len());
        GlobalUnlock(hglobal);
    }

    // On success the system takes ownership of hglobal (must not be freed).
    let result = unsafe { SetClipboardData(CF_UNICODETEXT, hglobal as HANDLE) };
    if result.is_null() {
        return Err("SetClipboardData failed".to_string());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_hash_is_stable_and_distinguishes() {
        assert_eq!(content_hash("hello"), content_hash("hello"));
        assert_ne!(content_hash("hello"), content_hash("world"));
        assert_ne!(content_hash(""), content_hash(" "));
    }

    #[test]
    fn fresh_content_broadcasts_then_duplicate_is_suppressed() {
        let mut guard = ClipboardLoopGuard::new();
        assert!(guard.should_broadcast("alpha"), "first sight is new");
        assert!(
            !guard.should_broadcast("alpha"),
            "immediate repeat is an echo"
        );
        assert!(guard.should_broadcast("beta"), "different content is new");
        assert!(!guard.should_broadcast("beta"));
    }

    #[test]
    fn mark_applied_suppresses_matching_observation() {
        let mut guard = ClipboardLoopGuard::new();
        // Simulate applying a peer's content: the local watcher must not echo it.
        guard.mark_applied("from-peer");
        assert!(
            !guard.should_broadcast("from-peer"),
            "applied content must not be rebroadcast"
        );
        // A genuinely new local copy still broadcasts.
        assert!(guard.should_broadcast("local-copy"));
    }
}
