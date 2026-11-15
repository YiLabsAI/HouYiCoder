//! Handing text to the desktop clipboard.
//!
//! The terminal escape that carries a copy is the caller's business; this
//! module answers the one question the caller cannot: whether the host has a
//! native utility worth running beside it, and which one. A clipboard write is
//! fire and forget, so a failure here is silent by design: the escape the
//! caller also emits is the fallback.

#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
use std::io::{Error, Result, Write};
#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;
#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
use std::process::{Command, Stdio};

#[cfg(all(test, target_os = "macos"))]
mod tests;

/// The creation flag that suppresses the console window a utility spawn would
/// otherwise allocate, avoiding the conhost flash on every copy.
#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Run one clipboard utility, feeding the text on standard input. Ok only on
/// a clean run, so the Linux probe can pick a winner from the same call.
///
/// Both streams are closed: a utility that inherited this process's output
/// would paint over the frame the copy was made from.
#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
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
    // A tool command never needs a console window of its own; the flag also
    // avoids the conhost allocation that flashes a black box on every copy.
    #[cfg(target_os = "windows")]
    cmd.creation_flags(CREATE_NO_WINDOW);
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

/// Whether a native utility is worth running on this host.
///
/// A terminal on the operating system's own desktop is the case it answers:
/// the escape a copy also emits is refused by some terminals, and the native
/// utility covers exactly that gap.
pub fn native_clipboard_preferred() -> bool {
    cfg!(target_os = "macos")
}

/// Hand text to the host's clipboard utility, blocking until it finishes.
///
/// The caller runs this off the interactive path; a terminal escape is
/// emitted either way, so a missing or failing utility costs nothing. On a
/// host with no utility the call returns without writing.
pub fn copy_to_native_clipboard(text: &str) {
    #[cfg(target_os = "macos")]
    {
        crate::macos::clipboard::copy(text);
    }
    #[cfg(target_os = "linux")]
    {
        crate::linux::clipboard::copy(text);
    }
    #[cfg(target_os = "windows")]
    {
        crate::windows::clipboard::copy(text);
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        let _ = text;
    }
}
