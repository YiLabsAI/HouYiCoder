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

#[test]
fn test_tool_pulse_off() {
    let pulse = tool_pulse_opacity(Duration::from_millis(500), false);
    assert_eq!(pulse, 0.0);
}

#[test]
fn test_stall_intensity_none() {
    assert_eq!(stall_intensity(None), 0.0);
}

#[test]
fn test_stall_reasoning_threshold_higher() {
    // Reasoning has a sparse token cadence; a 5s delta is healthy, so the
    // reasoning threshold (10s) must not fire red where the default (3s)
    // would. Pin both so a future tweak can't silently collapse them.
    let t = Instant::now() - Duration::from_secs(5);
    assert_eq!(stall_intensity_reasoning(Some(t)), 0.0);
    assert!(
        stall_intensity(Some(t)) > 0.0,
        "default 3s threshold fires at 5s"
    );
}

#[test]
fn test_stall_reasoning_long_think() {
    // A 12s delta exceeds even the reasoning threshold (10s): a true
    // hang still trips the gradient, so long thinking is calm but a
    // genuine stall is not hidden.
    let t = Instant::now() - Duration::from_secs(12);
    assert!(
        stall_intensity_reasoning(Some(t)) > 0.0,
        "12s exceeds reasoning 10s threshold"
    );
}

mod activity;
