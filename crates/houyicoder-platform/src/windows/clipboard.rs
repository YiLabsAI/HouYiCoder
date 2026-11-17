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
use std::mem::size_of;
use std::os::windows::ffi::OsStrExt;
use std::ptr::copy_nonoverlapping;

use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};

// The clipboard format for Unicode text: a UTF-16 string the clipboard
// reader delivers through GetClipboardData. The value is a Win32 constant
// the crate does not re-export, so it is spelled here.
const CF_UNICODETEXT: u32 = 13;

/// Copy text to the clipboard through the Win32 API. Ok only when the text
/// reaches the clipboard as Unicode; the caller treats an Err as a reason to
/// fall back to the terminal escape.
pub(crate) fn copy(text: &str) -> Result<()> {
    let wide: Vec<u16> = OsStr::new(text).encode_wide().chain(once(0)).collect();
    let bytes = wide.len() * size_of::<u16>();
    // GMEM_MOVEABLE: SetClipboardData requires a movable handle the system
    // can take ownership of. The allocation includes the trailing NUL the
    // clipboard reader expects.
    let handle =
        unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes) }.map_err(|e| Error::other(e.to_string()))?;
    // Lock, copy, unlock: the lock count must return to zero before the
    // handle is handed to the clipboard.
    let locked = unsafe { GlobalLock(handle) };
    if locked.is_null() {
        free_global(handle);
        return Err(Error::other("the clipboard memory lock failed"));
    }
    unsafe { copy_nonoverlapping(wide.as_ptr() as *const u8, locked as *mut u8, bytes) };
    drop(unsafe { GlobalUnlock(handle) });
    // OpenClipboard with no owner window: the clipboard is a process-wide
    // resource, and an HWND is only needed for message relay which this
    // synchronous write does not use.
    let opened = unsafe { OpenClipboard(None) }.is_ok();
    if !opened {
        free_global(handle);
        return Err(Error::other("the clipboard was held by another process"));
    }
    // The set transfers ownership: on success the system owns the handle and
    // this code must not free it. EmptyClipboard clears the previous content
    // so the set is the sole owner of what the reader finds. HGLOBAL and
    // HANDLE are both a pointer newtype; the clipboard takes the generic
    // handle form.
    let as_handle = HANDLE(handle.0);
    let set_ok = unsafe { EmptyClipboard() }.is_ok()
        && unsafe { SetClipboardData(CF_UNICODETEXT, Some(as_handle)) }.is_ok();
    drop(unsafe { CloseClipboard() });
    if !set_ok {
        free_global(handle);
        return Err(Error::other("the clipboard write was refused"));
    }
    Ok(())
}

/// Free a global allocation the clipboard did not take ownership of. The
/// result is ignored: the handle is already in a failure path, and a second
/// error reporting the first would lose the original cause.
fn free_global(handle: HGLOBAL) {
    drop(unsafe { GlobalFree(Some(handle)) });
}
