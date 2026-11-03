//! Reconciling delegation boundaries a host restart left open.

use super::*;

/// A spawn whose return never landed, because the host died mid-delegation, is
/// reconciled at the next run entry: the parent log gains the missing return
/// carrying the child's partial text, so the model does not see a dangling
/// child and can resume it by id. A child still running is left alone, so a
/// live delegation is not mistaken for an orphan.
#[tokio::test]
async fn test_orphan_reconciles_on_run() {
    let (provider, entered) = StallForeverProvider::new();
    let (runtime, store, parent_sid) = runtime_with(provider, None, ToolRegistry::new());
    let ctx = ToolCtx::new("c1").with_session(parent_sid);
    let mut args = SpawnArgs::new("explore", "live task", "live task");
    args.run_in_background = true;
    let live = runtime
        .spawn(&ctx, args)
        .await
        .expect("the live child starts");
    entered.await.expect("the live child enters its model call");

    // A dead child the host restarted before it returned: it has a spawn on
    // the parent log and a half-answer in its own log, but no return and no
    // live runner.
    let dead = SessionId::new();
    store
        .append(SessionLogEntry {
            id: EventId::new(),
            session: dead,
            ts: 0,
            prev_hash: None,
            event: SessionEvent::AssistantMessage {
                text: "half of the work".into(),
                thinking: None,
            },
        })
        .await
        .expect("seed dead child turn");
    store
        .append(SessionLogEntry {
            id: EventId::new(),
            session: parent_sid,
            ts: 0,
            prev_hash: None,
            event: SessionEvent::SubagentSpawn {
                child_session_id: dead.to_string(),
                subagent_type: "explore".into(),
                prompt_summary: "dead task".into(),
                isolation: "none".into(),
                policy: "delegate".into(),
                trigger_source: "model:c_dead".into(),
            },
        })
        .await
        .expect("seed dead child spawn");

    runtime.reconcile_orphans(parent_sid).await;

    let tail = store.trajectory_tail(parent_sid);
    let dead_returned = tail.iter().any(|e| {
        matches!(
            &e.event,
            SessionEvent::SubagentReturn {
                child_session_id,
                status,
                summary,
                ..
            } if child_session_id == &dead.to_string()
                && status == "interrupted"
                && summary.contains("orphaned by host restart")
                && summary.contains("half of the work")
        )
    });
    assert!(
        dead_returned,
        "the dead orphan gains a return with its partial text and resume id"
    );
    let live_returned = tail.iter().any(|e| {
        matches!(
            &e.event,
            SessionEvent::SubagentReturn { child_session_id, .. }
                if child_session_id == &live.child_session_id
        )
    });
    assert!(
        !live_returned,
        "a child still running is not reconciled as an orphan"
    );

    runtime.kill_child(&live.child_session_id);
    runtime.join_children().await;
}
