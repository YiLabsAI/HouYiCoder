//! Handing text to the desktop clipboard.
//!
//! This module answers what a native write accomplished. The terminal
//! escape that can carry a copy when no native path works belongs to the
//! caller, which owns the terminal; the platform never claims an escape it
//! did not send.

#[cfg(unix)]
use crate::unix::clipboard::copy_local;
#[cfg(target_os = "windows")]
use crate::windows::clipboard::copy as windows_copy;
#[cfg(target_os = "windows")]
use std::env::var_os;

/// What one native clipboard write accomplished. A caller that adds the
/// terminal escape maps these onto its own report, so a user is never told
/// an escape was sent when none was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyOutcome {
    /// The native clipboard accepted the text.
    NativeCopied,
    /// The tmux paste buffer holds the text.
    TmuxBufferCopied,
    /// A native path was attempted and failed; the caller falls back to the
    /// terminal escape.
    NativeFailed,
    /// No native path applies on this host or session, so none was tried.
    NoNativePath,
}

/// Write text to the host's clipboard where the platform offers a path.
#[cfg(unix)]
pub fn copy_to_native_clipboard(text: &str) -> CopyOutcome {
    copy_local(text)
}

/// Write text to the host's clipboard where the platform offers a path.
#[cfg(target_os = "windows")]
pub fn copy_to_native_clipboard(text: &str) -> CopyOutcome {
    if var_os("SSH_CONNECTION").is_some() {
        return CopyOutcome::NoNativePath;
    }
    match windows_copy(text) {
        Ok(()) => CopyOutcome::NativeCopied,
        Err(error) => {
            // The Win32 reason is for a developer; the caller reports the
            // fallback to the user.
            tracing::debug!(%error, "the native clipboard write failed");
            CopyOutcome::NativeFailed
        }
    }
}

/// No native clipboard exists on this host.
#[cfg(not(any(unix, target_os = "windows")))]
pub fn copy_to_native_clipboard(_text: &str) -> CopyOutcome {
    CopyOutcome::NoNativePath
}
