//! Isolate-stage tests: the runner under input isolation (paused from the
//! host queue mid-run). Uses the parent tests module's fixtures (Runner,
//! runner_with, SessionStore) via the glob import.
use super::append::ISOLATE_LARGE_OUTPUT_BYTES;
use super::append::new_event;
use super::turn_group::assemble_model_input;
use super::*;
use crate::agent::reducer::HotPathReducer;
use houyicoder_context::BlockHash;
use houyicoder_context::SessionEvent;
use houyicoder_protocol::block_ref::{BLOCK_REF_KEY, CONTAINS_KEY, block_addresses};
use houyicoder_protocol::llm::InputItem;
use houyicoder_protocol::tool::user_shell_call_id;
use serde_json::Value;

#[tokio::test]
async fn test_externalizes_large_tool_result() {
    // Isolate stage (PostToolUse): a tool result larger than the threshold
    // is externalized to the CAS at append time. The appended ToolResult
    // event carries a block_ref marker + preview, not the raw content; the
    // raw is in the CAS; the turn-group assembly materializes it on
    // demand. This is the wiring that makes the 3-tier retention policy
    // fire on real tool outputs, not just simulated markers in tests.
    let p = Arc::new(FakeProvider::text("done"));
    let runner = runner_with(p, ToolRegistry::new());
    let session = SessionId::new();
    // Append the matching ToolCall before each result so the pair invariant
    // (assembly debug_assert) holds.
    for cid in ["c1", "c2"] {
        runner
            .store()
            .append(new_event(
                session,
                SessionEvent::ToolCall {
                    call_id: cid.into(),
                    tool: "bash".into(),
                    input: serde_json::json!({}),
                },
            ))
            .await
            .unwrap();
    }
    let big_payload = "line with \"quotes\" and \\ backslash\nnext line\n".repeat(1000);
    let big = serde_json::json!({"data": big_payload});
    runner
        .append_tool_result(session, "c1".into(), "bash", big.clone(), 0)
        .await
        .unwrap();
    // The appended ToolResult event carries a field-level marker: the data
    // field is externalized (it is the largest top-level string field), the
    // envelope's other keys stay inline. The raw string is in the CAS.
    let events = runner.store().replay(session).await.unwrap();
    let tr = events
        .iter()
        .find(|e| matches!(&e.event, SessionEvent::ToolResult { call_id, .. } if call_id == "c1"))
        .expect("tool result appended");
    let output = match &tr.event {
        SessionEvent::ToolResult { output, .. } => output,
        _ => unreachable!(),
    };
    let hash = output["data"]
        .get("block_ref")
        .and_then(|v| v.as_str())
        .expect("large field externalized to block_ref");
    assert!(
        output["data"].as_str().is_none(),
        "raw field content not in the event"
    );
    assert!(
        output["data"].get("preview").is_some(),
        "field marker carries a preview"
    );
    // The raw content is in the CAS, addressable by hash.
    let stored = runner
        .store()
        .backend()
        .block_get(&BlockHash(hash.to_string()))
        .await
        .unwrap();
    let restored: serde_json::Value = serde_json::from_slice(&stored).unwrap();
    assert_eq!(
        restored,
        serde_json::json!(big_payload),
        "CAS stores the field's string value byte-exact"
    );
    // A small output (< threshold) stays raw — no externalization.
    runner
        .append_tool_result(session, "c2".into(), "", serde_json::json!({"ok": true}), 0)
        .await
        .unwrap();
    let events = runner.store().replay(session).await.unwrap();
    let small = events
        .iter()
        .find(|e| matches!(&e.event, SessionEvent::ToolResult { call_id, .. } if call_id == "c2"))
        .expect("small tool result appended");
    if let SessionEvent::ToolResult { output, .. } = &small.event {
        assert!(
            output.get("block_ref").is_none(),
            "small output stays raw, not externalized"
        );
        assert!(output.get("ok").is_some(), "small output content intact");
    }
    // The turn-group assembly materializes the large result (age 0 =>
    // Materialize tier) so the model sees the real content.
    let items = assemble_model_input(&events, Some(runner.store().backend()));
    let tr_item = items
        .iter()
        .find(|i| matches!(i, InputItem::ToolResult { call_id, .. } if call_id == "c1"))
        .expect("the isolated call's result assembled");
    if let InputItem::ToolResult { output, .. } = tr_item {
        assert_eq!(output, &big, "materialized output matches the original");
    }
}

#[tokio::test]
async fn test_isolate_reduces_large_output() {
    // With a tool-output reducer wired, the isolate stage reduces the preview
    // (strips ANSI for bash) + tags it as data. The raw stays in the CAS.
    let p = Arc::new(FakeProvider::text("done"));
    let runner = runner_with(p, ToolRegistry::new()).with_reducer(Arc::new(HotPathReducer));
    let session = SessionId::new();
    runner
        .store()
        .append(new_event(
            session,
            SessionEvent::ToolCall {
                call_id: "cr".into(),
                tool: "bash".into(),
                input: serde_json::json!({}),
            },
        ))
        .await
        .unwrap();
    // Large output (> ISOLATE threshold) carrying ANSI color codes.
    let big = serde_json::json!({"stdout": format!("\u{1b}[32m{}\u{1b}[0m", "x".repeat(33_000))});
    runner
        .append_tool_result(session, "cr".into(), "bash", big, 0)
        .await
        .unwrap();
    let events = runner.store().replay(session).await.unwrap();
    let tr = events
        .iter()
        .find(|e| matches!(&e.event, SessionEvent::ToolResult { call_id, .. } if call_id == "cr"))
        .expect("tool result appended");
    let SessionEvent::ToolResult { output, .. } = &tr.event else {
        unreachable!()
    };
    // The field marker carries a reduced (ansi-stripped) preview + a
    // data_tag. The stdout field (the largest string field) was externalized
    // with the envelope's other keys - none here - intact.
    assert!(
        output["stdout"].get("block_ref").is_some(),
        "block_ref present"
    );
    assert_eq!(
        output["stdout"].get("data_tag").and_then(|v| v.as_bool()),
        Some(true),
        "reduced output tagged as data"
    );
    let preview = output["stdout"]
        .get("preview")
        .and_then(|v| v.as_str())
        .expect("preview present");
    assert!(
        !preview.contains("\u{1b}"),
        "ansi stripped from the preview"
    );
}

/// A blob result (no top-level string field - a grep-style filenames array)
/// keeps the whole-output externalize: the top-level block_ref marker, the
/// envelope replaced. Field-level only fires when a string field can buy
/// enough headroom; an array payload cannot, and grep's existing behavior
/// (whole-marker in the event, full restore on materialize) must not drift.
#[tokio::test]
async fn test_blob_result_whole_externalize() {
    let p = Arc::new(FakeProvider::text("done"));
    let runner = runner_with(p, ToolRegistry::new());
    let session = SessionId::new();
    runner
        .store()
        .append(new_event(
            session,
            SessionEvent::ToolCall {
                call_id: "cg".into(),
                tool: "grep".into(),
                input: serde_json::json!({}),
            },
        ))
        .await
        .unwrap();
    let files: Vec<String> = (0..1000)
        .map(|i| format!("src/deeply/nested/path/to/file_{i:04}.rs"))
        .collect();
    let big = serde_json::json!({"filenames": files});
    runner
        .append_tool_result(session, "cg".into(), "grep", big.clone(), 0)
        .await
        .unwrap();
    let events = runner.store().replay(session).await.unwrap();
    let tr = events
        .iter()
        .find(|e| matches!(&e.event, SessionEvent::ToolResult { call_id, .. } if call_id == "cg"))
        .expect("tool result appended");
    let SessionEvent::ToolResult { output, .. } = &tr.event else {
        unreachable!()
    };
    assert!(
        output.get("block_ref").is_some(),
        "blob result whole-externalizes: top-level block_ref"
    );
    assert!(
        output.get("filenames").is_none(),
        "raw array not in the event"
    );
}

/// A whole-output externalize replaces the object that named other blocks, so
/// the event has to keep those addresses reachable. The reclaim pass walks
/// markers in the event and never opens the stored bytes; an address only
/// inside the stored output would read as unreferenced and be released while
/// the log still needs it.
#[tokio::test]
async fn test_whole_externalize_keeps_refs() {
    let p = Arc::new(FakeProvider::text("done"));
    let runner = runner_with(p, ToolRegistry::new());
    let session = SessionId::new();
    runner
        .store()
        .append(new_event(
            session,
            SessionEvent::ToolCall {
                call_id: "cb".into(),
                tool: "bash".into(),
                input: serde_json::json!({}),
            },
        ))
        .await
        .unwrap();
    let raw_stream = "a".repeat(64);
    let matches: Vec<Value> = (0..600)
        .map(|_| serde_json::json!({ "raw_capture": { "stdout": { BLOCK_REF_KEY: raw_stream } } }))
        .collect();
    let big = serde_json::json!({ "matches": matches });
    runner
        .append_tool_result(session, "cb".into(), "bash", big, 0)
        .await
        .unwrap();
    let events = runner.store().replay(session).await.unwrap();
    let tr = events
        .iter()
        .find(|e| matches!(&e.event, SessionEvent::ToolResult { call_id, .. } if call_id == "cb"))
        .expect("tool result appended");
    let SessionEvent::ToolResult { output, .. } = &tr.event else {
        unreachable!()
    };
    let stored = output
        .get(BLOCK_REF_KEY)
        .and_then(|v| v.as_str())
        .expect("a blob over the threshold is stored whole");
    let addresses = block_addresses(output);
    assert!(
        addresses.contains(&stored),
        "the event names the block it stored"
    );
    assert!(
        addresses.contains(&raw_stream.as_str()),
        "the event still names the blocks the stored output named: {addresses:?}"
    );
    let restated = output[CONTAINS_KEY]
        .as_array()
        .expect("the marker restates the addresses");
    assert_eq!(
        restated.len(),
        1,
        "one address restated once, however often the output named it"
    );
    assert!(
        serde_json::to_vec(output).unwrap().len() < ISOLATE_LARGE_OUTPUT_BYTES,
        "the restatement keeps the event inside the size the isolation bound exists for"
    );
}

/// An output that names more distinct blocks than the restatement allows stays
/// inline. The restatement cannot drop an address without the reclaim pass
/// releasing a block the log still needs, so past the limit the copy is not
/// made at all.
#[tokio::test]
async fn test_many_refs_stay_inline() {
    let p = Arc::new(FakeProvider::text("done"));
    let runner = runner_with(p, ToolRegistry::new());
    let session = SessionId::new();
    runner
        .store()
        .append(new_event(
            session,
            SessionEvent::ToolCall {
                call_id: "cm".into(),
                tool: "bash".into(),
                input: serde_json::json!({}),
            },
        ))
        .await
        .unwrap();
    let matches: Vec<Value> = (0..200)
        .map(|i| {
            let hash = format!("{i:064}");
            serde_json::json!({ "raw_capture": { "stdout": { BLOCK_REF_KEY: hash } } })
        })
        .collect();
    let big = serde_json::json!({ "matches": matches });
    runner
        .append_tool_result(session, "cm".into(), "bash", big, 0)
        .await
        .unwrap();
    let events = runner.store().replay(session).await.unwrap();
    let tr = events
        .iter()
        .find(|e| matches!(&e.event, SessionEvent::ToolResult { call_id, .. } if call_id == "cm"))
        .expect("tool result appended");
    let SessionEvent::ToolResult { output, .. } = &tr.event else {
        unreachable!()
    };
    assert!(
        output.get(BLOCK_REF_KEY).is_none(),
        "past the restatement limit the output stays inline"
    );
    assert_eq!(
        block_addresses(output).len(),
        200,
        "the walk reaches every reference the inline output carries"
    );
}

/// A direct-shell result (a command the user typed at the dollar prompt) is
/// already bounded by the bash tool and is meant to expand inline in the
/// transcript, so the isolate stage leaves it alone: the appended ToolResult
/// carries the full bounded body, with no block_ref marker, even past the
/// threshold that would externalize a model-issued result.
#[tokio::test]
async fn test_user_shell_skips_isolate() {
    let p = Arc::new(FakeProvider::text("done"));
    let runner = runner_with(p, ToolRegistry::new());
    let session = SessionId::new();
    let cid = user_shell_call_id("01ARZ3NDEKTSV4RRFFQ69G5FAV");
    runner
        .store()
        .append(new_event(
            session,
            SessionEvent::ToolCall {
                call_id: cid.clone(),
                tool: "bash".into(),
                input: serde_json::json!({}),
            },
        ))
        .await
        .unwrap();
    let big = serde_json::json!({"stdout": "x".repeat(ISOLATE_LARGE_OUTPUT_BYTES + 1000)});
    runner
        .append_tool_result(session, cid.clone(), "bash", big.clone(), 0)
        .await
        .unwrap();
    let events = runner.store().replay(session).await.unwrap();
    let tr = events
        .iter()
        .find(|e| matches!(&e.event, SessionEvent::ToolResult { call_id, .. } if call_id == &cid))
        .expect("tool result appended");
    let SessionEvent::ToolResult { output, .. } = &tr.event else {
        unreachable!()
    };
    assert_eq!(
        output, &big,
        "a direct-shell result stays inline, not externalized to a marker"
    );
    assert!(
        output["stdout"].get("block_ref").is_none(),
        "no block_ref marker on a direct-shell result"
    );
}
