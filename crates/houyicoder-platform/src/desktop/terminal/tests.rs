//! Tests for the terminal boundary module.

#[cfg(unix)]
use std::time::Duration;

use super::{
    SYNC_BEGIN, SYNC_END, TerminalCaps, TerminationWatch, caps_for, control_letter,
    paste_arrives_as_keystrokes,
};
#[cfg(unix)]
use super::{wait_console_input, wait_consumes_record};

/// The frame markers are the protocol contract: a terminal that accepts
/// synchronized output turns it on with the set form and off with the reset
/// form, and a byte of drift in either breaks the bracket the renderer
/// relies on.
#[test]
fn test_sync_frame_markers() {
    assert_eq!(SYNC_BEGIN, b"\x1b[?2026h");
    assert_eq!(SYNC_END, b"\x1b[?2026l");
}

/// A watch built for a stated flag reports it, and the shared decision
/// folds the quit and the signal into one answer.
#[test]
fn test_watch_reports_state() {
    let quiet = TerminationWatch::for_test(false);
    assert!(!quiet.requested());
    let raised = TerminationWatch::for_test(true);
    assert!(raised.requested());
}

/// The capability detection is cached: the same answer every call, so a
/// renderer gating a frame bracket never reads the environment twice.
#[test]
fn test_caps_detect_cached() {
    let first = TerminalCaps::detect();
    let second = TerminalCaps::detect();
    assert_eq!(first, second);
}

/// The synchronized-output decision over the identifying variables: the
/// recognized terminals each answer true, an unknown one answers false, and
/// a multiplexer overrides the inner terminal.
#[test]
fn test_sync_decision_known() {
    let known = [
        "WezTerm",
        "ghostty",
        "iTerm.app",
        "vscode",
        "kitty",
        "alacritty",
    ];
    for name in known {
        assert!(
            caps_for(Some(String::from(name)), None, None, None, None,).sync_output,
            "{name} recognized"
        );
    }
    assert!(
        !caps_for(Some(String::from("unknown-term")), None, None, None, None).sync_output,
        "an unknown terminal reports no sync"
    );
    assert!(
        !caps_for(None, None, None, None, None).sync_output,
        "no terminal reports no sync"
    );
    assert!(
        !caps_for(
            Some(String::from("WezTerm")),
            Some(String::from("1")),
            None,
            None,
            None
        )
        .sync_output,
        "a multiplexer overrides the inner terminal"
    );
}

/// The progress indicator is gated on the terminals that implement the
/// OSC 9;4 sequence, which is narrower than the sync set: Windows
/// Terminal owns its own progress surface and does not read it from the
/// stream.
#[test]
fn test_progress_gated() {
    assert!(
        caps_for(None, None, None, Some(String::from("WezTerm")), None).progress_indicator,
        "WezTerm reads the progress indicator"
    );
    assert!(
        !caps_for(None, None, Some(String::from("1")), None, None).progress_indicator,
        "Windows Terminal does not read the indicator from the stream"
    );
    assert!(
        !caps_for(None, None, None, None, None).progress_indicator,
        "an unknown terminal reports no progress"
    );
}

/// Whether a clipboard paste arrives as a run of keystrokes is a platform
/// fact: the console path needs it to decide whether to look ahead for the
/// next record. The test only proves the call returns; the value is a
/// fixed property of the platform, not the environment.
#[test]
fn test_paste_keystroke_fact_returns() {
    let _ = paste_arrives_as_keystrokes();
}

/// A terminal that delivers bytes has no record queue, so the wait answers no
/// and the event source keeps owning the wait. The take and the wait are one
/// decision: a host that takes records must also report the queue wait.
#[cfg(unix)]
#[test]
fn test_stream_wait_declines() {
    assert!(
        !wait_consumes_record(),
        "a byte stream is consumed by its event source, not by the reader"
    );
    assert!(
        !wait_console_input(Duration::ZERO).expect("a stream wait answers"),
        "a byte stream has no queue that reports readiness"
    );
}

/// The control-letter classifier is the decision the take rides on, so every
/// shape it must reject and the one shape it accepts are pinned here. A key
/// the event source can resolve on its own must answer none, or the take
/// would steal it; a control byte with no virtual key must answer its letter,
/// or the chord would be lost.
#[test]
fn test_control_letter_classifies_record() {
    const KEY_EVENT: u16 = 0x0001;
    const OTHER_EVENT: u16 = 0x0002;
    // The accepted shape: a key press whose whole content is a control byte.
    assert_eq!(control_letter(KEY_EVENT, true, 0, 0x0F, 1), Some('o'));
    assert_eq!(control_letter(KEY_EVENT, true, 0, 0x01, 1), Some('a'));
    assert_eq!(control_letter(KEY_EVENT, true, 0, 0x1A, 1), Some('z'));
    // An empty queue, a non-key record, a release, and a record with a
    // virtual key are all the event source's business.
    assert_eq!(
        control_letter(KEY_EVENT, true, 0, 0x0F, 0),
        None,
        "an empty queue is none"
    );
    assert_eq!(
        control_letter(OTHER_EVENT, true, 0, 0x0F, 1),
        None,
        "a non-key record is none"
    );
    assert_eq!(
        control_letter(KEY_EVENT, false, 0, 0x0F, 1),
        None,
        "a release is none"
    );
    assert_eq!(
        control_letter(KEY_EVENT, true, 0x4F, 0x0F, 1),
        None,
        "a record with a virtual key is none"
    );
    // The codes that carry their own virtual keys are resolved already.
    for code in [0x08, 0x09, 0x0A, 0x0D, 0x1B] {
        assert_eq!(
            control_letter(KEY_EVENT, true, 0, code, 1),
            None,
            "the code {code:#04x} carries its own virtual key"
        );
    }
    // A control byte above the range is not a chord the resolver drops.
    assert_eq!(
        control_letter(KEY_EVENT, true, 0, 0x20, 1),
        None,
        "a space is none"
    );
}
