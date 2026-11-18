//! Unix clipboard delivery: the tmux paste buffer, the bounded subprocess
//! wait shared by every clipboard utility, and the decision of which path
//! a write takes. Only unix reaches this module; the caller above stays
//! free of platform conditionals. The terminal escape a copy can fall back
//! to is emitted by the caller, which owns the terminal.

use std::env::var_os;
use std::io::{Error, Result, Write as _};
use std::process::{Child, Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use crate::desktop::clipboard::CopyOutcome;

#[cfg(test)]
mod tests;

/// How long a clipboard subprocess may run before it is killed. pbcopy,
/// xclip, and tmux load-buffer are all fast in normal use; a hang past this
/// bound is a pathology the caller must not freeze on.
pub(crate) const CLIPBOARD_TIMEOUT: Duration = Duration::from_secs(2);

/// Write text to the clipboard through the most robust local path, and
/// report what happened. The tmux paste buffer is tried first when inside
/// tmux, because it works over SSH too. The native clipboard is tried only
/// on a local session and only when the buffer did not already take the
/// text, so a successful buffer load is not followed by a second write the
/// user never pastes from.
pub(crate) fn copy_local(text: &str) -> CopyOutcome {
    let in_ssh = var_os("SSH_CONNECTION").is_some();
    let in_tmux = var_os("TMUX").is_some();
    let tmux_buffer_ok = in_tmux && load_tmux_buffer(text);
    let native_ok = !in_ssh && !tmux_buffer_ok && native_paths(text);
    decide_outcome(CopyInputs {
        in_ssh,
        in_tmux,
        tmux_buffer_ok,
        native_ok,
    })
}

/// Whether the native utility path completed. macOS and Linux answer
/// through their own probe; a probe that finds no tool answers false.
fn native_paths(text: &str) -> bool {
    #[cfg(target_os = "macos")]
    {
        crate::macos::clipboard::copy(text).is_ok()
    }
    #[cfg(target_os = "linux")]
    {
        crate::linux::clipboard::copy(text).is_ok()
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = text;
        false
    }
}

/// The inputs to the outcome decision, collected so the pure decision
/// function takes one value rather than a row of bools. Production and
/// tests share this type so the branching cannot diverge between them.
pub(crate) struct CopyInputs {
    pub in_ssh: bool,
    pub in_tmux: bool,
    pub tmux_buffer_ok: bool,
    pub native_ok: bool,
}

/// The outcome a write reports, given which paths were tried and which
/// succeeded. Pure so tests exercise the SSH gate and the buffer fallback
/// without calling system APIs. Production calls this after gathering the
/// side-effect results, so the branching lives in one place.
pub(crate) fn decide_outcome(c: CopyInputs) -> CopyOutcome {
    // tmux: tried before native because it works over SSH too. The buffer
    // is on the remote tmux server, and -w bridges to the outer terminal.
    if c.in_tmux && c.tmux_buffer_ok {
        return CopyOutcome::TmuxBufferCopied;
    }
    // Native is never attempted over SSH: the utility would write to the
    // remote clipboard, which the user's local terminal cannot paste.
    if c.in_ssh {
        return CopyOutcome::NoNativePath;
    }
    if c.native_ok {
        CopyOutcome::NativeCopied
    } else {
        CopyOutcome::NativeFailed
    }
}

/// Load text into the tmux paste buffer via load-buffer. -w (tmux 3.2+)
/// propagates to the outer terminal via the multiplexer own OSC 52; dropped
/// for iTerm2 where that emission crashes an SSH session.
#[expect(clippy::disallowed_methods, reason = "infra spawn, not model-driven")]
pub(crate) fn load_tmux_buffer(text: &str) -> bool {
    let args: &[&str] = if var_os("LC_TERMINAL").is_some_and(|v| v == "iTerm2") {
        &["load-buffer", "-"]
    } else {
        &["load-buffer", "-w", "-"]
    };
    match Command::new("tmux")
        .args(args)
        .stdin(Stdio::piped())
        .spawn()
    {
        Ok(mut child) => {
            if let Some(mut stdin) = child.stdin.take() {
                let _r = stdin.write_all(text.as_bytes());
                drop(stdin);
            }
            wait_bounded(&mut child)
        }
        Err(_) => false,
    }
}

/// Wait for a child with a bounded deadline, killing it if it does not
/// exit in time. Returns whether the child exited successfully.
pub(crate) fn wait_bounded(child: &mut Child) -> bool {
    let deadline = Instant::now() + CLIPBOARD_TIMEOUT;
    while Instant::now() < deadline {
        if let Ok(Some(status)) = child.try_wait() {
            return status.success();
        }
        sleep(Duration::from_millis(10));
    }
    drop(child.kill());
    child.wait().is_ok_and(|s| s.success())
}

/// Run one clipboard utility, feeding the text on standard input and
/// waiting with the shared bounded wait. Used by the macOS and Linux
/// backends so every clipboard subprocess shares the same hang bound.
#[expect(
    clippy::disallowed_methods,
    reason = "L0 clipboard utility below the ProcessLauncher port; no runtime dependency"
)]
pub(crate) fn run_utility_unix(program: &str, args: &[&str], text: &str) -> Result<()> {
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
    if wait_bounded(&mut child) {
        Ok(())
    } else {
        Err(Error::other("the clipboard utility failed or timed out"))
    }
}
