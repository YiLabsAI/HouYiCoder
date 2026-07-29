//! Isolate-stage tests: the runner under input isolation (paused from the
//! host queue mid-run). Uses the parent tests module's fixtures (Runner,
//! runner_with, SessionStore) via the glob import.
use super::*;

#[tokio::test]
async fn test_externalizes_large_tool_result() {
    // Isolate stage (PostToolUse): a tool result larger than the threshold
    // is externalized to the CAS at append time. The appended ToolResult
    // event carries a block_ref marker + preview, not the raw content; the
    // raw is in the CAS; the turn-group projection materializes it on
    // demand. This is the wiring that makes the 3-tier retention policy
    // fire on real tool outputs, not just simulated markers in tests.
    use houyicoder_context::{BlockHash, TurnEventKind};
    use houyicoder_protocol::llm::InputItem;
    let p = Arc::new(FakeProvider::text("done"));
    let runner = runner_with(p, ToolRegistry::new());
    let session = SessionId::new();
    // Append the matching ToolCall before each result so the pair invariant
    // (projection debug_assert) holds.
    for cid in ["c1", "c2"] {
        runner
            .store()
            .append(super::append::new_event(
                session,
                TurnEventKind::ToolCall {
                    call_id: cid.into(),
                    tool: "bash".into(),
                    input: serde_json::json!({}),
                },
            ))
            .await
            .unwrap();
    }
    let big = serde_json::json!({"data": "x".repeat(9000)});
    runner
        .append_tool_result(session, "c1".into(), "bash", big.clone(), 0)
        .await
        .unwrap();
    // The appended ToolResult event carries a block_ref marker, not the raw.
    let events = runner.store().replay(session).await.unwrap();
    let tr = events
        .iter()
        .find(|e| matches!(&e.kind, TurnEventKind::ToolResult { call_id, .. } if call_id == "c1"))
        .expect("tool result appended");
    let output = match &tr.kind {
        TurnEventKind::ToolResult { output, .. } => output,
        _ => unreachable!(),
    };
    let hash = output
        .get("block_ref")
        .and_then(|v| v.as_str())
        .expect("large output externalized to block_ref");
    assert!(output.get("data").is_none(), "raw content not in the event");
    assert!(output.get("preview").is_some(), "marker carries a preview");
    // The raw content is in the CAS, addressable by hash.
    let stored = runner
        .store()
        .backend()
        .block_get(&BlockHash(hash.to_string()))
        .await
        .unwrap();
    let restored: serde_json::Value = serde_json::from_slice(&stored).unwrap();
    assert_eq!(restored, big, "CAS stores the original output");
    // A small output (< threshold) stays raw — no externalization.
    runner
        .append_tool_result(session, "c2".into(), "", serde_json::json!({"ok": true}), 0)
        .await
        .unwrap();
    let events = runner.store().replay(session).await.unwrap();
    let small = events
        .iter()
        .find(|e| matches!(&e.kind, TurnEventKind::ToolResult { call_id, .. } if call_id == "c2"))
        .expect("small tool result appended");
    if let TurnEventKind::ToolResult { output, .. } = &small.kind {
        assert!(
            output.get("block_ref").is_none(),
            "small output stays raw, not externalized"
        );
        assert!(output.get("ok").is_some(), "small output content intact");
    }
    // The turn-group projection materializes the large result (age 0 =>
    // Materialize tier) so the model sees the real content.
    let items = super::turn_group::project_input_items(&events, Some(runner.store().backend()));
    let tr_item = items
        .iter()
        .find(|i| matches!(i, InputItem::ToolResult { call_id, .. } if call_id == "c1"))
        .expect("c1 result projected");
    if let InputItem::ToolResult { output, .. } = tr_item {
        assert_eq!(output, &big, "materialized output matches the original");
    }
}

#[tokio::test]
async fn test_isolate_reduces_large_output() {
    // With a tool-output reducer wired, the isolate stage reduces the preview
    // (strips ANSI for bash) + tags it as data. The raw stays in the CAS.
    use houyicoder_context::TurnEventKind;
    let p = Arc::new(FakeProvider::text("done"));
    let runner = runner_with(p, ToolRegistry::new())
        .with_reducer(Arc::new(crate::agent::reducer::HotPathReducer));
    let session = SessionId::new();
    runner
        .store()
        .append(super::append::new_event(
            session,
            TurnEventKind::ToolCall {
                call_id: "cr".into(),
                tool: "bash".into(),
                input: serde_json::json!({}),
            },
        ))
        .await
        .unwrap();
    // Large output (> ISOLATE threshold) carrying ANSI color codes.
    let big = serde_json::json!({"stdout": format!("\u{1b}[32m{}\u{1b}[0m", "x".repeat(9000))});
    runner
        .append_tool_result(session, "cr".into(), "bash", big, 0)
        .await
        .unwrap();
    let events = runner.store().replay(session).await.unwrap();
    let tr = events
        .iter()
        .find(|e| matches!(&e.kind, TurnEventKind::ToolResult { call_id, .. } if call_id == "cr"))
        .expect("tool result appended");
    let TurnEventKind::ToolResult { output, .. } = &tr.kind else {
        unreachable!()
    };
    // The marker carries a reduced (ansi-stripped) preview + a data_tag.
    assert!(output.get("block_ref").is_some(), "block_ref present");
    assert_eq!(
        output.get("data_tag").and_then(|v| v.as_bool()),
        Some(true),
        "reduced output tagged as data"
    );
    let preview = output
        .get("preview")
        .and_then(|v| v.as_str())
        .expect("preview present");
    assert!(
        !preview.contains("\u{1b}"),
        "ansi stripped from the preview"
    );
}
