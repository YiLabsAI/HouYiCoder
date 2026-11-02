//! Clipboard delivery for a completed selection.
//!
//! The write is fire-and-forget: a native utility runs in a detached thread
//! when the session is local, and a terminal escape carries the text over
//! SSH and through tmux. A failure is silent because the gesture has no
//! second chance to report one.

use std::env::var_os;
use std::io::Error;
use std::io::Result;
use std::io::Write as _;
use std::io::stdout;
use std::process::{Command, Stdio};
#[cfg(test)]
use std::sync::{Arc, Mutex};
use std::thread::spawn;

use base64::{Engine as _, engine::general_purpose::STANDARD};

#[cfg(target_os = "linux")]
use super::linux::linux_clipboard_tool;

/// Pluggable clipboard sink so adversarial selection tests can capture what a
/// drag-to-copy would write, without touching the real OS clipboard or
/// emitting OSC 52. Production holds a SystemClipboard; tests inject a
/// RecordingClipboard and assert the exact copied text.
pub trait ClipboardWriter: Send + Sync {
    fn write(&self, text: &str);
}

/// The production clipboard writer: pbcopy on macOS (reliable even when the
/// terminal disables OSC 52), OSC 52 elsewhere for SSH/remote sessions.
pub struct SystemClipboard;
impl ClipboardWriter for SystemClipboard {
    fn write(&self, text: &str) {
        write_clipboard(text);
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
    fn write(&self, text: &str) {
        self.captured
            .lock()
            .expect("recording clipboard")
            .push(text.to_string());
    }
}

/// Write text to the clipboard via the most robust path available. The
/// strategy: (1) a native safety net fired FIRST in a
/// detached thread (pbcopy / wl-copy / xclip / xsel / clip.exe), gated on
/// SSH_CONNECTION so it never writes to a remote clipboard over SSH; (2)
/// inside tmux, load-buffer -w propagates to the outer terminal, and a
/// DCS-passthrough-wrapped OSC 52 is also emitted; (3) raw OSC 52 (BEL
/// terminator) as the final fallback. Fire-and-forget; failures are silent.
pub fn write_clipboard(text: &str) {
    let b64 = STANDARD.encode(text.as_bytes());
    // Native net fires first in a thread so OSC 52 is not delayed by a
    // subprocess. Gated on SSH_CONNECTION: over SSH these hit the remote
    // clipboard, not the local one.
    if var_os("SSH_CONNECTION").is_none() {
        let owned = text.to_string();
        spawn(move || copy_native(&owned));
    }
    // tmux fast path: load-buffer reaches the outer terminal via the
    // multiplexer own clipboard bridge (-w, dropped for iTerm2). On
    // success also emit a passthrough-wrapped OSC 52.
    if var_os("TMUX").is_some() && tmux_load_buffer(text) {
        let _r = emit_raw(&tmux_passthrough_osc52(&b64));
        return;
    }
    let _r = emit_raw(&raw_osc52(&b64));
}

/// Which path write_clipboard will take, based on env state. Sync so a
/// caller can show an honest toast without running the copy. Native is only
/// claimed on macOS without an SSH session; the Linux probe is lazy.
pub fn get_clipboard_path() -> &'static str {
    clipboard_path_for(
        cfg!(target_os = "macos") && var_os("SSH_CONNECTION").is_none(),
        var_os("TMUX").is_some(),
    )
}

/// Pure decision used by get_clipboard_path so tests exercise the
/// SSH-gating and tmux fallback logic without mutating process env.
pub(crate) fn clipboard_path_for(native_available: bool, in_tmux: bool) -> &'static str {
    if native_available {
        "native"
    } else if in_tmux {
        "tmux-buffer"
    } else {
        "osc52"
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
// The clipboard helpers are a pure-client local copy path (tmux load-buffer,
// pbcopy); they live above the layer that owns the spawn chokepoint and must
// not pull a ports dependency, so the chokepoint rule is allowed here.
#[expect(clippy::disallowed_methods, reason = "infra spawn, not model-driven")]
fn tmux_load_buffer(text: &str) -> bool {
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
            child.wait().map(|s| s.success()).unwrap_or(false)
        }
        Err(_) => false,
    }
}

/// Shell out to a native clipboard utility as a safety net for OSC 52. Only
/// called when not in an SSH session; failures are silent. The Linux winner
/// is cached after the first probe so repeated mouse-ups skip the chain.
fn copy_native(text: &str) {
    #[cfg(target_os = "macos")]
    {
        let _r = run_native("pbcopy", &[], text);
    }
    #[cfg(target_os = "linux")]
    {
        if let Some(tool) = linux_clipboard_tool(text) {
            let args: &[&str] = match tool {
                "wl-copy" => &[],
                "xclip" => &["-selection", "clipboard"],
                "xsel" => &["--clipboard", "--input"],
                _ => &[],
            };
            let _r = run_native(tool, args, text);
        }
    }
    #[cfg(target_os = "windows")]
    {
        let _r = run_native("clip", &[], text);
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        let _ = text;
    }
}

/// Run a native clipboard binary, feeding text via stdin. Ok on a clean
/// (exit-0) run so the Linux probe can pick a winner.
#[expect(clippy::disallowed_methods, reason = "infra spawn, not model-driven")]
pub(super) fn run_native(bin: &str, args: &[&str], text: &str) -> Result<()> {
    let mut child = Command::new(bin).args(args).stdin(Stdio::piped()).spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(text.as_bytes())?;
        drop(stdin);
    }
    let status = child.wait()?;
    if status.success() {
        Ok(())
    } else {
        Err(Error::other("non-zero exit"))
    }
}

/// Write a byte sequence to stdout (locked, flushed). OSC 52 is a
/// fire-and-forget emit; a write failure is silent.
fn emit_raw(seq: &str) -> Result<()> {
    let mut out = stdout().lock();
    out.write_all(seq.as_bytes())?;
    out.flush()
}
