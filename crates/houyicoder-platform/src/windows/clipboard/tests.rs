//! Tests for this module.

use std::slice::from_raw_parts;

use windows::Win32::Foundation::HGLOBAL;
use windows::Win32::System::DataExchange::{CloseClipboard, GetClipboardData, OpenClipboard};
use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};

use super::CF_UNICODETEXT;

/// The clipboard format constant for Unicode text is CF_UNICODETEXT, which
/// the system defines as 13. A wrong constant would set the text in a
/// different format and the reader would find nothing.
#[test]
fn test_unicodetext_format_value() {
    assert_eq!(CF_UNICODETEXT, 13);
}

/// The retry window is bounded and the pause is shorter than the window, so
/// a contended clipboard gets several attempts without turning the write
/// into a long wait.
#[test]
fn test_retry_window_bounded() {
    assert!(super::OPEN_RETRY_PAUSE < super::OPEN_RETRY_SPAN);
}

/// Write a Unicode text to the real system clipboard and read it back
/// through the same Win32 API, comparing the exact wide text. Chinese,
/// emoji, and multi-line text all round-trip, which is the garbling the
/// API replaced clip.exe to fix. Ignored in the daily gate because it
/// writes the host clipboard; the on-hardware suite runs it.
#[test]
#[ignore = "writes the real system clipboard; run on hardware"]
fn test_copy_round_trips() {
    let text = "中文复制 🎉\nsecond line";
    super::copy(text).expect("the clipboard write succeeds");
    let read = read_clipboard_text();
    assert_eq!(
        read.as_deref(),
        Some(text),
        "the clipboard holds the exact text"
    );
}

/// Closes the clipboard on drop, paired with the manual open in
/// read_clipboard_text.
struct CloseOnDrop;

impl Drop for CloseOnDrop {
    #[expect(unsafe_code, reason = "closes the clipboard opened above")]
    fn drop(&mut self) {
        drop(unsafe { CloseClipboard() });
    }
}

/// Read the clipboard text through GetClipboardData as UTF-16. None when
/// the clipboard holds no text or cannot be opened.
#[expect(unsafe_code, reason = "reads the clipboard through the Win32 API")]
fn read_clipboard_text() -> Option<String> {
    unsafe { OpenClipboard(None) }.ok()?;
    let guard = CloseOnDrop;
    let handle = unsafe { GetClipboardData(CF_UNICODETEXT) }.ok()?;
    let global = HGLOBAL(handle.0);
    let locked = unsafe { GlobalLock(global) };
    if locked.is_null() {
        return None;
    }
    let bytes = unsafe { GlobalSize(global) };
    let wide: Vec<u16> = unsafe { from_raw_parts(locked.cast::<u16>(), bytes / 2) }.to_vec();
    drop(unsafe { GlobalUnlock(global) });
    drop(guard);
    let end = wide
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(wide.len());
    Some(String::from_utf16_lossy(&wide[..end]))
}
