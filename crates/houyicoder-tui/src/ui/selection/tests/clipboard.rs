//! Clipboard outcome and OSC 52 sequence tests. The outcome decision is a
//! pure function over the session state and each path's success, so the
//! SSH gate, the tmux fallback, and the native-success suppression of the
//! escape are exercised without calling system APIs. The OSC 52 sequence
//! builders are asserted on their byte shape.

#![cfg(test)]

use super::super::clipboard::{
    CopyInputs, decide_outcome, emit_osc52, raw_osc52, tmux_passthrough_osc52,
};
use crate::selection::CopyOutcome;
use base64::{Engine as _, engine::general_purpose::STANDARD};

/// Over SSH the tmux paste buffer still works: it is on the remote tmux
/// server, and -w bridges to the outer terminal.
#[test]
fn test_ssh_tmux_wins() {
    assert_eq!(
        decide_outcome(CopyInputs {
            in_ssh: true,
            in_tmux: true,
            tmux_buffer_ok: true,
            native_available: true,
            native_ok: true,
            osc52_ok: true,
        }),
        CopyOutcome::TmuxBufferCopied
    );
}

/// Over SSH with no tmux the escape is the only path.
#[test]
fn test_ssh_escape() {
    assert_eq!(
        decide_outcome(CopyInputs {
            in_ssh: true,
            in_tmux: false,
            tmux_buffer_ok: false,
            native_available: true,
            native_ok: true,
            osc52_ok: true,
        }),
        CopyOutcome::TerminalRequested
    );
}

/// Over SSH with tmux but the buffer failed, the escape is the fallback.
#[test]
fn test_ssh_tmux_fails() {
    assert_eq!(
        decide_outcome(CopyInputs {
            in_ssh: true,
            in_tmux: true,
            tmux_buffer_ok: false,
            native_available: true,
            native_ok: true,
            osc52_ok: true,
        }),
        CopyOutcome::TerminalRequested
    );
}

/// Over SSH the escape itself can fail too.
#[test]
fn test_ssh_escape_fails() {
    assert_eq!(
        decide_outcome(CopyInputs {
            in_ssh: true,
            in_tmux: false,
            tmux_buffer_ok: false,
            native_available: false,
            native_ok: false,
            osc52_ok: false,
        }),
        CopyOutcome::CopyFailed
    );
}

/// A native success suppresses the escape: the two writers never compete.
#[test]
fn test_native_success_suppresses_escape() {
    assert_eq!(
        decide_outcome(CopyInputs {
            in_ssh: false,
            in_tmux: false,
            tmux_buffer_ok: false,
            native_available: true,
            native_ok: true,
            osc52_ok: false,
        }),
        CopyOutcome::NativeCopied
    );
}

/// A native failure falls back to the escape, and the outcome names the
/// fallback rather than a clean terminal request.
#[test]
fn test_native_failure_fallback() {
    assert_eq!(
        decide_outcome(CopyInputs {
            in_ssh: false,
            in_tmux: false,
            tmux_buffer_ok: false,
            native_available: true,
            native_ok: false,
            osc52_ok: true,
        }),
        CopyOutcome::NativeFailedTerminalRequested
    );
}

/// A native failure with a dead escape is a total failure.
#[test]
fn test_both_paths_fail() {
    assert_eq!(
        decide_outcome(CopyInputs {
            in_ssh: false,
            in_tmux: false,
            tmux_buffer_ok: false,
            native_available: true,
            native_ok: false,
            osc52_ok: false,
        }),
        CopyOutcome::CopyFailed
    );
}

/// The tmux paste buffer is the whole copy when it answers, even locally.
#[test]
fn test_tmux_buffer_success() {
    assert_eq!(
        decide_outcome(CopyInputs {
            in_ssh: false,
            in_tmux: true,
            tmux_buffer_ok: true,
            native_available: true,
            native_ok: false,
            osc52_ok: true,
        }),
        CopyOutcome::TmuxBufferCopied
    );
}

/// No native and no tmux: the escape is the sole path.
#[test]
fn test_escape_only_path() {
    assert_eq!(
        decide_outcome(CopyInputs {
            in_ssh: false,
            in_tmux: false,
            tmux_buffer_ok: false,
            native_available: false,
            native_ok: false,
            osc52_ok: true,
        }),
        CopyOutcome::TerminalRequested
    );
}

/// The escape itself failed with no other path available.
#[test]
fn test_escape_failed_alone() {
    assert_eq!(
        decide_outcome(CopyInputs {
            in_ssh: false,
            in_tmux: false,
            tmux_buffer_ok: false,
            native_available: false,
            native_ok: false,
            osc52_ok: false,
        }),
        CopyOutcome::CopyFailed
    );
}

/// Emitting the raw OSC 52 reports a terminal request when stdout accepts
/// the bytes.
#[test]
fn test_emit_osc52_reports_requested() {
    assert_eq!(emit_osc52("hi", false), CopyOutcome::TerminalRequested);
}

/// Emitting the tmux passthrough form reaches the same outcome.
#[test]
fn test_emit_osc52_tmux_form() {
    assert_eq!(emit_osc52("hi", true), CopyOutcome::TerminalRequested);
}

/// Raw OSC 52 uses BEL, not ST, for wider terminal support.
#[test]
fn test_raw_osc52_bel_terminator() {
    let seq = raw_osc52("aGVsbG8=");
    assert_eq!(seq, "\x1b]52;c;aGVsbG8=\x07");
}

/// DCS passthrough wraps the inner OSC 52 and doubles every inner ESC.
#[test]
fn test_tmux_passthrough_esc_doubled() {
    let seq = tmux_passthrough_osc52("aGVsbG8=");
    assert!(seq.starts_with("\x1bPtmux;"), "missing DCS opener: {seq:?}");
    assert!(
        seq.contains("\x1b\x1b]52;c;aGVsbG8=\x07"),
        "inner doubled-ESC OSC 52 not found: {seq:?}"
    );
    assert!(seq.ends_with("\x1b\\"), "missing ST closer: {seq:?}");
}

/// The b64 step must use standard padded base64.
#[test]
fn test_osc52_b64_encoding() {
    let b64 = STANDARD.encode(b"hello");
    assert_eq!(b64, "aGVsbG8=");
    let seq = raw_osc52(&b64);
    assert_eq!(seq, format!("\x1b]52;c;{b64}\x07"));
}
