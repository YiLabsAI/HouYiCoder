//! Clipboard delivery for a completed selection.
//!
//! The write is synchronous with a bounded wait: the native clipboard API
//! and the terminal escape both complete in microseconds, and the subprocess
//! utilities (pbcopy, xclip, tmux load-buffer) are killed after two seconds
//! if they hang. A typed outcome carries the result so the toast reports
//! what happened rather than what was predicted.

use std::env::var_os;
use std::io::stdout;
use std::io::{Result, Write as _};
use std::process::{Child, Command, Stdio};
#[cfg(test)]
use std::sync::{Arc, Mutex};
use std::thread::sleep;
use std::time::{Duration, Instant};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use houyicoder_platform::desktop::clipboard::copy_to_native_clipboard;

/// What a clipboard write accomplished. The toast the loop raises reads this
/// rather than a predicted path, so a native failure that fell back to the
/// escape is reported as a fallback, not as a clean native copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyOutcome {
    /// The native clipboard accepted the text. No terminal escape was sent.
    NativeCopied,
    /// The tmux paste buffer holds the text. No terminal escape was sent.
    TmuxBufferCopied,
    /// The terminal escape was sent as the sole path. The clipboard may or
    /// may not receive it depending on terminal settings the process cannot
    /// verify, so the toast is neutral, not a success claim.
    TerminalRequested,
    /// The native write failed and the terminal escape was sent as a
    /// fallback. The toast warns about the fallback rather than claiming the
    /// native write succeeded.
    NativeFailedTerminalRequested,
    /// Both the native write and the terminal escape failed. The selection
    /// is kept so the user can retry.
    CopyFailed,
}

/// Pluggable clipboard sink so adversarial selection tests can capture what a
/// drag-to-copy would write, without touching the real OS clipboard or
/// emitting OSC 52. Production holds a SystemClipboard; tests inject a
/// RecordingClipboard and assert the exact copied text.
pub trait ClipboardWriter: Send + Sync {
    /// Write the text and report what happened.
    fn write(&self, text: &str) -> CopyOutcome;
}

/// The production clipboard writer: the host's native clipboard where the
/// platform offers one, since it works even when the terminal refuses the
/// escape; the terminal escape carries the text over a remote session.
pub struct SystemClipboard;
impl ClipboardWriter for SystemClipboard {
    fn write(&self, text: &str) -> CopyOutcome {
        write_clipboard(text)
    }
}

#[cfg(test)]
/// Test-only clipboard that records every write so adversarial selection
/// tests can assert the exact copied text without touching the OS clipboard.
pub struct RecordingClipboard {
    pub captured: Arc<Mutex<Vec<String>>>,
}
#[cfg(test)]
impl ClipboardWriter for RecordingClipboard {
    fn write(&self, text: &str) -> CopyOutcome {
        self.captured
            .lock()
            .expect("recording clipboard")
            .push(text.to_string());
        CopyOutcome::NativeCopied
    }
}

/// Write text to the clipboard through the most robust path available, and
/// report what happened. The tmux paste buffer is tried first when inside
/// tmux, because it works over SSH too. The native clipboard is tried
/// whenever the session is local, regardless of whether the host is known
/// to prefer it, and a probe that finds no tool simply fails and falls
/// back. The terminal escape is the final fallback. A native success
/// suppresses the escape so the two writers never compete.
pub fn write_clipboard(text: &str) -> CopyOutcome {
    let in_ssh = var_os("SSH_CONNECTION").is_some();
    let in_tmux = var_os("TMUX").is_some();
    let tmux_buffer_ok = in_tmux && tmux_load_buffer(text);
    if tmux_buffer_ok {
        return CopyOutcome::TmuxBufferCopied;
    }
    let native_available = !in_ssh;
    let native_ok = native_available && copy_to_native_clipboard(text).is_ok();
    let osc52_ok = native_ok || emit_osc52(text, in_tmux) != CopyOutcome::CopyFailed;
    decide_outcome(CopyInputs {
        in_ssh,
        in_tmux,
        tmux_buffer_ok,
        native_available,
        native_ok,
        osc52_ok,
    })
}

/// The inputs to the outcome decision, collected so the pure decision
/// function takes one value rather than a row of bools the workspace
/// forbids. Production and tests share this type so the branching cannot
/// diverge between them.
pub(crate) struct CopyInputs {
    pub in_ssh: bool,
    pub in_tmux: bool,
    pub tmux_buffer_ok: bool,
    pub native_available: bool,
    pub native_ok: bool,
    pub osc52_ok: bool,
}

/// The outcome a write reports, given which paths are available and which
/// succeeded. Pure so tests exercise the SSH gate, the tmux fallback, and
/// the native-success suppression of the escape without calling system APIs.
/// Production calls this after gathering the side-effect results, so the
/// decision logic lives in one place rather than being duplicated.
pub(crate) fn decide_outcome(c: CopyInputs) -> CopyOutcome {
    // tmux: tried before native because it works over SSH too. The buffer
    // is on the remote tmux server, and -w bridges to the outer terminal.
    if c.in_tmux && c.tmux_buffer_ok {
        return CopyOutcome::TmuxBufferCopied;
    }
    // Native: gated on SSH because a native utility would write to the
    // remote clipboard. On success the escape is suppressed; on failure the
    // escape is the fallback and the outcome names it as a fallback.
    if !c.in_ssh && c.native_available {
        return match c.native_ok {
            true => CopyOutcome::NativeCopied,
            false => match osc_outcome(c.osc52_ok) {
                CopyOutcome::TerminalRequested => CopyOutcome::NativeFailedTerminalRequested,
                other => other,
            },
        };
    }
    osc_outcome(c.osc52_ok)
}

/// The outcome from an OSC 52 emit, given whether the write reached stdout.
fn osc_outcome(osc52_ok: bool) -> CopyOutcome {
    if osc52_ok {
        CopyOutcome::TerminalRequested
    } else {
        CopyOutcome::CopyFailed
    }
}

/// Emit the OSC 52 escape and report whether it reached the terminal. The
/// passthrough form is used inside tmux so the outer terminal sees the
/// sequence; a raw emit is the direct path otherwise.
pub(crate) fn emit_osc52(text: &str, in_tmux: bool) -> CopyOutcome {
    let b64 = STANDARD.encode(text.as_bytes());
    let seq = if in_tmux {
        tmux_passthrough_osc52(&b64)
    } else {
        raw_osc52(&b64)
    };
    match emit_raw(&seq) {
        Ok(()) => CopyOutcome::TerminalRequested,
        Err(_) => CopyOutcome::CopyFailed,
    }
}

/// Raw OSC 52: ESC ] 52 ; c ; <base64> BEL. BEL is more widely parsed than
/// ST (ESC backslash) for OSC 52 across terminals.
pub(crate) fn raw_osc52(b64: &str) -> String {
    format!("\x1b]52;c;{b64}\x07")
}

/// Wrap an OSC 52 payload in tmux DCS passthrough (ESC P tmux ; <payload
/// with inner ESCs doubled> ESC backslash). tmux forwards it to the outer
/// terminal, bypassing its own parser. Needs allow-passthrough on;
/// otherwise the DCS is silently dropped (no worse than the raw fallback).
pub(crate) fn tmux_passthrough_osc52(b64: &str) -> String {
    let escaped = raw_osc52(b64).replace('\x1b', "\x1b\x1b");
    format!("\x1bPtmux;{escaped}\x1b\\")
}

/// Load text into the tmux paste buffer via load-buffer. -w (tmux 3.2+)
/// propagates to the outer terminal via the multiplexer own OSC 52; dropped
/// for iTerm2 where that emission crashes an SSH session.
#[expect(clippy::disallowed_methods, reason = "infra spawn, not model-driven")]
pub(crate) fn tmux_load_buffer(text: &str) -> bool {
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

/// Write a byte sequence to stdout (locked, flushed). OSC 52 is a one-way
/// escape the terminal does not acknowledge; a stdout write failure is
/// reported so the caller can choose a failure outcome.
fn emit_raw(seq: &str) -> Result<()> {
    let mut out = stdout().lock();
    out.write_all(seq.as_bytes())?;
    out.flush()
}

/// How long a clipboard subprocess may run before it is killed. pbcopy,
/// xclip, and tmux load-buffer are all fast in normal use; a hang past this
/// bound is a pathology the TUI must not freeze on.
const CLIPBOARD_TIMEOUT: Duration = Duration::from_secs(2);

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
