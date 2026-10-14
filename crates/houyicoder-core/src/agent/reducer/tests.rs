//! Tests for this module.

use super::*;

#[test]
fn test_smaller_filtered_kept() {
    let raw = "a".repeat(400);
    assert_eq!(never_worse(&raw, "ok"), "ok");
}

#[test]
fn test_filtered_bigger_falls_back() {
    // A pretty-printer that inflates a compact JSON falls back to raw.
    let raw = "{}";
    let filtered = "{\n  \"pretty\": true\n}";
    assert_eq!(never_worse(raw, filtered), raw);
}

#[test]
fn test_reducer_never_worse() {
    // The hot-path reducer never returns more than the raw: an inflation
    // is caught by the never-worse guard + reduced=false.
    let r = HotPathReducer;
    let raw = "{}";
    let out = r.reduce(
        raw,
        "bash",
        &ReduceCtx {
            raw: false,
            trust: TrustLevel::Untrusted,
        },
    );
    assert!(
        !out.text.len() > raw.len() || !out.reduced,
        "reducer never emits more than raw"
    );
    assert!(out.text.len() <= raw.len() || out.text.as_str() == raw);
}

#[test]
fn test_trust_gate_data_tag() {
    // Untrusted tool output is tagged data; trusted output is not.
    let r = HotPathReducer;
    let untrusted = r.reduce(
        "output",
        "bash",
        &ReduceCtx {
            raw: false,
            trust: TrustLevel::Untrusted,
        },
    );
    assert!(untrusted.data_tag, "untrusted output tagged DATA");
    let trusted = r.reduce(
        "output",
        "bash",
        &ReduceCtx {
            raw: false,
            trust: TrustLevel::Trusted,
        },
    );
    assert!(!trusted.data_tag, "trusted output not tagged");
}

#[test]
fn test_bash_strips_ansi_truncates() {
    let r = HotPathReducer;
    // ANSI color wrapped around text is stripped.
    let colored = "\x1b[32mgreen\x1b[0m";
    let out = r.reduce(
        colored,
        "bash",
        &ReduceCtx {
            raw: false,
            trust: TrustLevel::Untrusted,
        },
    );
    assert_eq!(out.text, "green", "ansi stripped");
    assert!(out.reduced, "stripped is smaller");

    // Large output truncates to head + tail + a marker.
    let big = "x".repeat(10_000);
    let out = r.reduce(
        &big,
        "bash",
        &ReduceCtx {
            raw: false,
            trust: TrustLevel::Untrusted,
        },
    );
    assert!(out.reduced, "large output reduced");
    assert!(out.text.contains("[truncated"), "truncation marker present");
    assert!(out.text.len() < big.len(), "truncated is smaller");
}

#[test]
fn test_unknown_tool_no_mangle() {
    // An unknown tool's output is not reduced (identity) so a new tool is
    // never silently mangled.
    let r = HotPathReducer;
    let out = r.reduce(
        "raw output",
        "future_tool",
        &ReduceCtx {
            raw: false,
            trust: TrustLevel::Untrusted,
        },
    );
    assert!(!out.reduced, "unknown tool not reduced");
    assert_eq!(out.text, "raw output");
}

#[test]
fn test_raw_flag_skips_reduction() {
    let r = HotPathReducer;
    let colored = "\x1b[32mgreen\x1b[0m";
    let out = r.reduce(
        colored,
        "bash",
        &ReduceCtx {
            raw: true,
            trust: TrustLevel::Untrusted,
        },
    );
    assert!(!out.reduced, "raw flag skips reduction");
    assert_eq!(out.text, colored, "raw output verbatim");
}
