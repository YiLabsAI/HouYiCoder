//! The spinner's own rendering rules: the suffix and its width gates, the
//! shimmer spans, and the stall gradient's thresholds.

use std::time::{Duration, Instant};

use super::*;

#[test]
fn test_suffix_k() {
    let s = spinner_suffix(Duration::from_secs(494), 13400);
    assert!(s.contains("8m 14s"));
    assert!(s.contains("↓ 13.4k tokens"));
}

#[test]
fn test_suffix_secs() {
    let s = spinner_suffix(Duration::from_secs(5), 80);
    assert!(s.contains("5s"));
    assert!(s.contains("↓ 80 tokens"));
}

#[test]
fn test_suffix_gated_narrow() {
    let s = spinner_suffix_gated(Duration::from_secs(5), 80, 25);
    assert!(s.contains("5s"));
    assert!(!s.contains("tokens"));
}

#[test]
fn test_suffix_gated_tiny() {
    let s = spinner_suffix_gated(Duration::from_secs(5), 80, 15);
    assert!(s.is_empty());
}

#[test]
fn test_shimmers() {
    let line = spinner_line(
        "✻ Thinking… (5s · ↓ 80 tokens)",
        Duration::from_millis(200),
        0.0,
        false,
    );
    assert!(line.spans.len() >= 3);
}

#[test]
fn test_stall_gradient_red() {
    let line = spinner_line("✻ Thinking… (5s)", Duration::from_secs(4), 1.0, false);
    assert!(!line.spans.is_empty());
    let glyph_span = &line.spans[0];
    let verb_span = &line.spans[2];
    assert!(matches!(glyph_span.style.fg, Some(Color::Rgb(255, _, _))));
    assert_eq!(verb_span.style.fg, glyph_span.style.fg);
    assert!(
        line.spans.iter().all(|s| s.style.fg != Some(Color::Cyan)),
        "a stalled spinner suppresses the cyan shimmer"
    );
}

#[test]
fn test_tool_pulse_whole_verb() {
    let line = spinner_line(
        "✻ Working… (5s · ↓ 80 tokens)",
        Duration::from_millis(300),
        0.0,
        true,
    );
    assert!(
        line.spans.iter().all(|s| s.style.fg != Some(Color::Cyan)),
        "tool-active breathing replaces the sliding shimmer window"
    );
    let tail = line.spans.last().expect("tail span");
    assert_eq!(tail.style.fg, Some(Color::DarkGray));
}

#[test]
fn test_tool_pulse_bounded() {
    let pulse = tool_pulse_opacity(Duration::from_millis(0), true);
    assert!(pulse >= 0.0);
}

/// The breathing sine reaches its trough once per cycle, where the pulse is
/// near zero. The row's state while a tool runs is "breathing", not "shimmer",
/// so no frame of a whole cycle may take the cyan highlight.
#[test]
fn test_tool_pulse_never_shimmers() {
    for millis in (0..1250).step_by(5) {
        let line = spinner_line(
            "✻ Working… (5s · ↓ 80 tokens)",
            Duration::from_millis(millis),
            0.0,
            true,
        );
        assert!(
            line.spans.iter().all(|s| s.style.fg != Some(Color::Cyan)),
            "tool-active frame at {millis}ms took the cyan shimmer"
        );
    }
}

/// At the trough the verb is the dim base colour, not a brighter pulse: the
/// breathing fades back to the gray it starts from rather than stopping at a
/// brighter floor.
#[test]
fn test_tool_pulse_floor_dim() {
    // Three quarters of the cycle puts the sine at its minimum.
    let line = spinner_line(
        "✻ Working… (5s · ↓ 80 tokens)",
        Duration::from_millis(938),
        0.0,
        true,
    );
    let verb = &line.spans[2];
    assert_eq!(
        verb.style.fg,
        Some(Color::Rgb(88, 88, 88)),
        "the trough is the dim base gray"
    );
}

#[test]
fn test_tool_pulse_off() {
    let pulse = tool_pulse_opacity(Duration::from_millis(500), false);
    assert_eq!(pulse, 0.0);
}

#[test]
fn test_stall_intensity_none() {
    assert_eq!(stall_intensity(None), 0.0);
}

/// Waiting for a reply to start is silence, not a stall. A large context
/// spends several seconds before its first token, and the row that turns red
/// on that wait shows the user an error on a turn that is going exactly as
/// expected. The default threshold must sit past a slow first token.
#[test]
fn test_stall_clears_token_wait() {
    let t = Instant::now() - Duration::from_secs(5);
    assert_eq!(
        stall_intensity(Some(t)),
        0.0,
        "a five-second wait for the first token is healthy"
    );
}

/// Reasoning is held calm for longer than ordinary text, and the two
/// thresholds stay ordered now that the default one moved out past a slow
/// first token: a future tweak cannot silently collapse them.
#[test]
fn test_stall_reasoning_threshold_higher() {
    // At the point text turns fully red, reasoning is still calm. This is the
    // one ordering the two thresholds exist to express: reasoning is the mode
    // where silence is most expected, so it must never redden first.
    let t = Instant::now() - Duration::from_secs(20);
    assert!(
        stall_intensity(Some(t)) > 0.0,
        "twenty seconds of silence is a stall for text"
    );
    assert_eq!(
        stall_intensity_reasoning(Some(t)),
        0.0,
        "reasoning is still held calm at that point"
    );
}

#[test]
fn test_stall_reasoning_long_think() {
    // A long delta exceeds even the reasoning threshold: a true hang still
    // trips the gradient, so long thinking is calm but a genuine stall is not
    // hidden behind the longer window.
    let t = Instant::now() - Duration::from_secs(30);
    assert!(
        stall_intensity_reasoning(Some(t)) > 0.0,
        "thirty seconds exceeds the reasoning threshold"
    );
}

mod activity;
