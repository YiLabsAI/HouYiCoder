//! Clipboard path-selection tests for the hardened OSC 52 write path.
//! Asserts the SSH-gating logic (native vs OSC 52), the tmux fallback
//! prediction, and the raw / passthrough OSC 52 byte-sequence shape. No
//! real clipboard or subprocess calls: the pure decision function and the
//! sequence builders are exercised directly.

#![cfg(test)]

use crate::selection::{clipboard_path_for, raw_osc52, tmux_passthrough_osc52};
use base64::{Engine as _, engine::general_purpose::STANDARD};

#[test]
fn test_native_when_no_ssh() {
    // No SSH session: a native tool is the high-confidence path.
    assert_eq!(clipboard_path_for(true, false), "native");
}

#[test]
fn test_ssh_gates_to_osc52() {
    // Over SSH the native tool would write to the remote clipboard, so the
    // prediction falls through to OSC 52 (no tmux here).
    assert_eq!(clipboard_path_for(false, false), "osc52");
}

#[test]
fn test_tmux_buffer_path() {
    // SSH session inside tmux: native is gated off, tmux-buffer is next.
    assert_eq!(clipboard_path_for(false, true), "tmux-buffer");
}

#[test]
fn test_native_wins_over_tmux() {
    // Local macOS in tmux: native still wins (higher confidence than the
    // buffer, which depends on tmux set-clipboard config).
    assert_eq!(clipboard_path_for(true, true), "native");
}

#[test]
fn test_raw_osc52_bel_terminator() {
    // Raw OSC 52 uses BEL (\x07), not ST, for wider terminal support.
    let seq = raw_osc52("aGVsbG8=");
    assert_eq!(seq, "\x1b]52;c;aGVsbG8=\x07");
}

#[test]
fn test_tmux_passthrough_esc_doubled() {
    // DCS passthrough wraps the inner OSC 52 and doubles every inner ESC.
    // The inner raw sequence has one ESC (start); the wrapped form has the
    // DCS opener ESC, then the doubled pair, then the ST closer.
    let seq = tmux_passthrough_osc52("aGVsbG8=");
    // Opens with ESC P t m u x ;
    assert!(seq.starts_with("\x1bPtmux;"), "missing DCS opener: {seq:?}");
    // The inner OSC 52 with doubled ESC: ESC ESC ] 5 2 ; c ; <b64> BEL
    assert!(
        seq.contains("\x1b\x1b]52;c;aGVsbG8=\x07"),
        "inner doubled-ESC OSC 52 not found: {seq:?}"
    );
    // Closes with ST (ESC backslash).
    assert!(seq.ends_with("\x1b\\"), "missing ST closer: {seq:?}");
}

#[test]
fn test_osc52_b64_encoding() {
    // The b64 step must use standard padded base64 (OSC 52 spec).
    let b64 = STANDARD.encode(b"hello");
    assert_eq!(b64, "aGVsbG8=");
    // The raw sequence embeds the b64 between ;c; and the BEL terminator.
    let seq = raw_osc52(&b64);
    assert_eq!(seq, format!("\x1b]52;c;{b64}\x07"));
}
