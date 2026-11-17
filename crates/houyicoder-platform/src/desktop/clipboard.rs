//! Handing text to the desktop clipboard.
//!
//! The terminal escape that carries a copy is the caller's business; this
//! module answers the one question the caller cannot: whether the host has a
//! native clipboard the process can write to directly, and the call that does
//! so. The caller uses the result to decide whether the escape is still
//! needed: a clean native write is the whole copy, and a failure is the
//! reason to fall back to the escape.

#[cfg(target_os = "windows")]
use std::io::Result;
#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
use std::io::{Error, Result};
#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::io::{Error, Result, Write};
#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::process::{Command, Stdio};

#[cfg(all(test, target_os = "macos"))]
mod tests;

/// Run one clipboard utility, feeding the text on standard input. Ok only on
/// a clean run, so the Linux probe can pick a winner from the same call.
///
/// Both streams are closed: a utility that inherited this process's output
/// would paint over the frame the copy was made from. Windows uses the Win32
/// clipboard API directly rather than this runner, so it is reached only on
/// macOS and Linux where the utility is a subprocess.
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[expect(
    clippy::disallowed_methods,
    reason = "L0 clipboard utility below the ProcessLauncher port; no runtime dependency"
)]
pub(crate) fn run_utility(program: &str, args: &[&str], text: &str) -> Result<()> {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = cmd.spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(text.as_bytes())?;
        drop(stdin);
    }
    let status = child.wait()?;
    if status.success() {
        Ok(())
    } else {
        Err(Error::other("the clipboard utility exited non-zero"))
    }
}

/// Whether a native clipboard write is worth trying on this host.
///
/// macOS and Windows both offer a direct clipboard the process can write to
/// without a terminal escape: the macOS pasteboard through pbcopy and the
/// Windows clipboard through the Win32 API. Linux relies on a subprocess
/// utility that may not be installed, so the caller treats it as a probe
/// rather than a preference.
pub fn native_clipboard_preferred() -> bool {
    cfg!(any(target_os = "macos", target_os = "windows"))
}

/// Hand text to the host's native clipboard, blocking until it finishes.
///
/// The caller uses the result to decide whether to also emit the terminal
/// escape: Ok is the whole copy, and an Err is the reason to fall back.
pub fn copy_to_native_clipboard(text: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        crate::macos::clipboard::copy(text)
    }
    #[cfg(target_os = "linux")]
    {
        crate::linux::clipboard::copy(text)
    }
    #[cfg(target_os = "windows")]
    {
        crate::windows::clipboard::copy(text)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        let _ = text;
        Err(Error::other("no native clipboard on this host"))
    }
}
