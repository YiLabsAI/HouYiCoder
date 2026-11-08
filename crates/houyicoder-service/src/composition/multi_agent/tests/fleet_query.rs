//! The fleet queries: listing the parent's children and paging one child's
//! output.

use super::*;

/// A finished child lists from the parent log with its terminal state, and
/// the list holds running and finished children in one view.
#[tokio::test]
async fn test_fleet_list_reports_children() {
    let (runtime, _store, parent_sid) = runtime_with_text_child("answer one");
    let ctx = ToolCtx::new("c1").with_session(parent_sid);
    let outcome = runtime
        .spawn(
            &ctx,
            SpawnArgs::new("explore", "find the auth module", "find auth"),
        )
        .await
        .expect("spawn");
    let rows = runtime.fleet_list(parent_sid).await;
    let row = rows
        .iter()
        .find(|r| r.child_session_id == outcome.child_session_id)
        .expect("the finished child lists");
    assert_eq!(row.status, "completed");
    assert_eq!(row.subagent_type, "explore");
    assert_eq!(row.description, "find auth");
}

/// The fleet output pages the child's assistant text: the window honors
/// offset and limit, reports the total, and names the child's terminal
/// state and summary.
#[tokio::test]
async fn test_fleet_output_pages() {
    let (runtime, _store, parent_sid) = runtime_with_text_child("answer one");
    let ctx = ToolCtx::new("c1").with_session(parent_sid);
    let outcome = runtime
        .spawn(
            &ctx,
            SpawnArgs::new("explore", "find the auth module", "find auth"),
        )
        .await
        .expect("spawn");
    let child = outcome.child_session_id.clone();
    let view = runtime
        .fleet_output(parent_sid, &child, 0, 4)
        .await
        .expect("read");
    assert_eq!(view.status, "completed");
    assert_eq!(view.summary.as_deref(), Some("answer one"));
    assert_eq!(view.text, "answ");
    assert_eq!(view.total_chars, "answer one".chars().count());
    // A later window continues where the first ended.
    let tail = runtime
        .fleet_output(parent_sid, &child, 4, 100)
        .await
        .expect("read");
    assert_eq!(tail.text, "er one");
}

/// A child id the parent never delegated is refused, so the read stays
/// scoped to the caller's own fleet.
#[tokio::test]
async fn test_fleet_output_refuses_foreign() {
    let (runtime, _store, parent_sid) = runtime_with_text_child("answer");
    let err = runtime
        .fleet_output(parent_sid, "01ARZ3NDEKTSV4RRFFQ69G5FAV", 0, 10)
        .await
        .expect_err("a foreign id is refused");
    assert!(
        err.contains("not a child"),
        "the refusal names the scoping: {err}"
    );
}

/// The SpawnHandle port serves the fleet queries the task tools call: the
/// same runtime answers list_children and child_output through the trait,
/// so a tool dispatch and a direct call read one view.
#[tokio::test]
async fn test_port_serves_fleet_queries() {
    let (runtime, _store, parent_sid) = runtime_with_text_child("answer one");
    let ctx = ToolCtx::new("c1").with_session(parent_sid);
    let outcome = runtime
        .spawn(&ctx, SpawnArgs::new("explore", "find auth", "find auth"))
        .await
        .expect("spawn");
    let handle: &dyn SpawnHandle = &runtime;
    let rows = handle.list_children(parent_sid).await;
    assert!(
        rows.iter()
            .any(|r| r.child_session_id == outcome.child_session_id),
        "the port lists the child"
    );
    let view = handle
        .child_output(parent_sid, &outcome.child_session_id, 0, 100)
        .await
        .expect("the port reads the child");
    assert_eq!(view.text, "answer one");
}

/// An offset past the end reads an empty window, not an error: the caller
/// learns it has read to the end from the total, and the window is clamped.
#[tokio::test]
async fn test_fleet_output_clamps_offset() {
    let (runtime, _store, parent_sid) = runtime_with_text_child("answer one");
    let ctx = ToolCtx::new("c1").with_session(parent_sid);
    let outcome = runtime
        .spawn(&ctx, SpawnArgs::new("explore", "find auth", "find auth"))
        .await
        .expect("spawn");
    let view = runtime
        .fleet_output(parent_sid, &outcome.child_session_id, 999, 10)
        .await
        .expect("read");
    assert_eq!(view.text, "", "the window past the end is empty");
    assert_eq!(
        view.offset, 10,
        "the offset clamps to the end, so the caller sees where it stopped"
    );
    assert_eq!(
        view.total_chars, 10,
        "the total still tells the real length"
    );
}

/// A zero limit reads nothing but still reports status and total, so a
/// size-probing call costs no text.
#[tokio::test]
async fn test_fleet_output_zero_limit() {
    let (runtime, _store, parent_sid) = runtime_with_text_child("answer one");
    let ctx = ToolCtx::new("c1").with_session(parent_sid);
    let outcome = runtime
        .spawn(&ctx, SpawnArgs::new("explore", "find auth", "find auth"))
        .await
        .expect("spawn");
    let view = runtime
        .fleet_output(parent_sid, &outcome.child_session_id, 0, 0)
        .await
        .expect("read");
    assert_eq!(view.text, "");
    assert_eq!(view.status, "completed");
}

/// A resumed child lists once: the second spawn boundary resets the same
/// row instead of adding a duplicate id, and the turns field reflects the
/// return's own count rather than a permanent zero.
#[tokio::test]
async fn test_resumed_child_lists_once() {
    let (runtime, store, parent_sid) = runtime_with_text_child("answer");
    let ctx = ToolCtx::new("c1").with_session(parent_sid);
    let outcome = runtime
        .spawn(&ctx, SpawnArgs::new("explore", "first task", "first"))
        .await
        .expect("spawn");
    let child = outcome.child_session_id.clone();
    // A second delegation to the same child resumes it.
    let mut args = SpawnArgs::new("explore", "second task", "second");
    args.resume = Some(child.clone());
    let outcome2 = runtime.spawn(&ctx, args).await.expect("resume");
    assert_eq!(outcome2.child_session_id, child, "a resume keeps the id");
    let rows = runtime.fleet_list(parent_sid).await;
    let matches: Vec<_> = rows
        .iter()
        .filter(|r| r.child_session_id == child)
        .collect();
    assert_eq!(matches.len(), 1, "one row per child id, not one per spawn");
    assert_eq!(
        matches[0].description, "second",
        "the row carries the newest delegation's description"
    );
    // A stub child runs no tools, so its return's count is a measured zero:
    // the field mirrors the return rather than staying a dead zero by
    // construction.
    assert_eq!(matches[0].turn_count, 0);
    // A return that carries a real count reaches the row. Seeding the event
    // directly controls the figure without a tool-running child.
    let seeded = SessionLogEntry {
        id: EventId::new(),
        session: parent_sid,
        ts: 0,
        prev_hash: None,
        event: SessionEvent::SubagentReturn {
            child_session_id: child.clone(),
            status: "completed".into(),
            summary: "done".into(),
            result_ref: child.clone(),
            input_tokens: 0,
            output_tokens: 0,
            cache_read_input_tokens: 0,
            cache_write_input_tokens: 0,
            reasoning_tokens: 0,
            input_reported: false,
            cache_read_reported: false,
            cache_write_reported: false,
            tool_calls: 7,
            tool_failures: 0,
            duration_ms: 0,
            ledger_measured: true,
            orphaned: false,
        },
    };
    store.append(seeded).await.expect("seed a counted return");
    let rows = runtime.fleet_list(parent_sid).await;
    let row = rows
        .iter()
        .find(|r| r.child_session_id == child)
        .expect("the row lists");
    assert_eq!(row.turn_count, 7, "the return's count reaches the row");
}
