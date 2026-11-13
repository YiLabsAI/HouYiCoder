//! Handing text to the desktop clipboard.
//!
//! The terminal escape that carries a copy is the caller's business; this
//! module answers the one question the caller cannot: whether the host has a
//! native utility worth running beside it, and which one. A clipboard write is
//! fire and forget, so a failure here is silent by design: the escape the
//! caller also emits is the fallback.

#[cfg(target_os = "linux")]
use std::env::var_os;
#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
use std::io::{Error, Result, Write};
#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
use std::process::{Command, Stdio};
#[cfg(target_os = "linux")]
use std::sync::Mutex;

#[cfg(test)]
mod tests;

/// The Linux utility that answered last, cached so a copy costs no probe.
#[cfg(target_os = "linux")]
static LINUX_TOOL: Mutex<Option<Option<&'static str>>> = Mutex::new(None);

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
        drop(run_utility("pbcopy", &[], text));
    }
    #[cfg(target_os = "linux")]
    {
        if let Some(tool) = linux_clipboard_tool(text) {
            let args: &[&str] = match tool {
                "xclip" => &["-selection", "clipboard"],
                "xsel" => &["--clipboard", "--input"],
                _ => &[],
            };
            drop(run_utility(tool, args, text));
        }
    }
    #[cfg(target_os = "windows")]
    {
        drop(run_utility("clip", &[], text));
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        let _ = text;
    }
}

/// The Linux utility that answers, cached after the first copy. The probe
/// runs with the actual text because an empty stdin leaves some of these
/// waiting for input that never comes.
#[cfg(target_os = "linux")]
fn linux_clipboard_tool(text: &str) -> Option<&'static str> {
    let cached = LINUX_TOOL.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(winner) = *cached {
        return winner;
    }
    drop(cached);
    let wayland = var_os("WAYLAND_DISPLAY").is_some();
    let winner = if wayland && run_utility("wl-copy", &[], text).is_ok() {
        Some("wl-copy")
    } else if run_utility("xclip", &["-selection", "clipboard"], text).is_ok() {
        Some("xclip")
    } else if run_utility("xsel", &["--clipboard", "--input"], text).is_ok() {
        Some("xsel")
    } else {
        None
    };
    *LINUX_TOOL.lock().unwrap_or_else(|e| e.into_inner()) = Some(winner);
    winner
}

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
fn run_utility(program: &str, args: &[&str], text: &str) -> Result<()> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
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
