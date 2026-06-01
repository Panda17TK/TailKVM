//! Real-FFI single-machine test for the clipboard module.
//!
//! This exercises the actual Win32 clipboard API on the current desktop, so it
//! mutates the real system clipboard and can fail spuriously if another process
//! holds the clipboard open. It is therefore `#[ignore]`d by default; run it
//! explicitly on a Windows desktop with:
//!
//! ```text
//! cargo test -p tailkvm-win32 --test clipboard_roundtrip -- --ignored
//! ```

use tailkvm_win32::clipboard::{get_clipboard_text, set_clipboard_text};

#[test]
#[ignore = "touches the real Windows clipboard; run with --ignored on a desktop"]
fn clipboard_text_roundtrip() {
    let sample = "tailkvm clipboard test 日本語 🚀 abc123";

    set_clipboard_text(sample).expect("set_clipboard_text should succeed");
    let got = get_clipboard_text().expect("get_clipboard_text should succeed");

    assert_eq!(
        got.as_deref(),
        Some(sample),
        "clipboard round-trip must preserve Unicode text exactly"
    );
}
