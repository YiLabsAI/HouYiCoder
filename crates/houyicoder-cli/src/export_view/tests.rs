//! Unit tests for export data assembly: feeds synthetic events and asserts
//! aggregated tool records, token usage, and session checkpoints.
//! fault collection, checkpoint collection, the filename slug, and the
//! empty-log edge. The record layer's TurnUsage/ToolResult/HookSignal
//! work is invisible in the export without them.

use super::*;
use houyicoder_context::{
    CheckpointId, EventId, HookErrorKind, HookEventKind, HookVerdictKind, SessionEvent, SessionId,
    SessionLogEntry,
};
use houyicoder_tui::redaction::redact;

fn ev(ts: u64, kind: SessionEvent) -> SessionLogEntry {
    SessionLogEntry {
        id: EventId::new(),
        session: SessionId::new(),
        ts,
        prev_hash: None,
        event: kind,
    }
}

fn usage_event(ts: u64, model: &str, input: u64, output: u64) -> SessionLogEntry {
    ev(
        ts,
        SessionEvent::TurnUsage {
            turn: 1,
            call_in_turn: 1,
            input_tokens: input,
            output_tokens: output,
            cache_read_input_tokens: 0,
            cache_write_input_tokens: 0,
            reasoning_tokens: 0,
            model: model.to_string(),
            recovery: false,
            effort: None,
        },
    )
}

#[test]
fn test_export_aggregates_tool_stats() {
    // Two bash ToolCalls; one result errors, one succeeds. Fail + latency
    // attribute to bash via the call_id correlation.
    let events = vec![
        ev(
            100,
            SessionEvent::ToolCall {
                call_id: "c1".into(),
                tool: "bash".into(),
                input: serde_json::json!({}),
            },
        ),
        ev(
            110,
            SessionEvent::ToolCall {
                call_id: "c2".into(),
                tool: "bash".into(),
                input: serde_json::json!({}),
            },
        ),
        ev(
            120,
            SessionEvent::ToolResult {
                call_id: "c1".into(),
                output: serde_json::json!({"error": "boom"}),
                duration_ms: 300,
            },
        ),
        ev(
            130,
            SessionEvent::ToolResult {
                call_id: "c2".into(),
                output: serde_json::json!({"ok": 1}),
                duration_ms: 120,
            },
        ),
    ];
    let data = ExportDataBuilder::new(&events, "s", "m").build();
    assert_eq!(data.tool_stats.len(), 1, "one tool (bash)");
    let bash = &data.tool_stats[0];
    assert_eq!(bash.tool, "bash");
    assert_eq!(bash.calls, 2);
    assert_eq!(bash.fail, 1);
    assert_eq!(bash.latency_ms_total, 420);
    assert_eq!(bash.latency_ms_max, 300);
}

#[test]
fn test_export_usage_breakdown() {
    let events = vec![
        usage_event(100, "haiku", 1000, 200),
        usage_event(110, "haiku", 500, 100),
        usage_event(120, "sonnet", 3000, 400),
    ];
    let data = ExportDataBuilder::new(&events, "s", "m").build();
    assert_eq!(data.usage.total.input_tokens, 4500);
    assert_eq!(data.usage.total.output_tokens, 700);
    // per-model in first-seen order: haiku then sonnet.
    assert_eq!(data.usage.per_model.len(), 2);
    assert_eq!(data.usage.per_model[0].model, "haiku");
    assert_eq!(data.usage.per_model[0].counts.input_tokens, 1500);
    assert_eq!(data.usage.per_model[1].model, "sonnet");
    assert_eq!(data.usage.per_model[1].counts.input_tokens, 3000);
}

#[test]
fn test_export_collects_faults() {
    // A hook fault (error = Some) lands in errors; a bare-Allow
    // HookSignal does NOT (the record layer skips Allow, and the export
    // only collects error-bearing signals). TurnAborted also lands.
    let hook_fault = ev(
        100,
        SessionEvent::HookSignal {
            event: HookEventKind::default(),
            verdict: HookVerdictKind::Deny,
            error: Some(HookErrorKind::Timeout),
            reason: "timed out".into(),
            hook_name: "guard".into(),
            tool_name: Some("bash".into()),
            triggered_event: None,
            turn: Some(1),
            call_in_turn: Some(1),
        },
    );
    let aborted = ev(
        110,
        SessionEvent::TurnAborted {
            reason: "crashed".into(),
        },
    );
    let data = ExportDataBuilder::new(&[hook_fault, aborted], "s", "m").build();
    assert_eq!(data.errors.len(), 2);
    assert_eq!(data.errors[0].kind, "hook_error");
    assert_eq!(data.errors[0].hook_name.as_deref(), Some("guard"));
    assert_eq!(data.errors[0].tool_name.as_deref(), Some("bash"));
    assert_eq!(data.errors[1].kind, "turn_aborted");
    assert_eq!(data.errors[1].reason, "crashed");
}

#[test]
fn test_export_collects_checkpoints() {
    let events = vec![
        ev(
            100,
            SessionEvent::CompactionBoundary {
                checkpoint: CheckpointId::new(),
                pre_tokens: 0,
                post_tokens: 0,
            },
        ),
        ev(
            110,
            SessionEvent::Summary {
                text: "folded T1-T3".into(),
            },
        ),
    ];
    let data = ExportDataBuilder::new(&events, "s", "m").build();
    assert_eq!(data.checkpoints.len(), 2);
    assert!(data.checkpoints[0].checkpoint.is_some());
    assert_eq!(data.checkpoints[1].summary.as_deref(), Some("folded T1-T3"));
}

#[test]
fn test_export_filename_slug() {
    // 2026-08-03 20:25 UTC = epoch ms 1785788700000. Verify the slug
    // formatter produces the expected stamp + the prompt slugifier trims
    // punctuation. format_ts_slug is UTC + deterministic.
    assert_eq!(format_ts_slug(1_785_788_700_000), "2026-08-03-2025");
    let events = vec![ev(
        100,
        SessionEvent::UserInput {
            text: "Fix the bug!! (urgent)".into(),
        },
    )];
    assert_eq!(first_prompt_slug(&events), "fix-the-bug-urgent");
}

#[test]
fn test_export_empty_is_empty() {
    let data = ExportDataBuilder::new(&[], "s", "m").build();
    assert_eq!(data.started_at, 0);
    assert!(data.tool_stats.is_empty());
    assert_eq!(data.usage.total.input_tokens, 0);
    assert!(data.errors.is_empty());
    assert_eq!(
        first_prompt_slug(&[]),
        "session",
        "no prompt => slug session"
    );
    assert_eq!(
        format_ts_slug(0),
        "1970-01-01-0000",
        "epoch zero => 1970 slug"
    );
}

#[test]
fn test_export_redacts_secrets_trajectory() {
    // A tool result whose output carries an OpenAI key. The export
    // serializes the full trajectory then redacts on the share boundary,
    // so the shared JSON file must NOT carry the secret — even though
    // the durable log the export reads from stays full-fidelity.
    let secret = "sk-abcd1234efgh5678ijkl9012mnop3456qrst";
    let events = vec![ev(
        100,
        SessionEvent::ToolResult {
            call_id: "c1".into(),
            output: serde_json::json!({"token": secret}),
            duration_ms: 0,
        },
    )];
    let data = ExportDataBuilder::new(&events, "s", "m").build();
    let raw = serde_json::to_string(&data).unwrap();
    let redacted = redact(&raw);
    assert!(
        redacted.contains("[REDACTED"),
        "the secret must be redacted in the export json, got: {redacted}"
    );
    assert!(
        !redacted.contains(secret),
        "the raw secret must not land in the export file, got: {redacted}"
    );
}
