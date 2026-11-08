//! When an orphaned delegation's completion notice reaches the model.
//!
//! A notice reports a background ending, whose call result is only the launch
//! acknowledgement. These tests cover the endings that owe a notice, the
//! endings that do not, and the rule that orders a notice against the run it
//! reports.

use super::*;
use houyicoder_core::agent::multi_agent::{OrphanedChild, RecordOwed, record_orphaned_child};

/// A background child's ending reaches the model only through the completion
/// notice, which the parent's next turn boundary writes. A host that stopped
/// between the child's return and that boundary left the model with a launch
/// acknowledgement and no ending; the scan writes the notice the parent never
/// drained.
#[tokio::test]
async fn test_orphan_restores_missed_notice() {
    let (runtime, store, parent_sid) = runtime_with(
        Arc::new(FakeProvider::text("kept")),
        None,
        ToolRegistry::new(),
    );
    let child = SessionId::new();
    for event in [
        SessionEvent::ToolCall {
            call_id: "c_bg".into(),
            tool: "agent".into(),
            input: serde_json::json!({ "prompt": "background task" }),
        },
        // The launch acknowledgement lands before the child ends, which is
        // what makes this a background delegation.
        SessionEvent::ToolResult {
            call_id: "c_bg".into(),
            output: serde_json::json!({ "agentId": child.to_string() }),
            duration_ms: 0,
        },
        SessionEvent::SubagentSpawn {
            child_session_id: child.to_string(),
            subagent_type: "explore".into(),
            prompt_summary: "background task".into(),
            isolation: "none".into(),
            policy: "delegate".into(),
            trigger_source: "model:c_bg".into(),
            run_in_background: true,
        },
        return_event(parent_sid, &child).event,
    ] {
        store
            .append(entry(parent_sid, event))
            .await
            .expect("seed the finished background child");
    }

    runtime.reconcile_orphans(parent_sid).await;

    let log = store.replay(parent_sid).await.expect("read the parent log");
    let notices = log
        .iter()
        .filter(|e| {
            matches!(
                &e.event,
                SessionEvent::NotificationInjected { child_session_id, topic, .. }
                    if child_session_id == &child.to_string() && topic == "completion"
            )
        })
        .count();
    assert_eq!(
        notices, 1,
        "the model reads the completion notice the host never drained"
    );
    let results = log
        .iter()
        .filter(|e| {
            matches!(
                &e.event,
                SessionEvent::ToolResult { call_id, .. } if call_id == "c_bg"
            )
        })
        .count();
    assert_eq!(results, 1, "the launch answer stays the call's only result");
}

/// A foreground ending is answered by its own call result, which is durable
/// in the log, so the scan adds no notice for it.
#[tokio::test]
async fn test_orphan_keeps_foreground_ending() {
    let (runtime, store, parent_sid) = runtime_with(
        Arc::new(FakeProvider::text("kept")),
        None,
        ToolRegistry::new(),
    );
    let child = SessionId::new();
    for event in [
        SessionEvent::ToolCall {
            call_id: "c_fg".into(),
            tool: "agent".into(),
            input: serde_json::json!({ "prompt": "foreground task" }),
        },
        SessionEvent::SubagentSpawn {
            child_session_id: child.to_string(),
            subagent_type: "explore".into(),
            prompt_summary: "foreground task".into(),
            isolation: "none".into(),
            policy: "delegate".into(),
            trigger_source: "model:c_fg".into(),
            run_in_background: false,
        },
        return_event(parent_sid, &child).event,
        // The foreground result lands after the return, so the call already
        // carries the child's answer.
        SessionEvent::ToolResult {
            call_id: "c_fg".into(),
            output: serde_json::json!({ "agentId": child.to_string() }),
            duration_ms: 0,
        },
    ] {
        store
            .append(entry(parent_sid, event))
            .await
            .expect("seed the finished foreground child");
    }

    runtime.reconcile_orphans(parent_sid).await;

    let log = store.replay(parent_sid).await.expect("read the parent log");
    assert!(
        !log.iter().any(|e| matches!(
            &e.event,
            SessionEvent::NotificationInjected { child_session_id, .. }
                if child_session_id == &child.to_string()
        )),
        "a foreground ending the call already answered needs no notice"
    );
}

/// A foreground child the user interrupted writes its interrupted tool result
/// before its interrupted return, the order a position heuristic once read as
/// a background launch. The launch mode travels on the spawn event, so the
/// interrupted foreground child is not mistaken for a background child and no
/// completion notice is written for it on restart.
#[tokio::test]
async fn test_orphan_skips_interrupted_foreground() {
    let (runtime, store, parent_sid) = runtime_with(
        Arc::new(FakeProvider::text("kept")),
        None,
        ToolRegistry::new(),
    );
    let child = SessionId::new();
    for event in [
        SessionEvent::ToolCall {
            call_id: "c_fg".into(),
            tool: "agent".into(),
            input: serde_json::json!({ "prompt": "foreground task" }),
        },
        SessionEvent::SubagentSpawn {
            child_session_id: child.to_string(),
            subagent_type: "explore".into(),
            prompt_summary: "foreground task".into(),
            isolation: "none".into(),
            policy: "delegate".into(),
            trigger_source: "model:c_fg".into(),
            run_in_background: false,
        },
        // The interrupted result lands before the return, the order that used
        // to be misread as a background launch.
        SessionEvent::ToolResult {
            call_id: "c_fg".into(),
            output: serde_json::json!({ "error": "interrupted by user", "not_run": true }),
            duration_ms: 0,
        },
        SessionEvent::SubagentReturn {
            child_session_id: child.to_string(),
            status: "interrupted".into(),
            summary: String::new(),
            result_ref: child.to_string(),
            input_tokens: 0,
            output_tokens: 0,
            cache_read_input_tokens: 0,
            cache_write_input_tokens: 0,
            reasoning_tokens: 0,
            input_reported: false,
            cache_read_reported: false,
            cache_write_reported: false,
            tool_calls: 0,
            tool_failures: 0,
            duration_ms: 0,
            ledger_measured: true,
            orphaned: false,
        },
    ] {
        store
            .append(entry(parent_sid, event))
            .await
            .expect("seed the interrupted foreground child");
    }

    runtime.reconcile_orphans(parent_sid).await;

    let log = store.replay(parent_sid).await.expect("read the parent log");
    assert!(
        !log.iter().any(|e| matches!(
            &e.event,
            SessionEvent::NotificationInjected { child_session_id, .. }
                if child_session_id == &child.to_string()
        )),
        "an interrupted foreground child is not a background launch and owes no notice"
    );
}

/// A pass that wrote an orphan's return and stopped before its notice leaves
/// the background ending unreported. The return answers nothing new — the
/// launch acknowledgement already answered the call — so the next entry has to
/// write the notice that pass never reached.
#[tokio::test]
async fn test_orphan_repairs_interrupted_notice() {
    let (runtime, store, parent_sid) = runtime_with(
        Arc::new(FakeProvider::text("kept")),
        None,
        ToolRegistry::new(),
    );
    let child = SessionId::new();
    for event in [
        SessionEvent::ToolCall {
            call_id: "c_bg".into(),
            tool: "agent".into(),
            input: serde_json::json!({ "prompt": "background task" }),
        },
        SessionEvent::ToolResult {
            call_id: "c_bg".into(),
            output: serde_json::json!({ "agentId": child.to_string() }),
            duration_ms: 0,
        },
        SessionEvent::SubagentSpawn {
            child_session_id: child.to_string(),
            subagent_type: "explore".into(),
            prompt_summary: "background task".into(),
            isolation: "none".into(),
            policy: "delegate".into(),
            trigger_source: "model:c_bg".into(),
            run_in_background: true,
        },
        orphan_return_event(parent_sid, &child).event,
    ] {
        store
            .append(entry(parent_sid, event))
            .await
            .expect("seed the interrupted boundary");
    }

    runtime.reconcile_orphans(parent_sid).await;

    let log = store.replay(parent_sid).await.expect("read the parent log");
    let notices = log
        .iter()
        .filter(|e| {
            matches!(
                &e.event,
                SessionEvent::NotificationInjected { child_session_id, topic, .. }
                    if child_session_id == &child.to_string() && topic == "completion"
            )
        })
        .count();
    assert_eq!(
        notices, 1,
        "the ending gets the notice the interrupted pass never wrote"
    );
}

/// A background ending publishes its completion notice before the return is
/// written. A restart scan reads that notice as reporting the run, so it does
/// not announce the same ending again.
#[tokio::test]
async fn test_orphan_keeps_early_notice() {
    let (runtime, store, parent_sid) = runtime_with(
        Arc::new(FakeProvider::text("kept")),
        None,
        ToolRegistry::new(),
    );
    let child = SessionId::new();
    for event in [
        SessionEvent::ToolCall {
            call_id: "c1".into(),
            tool: "agent".into(),
            input: serde_json::json!({ "prompt": "background task" }),
        },
        SessionEvent::ToolResult {
            call_id: "c1".into(),
            output: serde_json::json!({ "agentId": child.to_string() }),
            duration_ms: 0,
        },
        spawn_event(&child),
        SessionEvent::NotificationInjected {
            child_session_id: child.to_string(),
            turn: 0,
            order: 0,
            topic: "completion".into(),
            summary: "the ending".into(),
            capture: None,
        },
        return_event(parent_sid, &child).event,
    ] {
        store
            .append(entry(parent_sid, event))
            .await
            .expect("seed the background ending");
    }

    runtime.reconcile_orphans(parent_sid).await;

    let log = store.replay(parent_sid).await.expect("read the parent log");
    let notices = log
        .iter()
        .filter(|e| {
            matches!(
                &e.event,
                SessionEvent::NotificationInjected { child_session_id, topic, .. }
                    if child_session_id == &child.to_string() && topic == "completion"
            )
        })
        .count();
    assert_eq!(
        notices, 1,
        "a notice published before the return still reports that ending"
    );
}

/// A resumed child writes a second return, and the notice that reported the
/// first ending does not report the second: the scan announces the new one.
#[tokio::test]
async fn test_orphan_notices_new_ending() {
    let (runtime, store, parent_sid) = runtime_with(
        Arc::new(FakeProvider::text("kept")),
        None,
        ToolRegistry::new(),
    );
    let child = SessionId::new();
    for event in [
        SessionEvent::ToolCall {
            call_id: "c_one".into(),
            tool: "agent".into(),
            input: serde_json::json!({ "prompt": "first run" }),
        },
        SessionEvent::ToolResult {
            call_id: "c_one".into(),
            output: serde_json::json!({ "agentId": child.to_string() }),
            duration_ms: 0,
        },
        spawn_event(&child),
        return_event(parent_sid, &child).event,
        SessionEvent::NotificationInjected {
            child_session_id: child.to_string(),
            turn: 0,
            order: 0,
            topic: "completion".into(),
            summary: "the first ending".into(),
            capture: None,
        },
        spawn_event(&child),
        orphan_return_event(parent_sid, &child).event,
    ] {
        store
            .append(entry(parent_sid, event))
            .await
            .expect("seed the resumed child");
    }

    runtime.reconcile_orphans(parent_sid).await;

    let log = store.replay(parent_sid).await.expect("read the parent log");
    let notices = log
        .iter()
        .filter(|e| {
            matches!(
                &e.event,
                SessionEvent::NotificationInjected { child_session_id, topic, .. }
                    if child_session_id == &child.to_string() && topic == "completion"
            )
        })
        .count();
    assert_eq!(
        notices, 2,
        "the second ending gets its own notice after the first one"
    );
}

/// A delegation call a context clear moved out of the model's view is answered
/// with the notice: a result appended for it would never reach the model.
#[tokio::test]
async fn test_orphan_notices_after_clear() {
    let (runtime, store, parent_sid) = runtime_with(
        Arc::new(FakeProvider::text("kept")),
        None,
        ToolRegistry::new(),
    );
    let dead = SessionId::new();
    seed_dead_child(&store, dead).await;
    for event in [
        SessionEvent::ToolCall {
            call_id: "c1".into(),
            tool: "agent".into(),
            input: serde_json::json!({ "task": "a task" }),
        },
        spawn_event(&dead),
        SessionEvent::ContextCleared { prior_turn: 1 },
    ] {
        store
            .append(entry(parent_sid, event))
            .await
            .expect("seed the cleared delegation");
    }

    runtime.reconcile_orphans(parent_sid).await;

    let log = store.replay(parent_sid).await.expect("read the parent log");
    let answered = log
        .iter()
        .any(|e| matches!(&e.event, SessionEvent::ToolResult { call_id, .. } if call_id == "c1"));
    assert!(
        !answered,
        "a result behind the clear would never reach the model"
    );
    let noticed = log.iter().any(|e| {
        matches!(
            &e.event,
            SessionEvent::NotificationInjected { child_session_id, summary, .. }
                if child_session_id == &dead.to_string()
                    && summary.contains("half of the work")
        )
    });
    assert!(noticed, "the notice carries the work the model can resume");
}

/// Closing the same boundary twice leaves one notice for the model: a pass
/// resumed after a failed write must not queue the ending a second time.
#[tokio::test]
async fn test_orphan_notice_written_once() {
    let (_, store, parent_sid) = runtime_with(
        Arc::new(FakeProvider::text("kept")),
        None,
        ToolRegistry::new(),
    );
    let dead = SessionId::new();
    let dead_id = dead.to_string();
    // A settled boundary already has its return in the log; the notice the
    // first pass writes is judged against that newest ending.
    store
        .append(return_event(parent_sid, &dead))
        .await
        .expect("seed the settled return");
    let notice = "Subagent explore (abc) interrupted: half\nresumable via abc";
    let found = OrphanedChild {
        child_session_id: &dead_id,
        boundary_open: false,
        status: "interrupted",
        summary: "half",
        record: RecordOwed::Notice,
        usage: None,
        ledger: None,
        notice,
    };
    let log: Arc<dyn SessionLog> = store.clone();
    record_orphaned_child(log.as_ref(), parent_sid, &found)
        .await
        .expect("the first pass queues the ending");
    record_orphaned_child(log.as_ref(), parent_sid, &found)
        .await
        .expect("the resumed pass is accepted");

    let log = store.replay(parent_sid).await.expect("read the parent log");
    let notices = log
        .iter()
        .filter(|e| {
            matches!(
                &e.event,
                SessionEvent::NotificationInjected { child_session_id, .. }
                    if child_session_id == &dead_id
            )
        })
        .count();
    assert_eq!(notices, 1, "the model reads the ending once");
}
