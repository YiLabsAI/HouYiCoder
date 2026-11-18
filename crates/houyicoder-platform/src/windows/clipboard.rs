//! The Windows clipboard through the Win32 API, not a subprocess.
//!
//! clip.exe reads its stdin through the console code page, so a Unicode text
//! fed to it arrives as the code page's interpretation of its bytes: CJK
//! garbled on a CP936 host. The clipboard API takes a UTF-16 handle directly,
//! which the system owns after the set, so the text reaches the clipboard
//! without a code page in the path.

#![expect(unsafe_code, reason = "the Win32 clipboard API takes raw handles")]

use std::ffi::OsStr;
use std::io::{Error, Result};
use std::iter::once;
use std::mem::ManuallyDrop;
use std::mem::size_of_val;
use std::os::windows::ffi::OsStrExt;
use std::ptr::copy_nonoverlapping;
use std::thread::sleep;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};

#[cfg(test)]
mod tests;

// The clipboard format for Unicode text: a UTF-16 string the clipboard
// reader delivers through GetClipboardData. The value is a Win32 constant
// the crate does not re-export, so it is spelled here.
const CF_UNICODETEXT: u32 = 13;

/// How long OpenClipboard retries when another process holds the clipboard.
/// Clipboard managers and remote-desktop agents hold it for milliseconds at
/// a time, so a short bounded retry covers the common contention without
/// waiting on a pathological holder.
const OPEN_RETRY_SPAN: Duration = Duration::from_millis(75);
/// The pause between OpenClipboard attempts.
const OPEN_RETRY_PAUSE: Duration = Duration::from_millis(15);

/// A global allocation that frees itself when the clipboard does not take
/// ownership of it. SetClipboardData transfers ownership on success, so the
/// release is canceled before the handoff and kept on every failure path.
struct GlobalBuffer(HGLOBAL);

impl GlobalBuffer {
    /// Allocate a movable buffer holding the UTF-16 text and its trailing
    /// NUL. GMEM_MOVEABLE is what SetClipboardData requires: the system
    /// takes ownership of a movable handle it can relocate.
    fn for_wide(wide: &[u16]) -> Result<Self> {
        let bytes = size_of_val(wide);
        let handle = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes) }
            .map_err(|e| Error::other(e.to_string()))?;
        Ok(Self(handle))
    }

    /// Copy the wide text into the buffer. The lock count returns to zero
    /// before the buffer is handed to the clipboard.
    fn fill(&self, wide: &[u16]) -> Result<()> {
        let bytes = size_of_val(wide);
        let locked = unsafe { GlobalLock(self.0) };
        if locked.is_null() {
            return Err(Error::other("the clipboard memory lock failed"));
        }
        unsafe { copy_nonoverlapping(wide.as_ptr() as *const u8, locked as *mut u8, bytes) };
        drop(unsafe { GlobalUnlock(self.0) });
        Ok(())
    }

    /// The clipboard handle form of this buffer. The guard stays alive over
    /// the same handle: on a failed set it frees the memory, and on success
    /// the caller releases it so the system owns the memory instead.
    fn as_clipboard_handle(&self) -> HANDLE {
        HANDLE(self.0.0)
    }

    /// Give the handle to the clipboard: wrapping the guard in ManuallyDrop
    /// skips the drop that would free memory the system now owns.
    fn release(self) {
        let _ = ManuallyDrop::new(self);
    }
}

impl Drop for GlobalBuffer {
    fn drop(&mut self) {
        drop(unsafe { GlobalFree(Some(self.0)) });
    }
}

/// An open clipboard session that closes itself. OpenClipboard grants one
/// owner at a time, so the close in drop is what releases the lock for the
/// next writer.
struct OpenClipboardGuard;

impl OpenClipboardGuard {
    /// Open the clipboard, retrying briefly when another process holds it.
    fn open() -> Result<Self> {
        let deadline = Instant::now() + OPEN_RETRY_SPAN;
        loop {
            // No owner window: the clipboard is a process-wide resource, and
            // an HWND is only needed for message relay which this write does
            // not use.
            if unsafe { OpenClipboard(None) }.is_ok() {
                return Ok(Self);
            }
            if Instant::now() >= deadline {
                return Err(Error::other("the clipboard was held by another process"));
            }
            sleep(OPEN_RETRY_PAUSE);
        }
    }
}

impl Drop for OpenClipboardGuard {
    fn drop(&mut self) {
        drop(unsafe { CloseClipboard() });
    }
}

/// Copy text to the clipboard through the Win32 API. Ok only when the text
/// reaches the clipboard as Unicode; the caller treats an Err as a reason to
/// fall back to the terminal escape.
pub(crate) fn copy(text: &str) -> Result<()> {
    let wide: Vec<u16> = OsStr::new(text).encode_wide().chain(once(0)).collect();
    let buffer = GlobalBuffer::for_wide(&wide)?;
    buffer.fill(&wide)?;
    let _session = OpenClipboardGuard::open()?;
    // EmptyClipboard clears the previous content so the set is the sole
    // owner of what the reader finds.
    unsafe { EmptyClipboard() }.map_err(|e| Error::other(e.to_string()))?;
    let set = unsafe { SetClipboardData(CF_UNICODETEXT, Some(buffer.as_clipboard_handle())) };
    match set {
        // Success transfers ownership: the guard releases its claim so the
        // system owns the handle from here on.
        Ok(_) => {
            buffer.release();
            Ok(())
        }
        // A failed set returns ownership to this process, and the guard
        // frees it on the way out.
        Err(e) => Err(Error::other(e.to_string())),
    }
}
