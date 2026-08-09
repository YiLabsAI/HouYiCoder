use super::*;
use houyicoder_protocol::frontend::status::StatusSnapshot;
use houyicoder_protocol::llm::Usage;

fn snap_with_data() -> StatusSnapshot {
    StatusSnapshot {
        model: "qwen3-test".into(),
        breaker_state: None,
        breaker_reason: None,
        breaker_cool_down_secs: None,
        cumulative_usage: Usage {
            input_tokens: 1000,
            output_tokens: 80,
            total_tokens: 1080,
            non_cached_input_tokens: 300,
            cache_read_input_tokens: 700,
            cache_write_input_tokens: 50,
            reasoning_tokens: 30,
        },
        last_input_tokens: 1000,
        context_window: 8000,
        tool_calls: 15,
        tool_success: 12,
        tool_errors: 3,
        meta: None,
        ..Default::default()
    }
}

#[test]
fn test_context_renders_data() {
    let s = render_context(&snap_with_data());
    assert!(s.contains("reasoning 30"), "{s}");
    assert!(s.contains("visible 50"), "{s}");
    assert!(s.contains("cache_read 700"), "{s}");
    assert!(s.contains("non_cached 300"), "{s}");
    assert!(s.contains("15 calls"), "{s}");
    assert!(s.contains("12 ok"), "{s}");
    assert!(s.contains("3 errored"), "{s}");
    assert!(s.contains("1000/8000"), "{s}");
    assert!(s.contains("12.5%"), "{s}");
}

#[test]
fn test_context_zero_stub() {
    let s = render_context(&StatusSnapshot {
        model: "test".into(),
        breaker_state: None,
        breaker_reason: None,
        breaker_cool_down_secs: None,
        cumulative_usage: Usage::default(),
        last_input_tokens: 0,
        context_window: 0,
        tool_calls: 0,
        tool_success: 0,
        tool_errors: 0,
        meta: None,
        ..Default::default()
    });
    assert!(s.contains("0 calls"), "{s}");
    assert!(s.contains("0 ok"), "{s}");
    assert!(s.contains("0 errored"), "{s}");
    assert!(s.contains("no provider window"), "{s}");
}

#[test]
fn test_tools_sorted_desc() {
    let tools = vec![
        houyicoder_protocol::frontend::tools::ToolEntry {
            name: "bash".into(),
            description: "run a shell command".into(),
        },
        houyicoder_protocol::frontend::tools::ToolEntry {
            name: "read".into(),
            description: "read a file\nsecond line ignored".into(),
        },
    ];
    let s = render_tools(&tools);
    assert!(s.contains("2 registered"), "{s}");
    let bi = s.find("bash").unwrap();
    let ri = s.find("read").unwrap();
    assert!(bi < ri, "sorted: bash before read");
    assert!(s.contains("read a file"), "{s}");
    assert!(!s.contains("second line ignored"), "{s}");
}

#[test]
fn test_tools_empty() {
    let s = render_tools(&[]);
    assert!(s.contains("none registered"), "{s}");
}

#[test]
fn test_sandbox_none_wired() {
    let s = render_sandbox(&snap_with_data(), "landlock");
    assert!(s.contains("landlock"), "{s}");
    assert!(s.contains("none wired"), "{s}");
}

#[test]
fn test_sandbox_open_reason() {
    let mut snap = snap_with_data();
    snap.breaker_state = Some("Open".to_string());
    snap.breaker_reason = Some("cpu budget exceeded".into());
    snap.breaker_cool_down_secs = Some(30);
    let s = render_sandbox(&snap, "landlock");
    assert!(s.contains("Open"), "{s}");
    assert!(s.contains("cpu budget exceeded"), "{s}");
    assert!(s.contains("30s remaining"), "{s}");
}

fn verdict_event(
    tool: &str,
    verdict: &str,
    scope: &str,
    call_id: &str,
) -> houyicoder_protocol::frontend::permission::PermissionDecisionEntry {
    houyicoder_protocol::frontend::permission::PermissionDecisionEntry {
        tool: tool.into(),
        verdict: verdict.into(),
        scope: scope.into(),
        call_id: call_id.into(),
    }
}

#[test]
fn test_permission_view_empty() {
    let s = render_permission_view(PermissionMode::Manual, &[], &[], true);
    assert!(s.contains("mode: manual"), "{s}");
    assert!(s.contains("ask before git operations: on"), "{s}");
    assert!(s.contains("rules: (none"), "{s}");
    assert!(s.contains("verdicts: (none"), "{s}");
}

#[test]
fn test_permission_view_with_verdicts() {
    let verdicts = vec![
        verdict_event("bash", "approved", "once", "c1"),
        verdict_event("edit", "denied", "session", "c2"),
    ];
    let s = render_permission_view(PermissionMode::Auto, &[], &verdicts, false);
    assert!(s.contains("mode: auto"), "{s}");
    assert!(s.contains("ask before git operations: off"), "{s}");
    assert!(s.contains("approved bash"), "{s}");
    assert!(s.contains("denied edit"), "{s}");
    assert!(s.contains("call_id:c1"), "{s}");
}

#[test]
fn test_verdict_skipped_on_surface() {
    // A permission verdict is durable (audit trail) but NOT projected
    // onto the interaction surface — transcript_from_frames skips the
    // acpx/context/permissions_decision notification. The user sees
    // verdicts only via /permissions view (which filters the trajectory).
    use houyicoder_protocol::acpx::{AcpxMethod, AcpxNotification};
    let frames = vec![crate::transcript::TranscriptFrame::Acpx(
        AcpxNotification::new(
            AcpxMethod::ContextPermissionDecision,
            serde_json::json!({
                "call_id": "c1",
                "tool": "bash",
                "verdict": "approved",
                "scope": "once",
            }),
        ),
    )];
    let lines = crate::transcript::transcript_from_frames(&frames);
    assert!(
        lines.is_empty(),
        "verdict must not appear on the interaction surface"
    );
}

/// memory_entries_from_wire maps wire summaries to pane rows: topic = key,
/// summary = "[source] description" (or "[source]" when empty). Pins the
/// mapping independent of the App plumbing that calls it.
#[test]
fn test_wire_to_pane_rows() {
    use houyicoder_protocol::frontend::memory::MemorySummaryEntry;
    let entries = vec![
        MemorySummaryEntry {
            key: "build-gate".into(),
            description: "make check must stay green".into(),
            source: "project".into(),
            scope: "project".into(),
            mtime_secs: 0,
        },
        MemorySummaryEntry {
            key: "bare".into(),
            description: String::new(),
            source: "user".into(),
            scope: "user".into(),
            mtime_secs: 0,
        },
    ];
    let rows = memory_entries_from_wire(&entries);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].topic, "build-gate");
    assert_eq!(rows[0].summary, "make check must stay green");
    assert_eq!(rows[0].scope, "project");
    assert_eq!(rows[0].source, "project");
    assert_eq!(rows[1].topic, "bare");
    assert!(rows[1].summary.is_empty(), "empty desc stays empty");
    assert_eq!(rows[1].scope, "user");
    assert_eq!(rows[1].source, "user");
}

/// render_memory_entry shows the source+key header + the description hook +
/// the body. Pins the /memory <key> show render.
#[test]
fn test_show_entry_renders() {
    use houyicoder_protocol::frontend::memory::MemoryDetail;
    let entry = MemoryDetail {
        key: "build-gate".into(),
        content: "make check must stay green".into(),
        source: "project".into(),
        description: "the build must pass".into(),
        mtime_secs: 0,
    };
    let s = render_memory_entry(&entry);
    assert!(s.contains("[project] build-gate"), "header: {s}");
    assert!(s.contains("the build must pass"), "description hook: {s}");
    assert!(s.contains("make check must stay green"), "body: {s}");
}

/// The redundant section renders when redundant calls are present, with the
/// human-readable kind name (same-message repeat / cross-turn context-loss
/// re-read), not the machine label. Pins the trajectory redundant surfacing.
#[test]
fn test_trajectory_renders_redundant() {
    use houyicoder_protocol::frontend::trajectory::{RedundantCallEntry, TrajectoryEntry};
    let entries: Vec<TrajectoryEntry> = Vec::new();
    let redundant = vec![
        RedundantCallEntry {
            tool: "read".into(),
            input_preview: "{\"file_path\":\"a.rs\"}".into(),
            kind: "same-batch".into(),
            gap: 0,
            last_seq: 7,
        },
        RedundantCallEntry {
            tool: "read".into(),
            input_preview: "{\"file_path\":\"b.rs\"}".into(),
            kind: "cross-batch".into(),
            gap: 4,
            last_seq: 12,
        },
    ];
    let out = crate::command::render::render_trajectory_wire(&entries, &redundant);
    assert!(out.contains("redundant calls: 2 flagged"), "{out}");
    assert!(out.contains("same-message repeat"), "{out}");
    assert!(out.contains("cross-turn context-loss re-read"), "{out}");
    assert!(out.contains("a.rs"), "tool + preview: {out}");
}

/// Both empty (entries + redundant) → the "no events" message. Covers the
/// empty-both early return.
#[test]
fn test_trajectory_empty_both() {
    let out = crate::command::render::render_trajectory_wire(&[], &[]);
    assert!(out.contains("no events"), "{out}");
}
