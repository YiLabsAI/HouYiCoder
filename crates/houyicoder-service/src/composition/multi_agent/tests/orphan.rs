//! Reconciling delegation boundaries a host restart left open.

use super::*;
use std::env::temp_dir;
use std::fs::remove_dir_all;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::thread;
use tokio::time::timeout;

mod notices;

/// A spawn whose return never landed, because the host died mid-delegation, is
/// reconciled at the next run entry: the parent log gains the missing return
/// carrying the child's partial text and the completion notice naming the id,
/// so the model does not see a dangling child and can resume it. A child still
/// running is left alone, so a live delegation is not mistaken for an orphan.
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
    timeout(Duration::from_secs(30), entered)
        .await
        .expect("the live child enters its model call")
        .expect("the live child reaches its provider");

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
    let noticed = tail.iter().any(|e| {
        matches!(
            &e.event,
            SessionEvent::NotificationInjected {
                child_session_id,
                summary,
                ..
            } if child_session_id == &dead.to_string()
                && summary.contains(&format!("resumable via {dead}"))
        )
    });
    assert!(
        noticed,
        "the model reads a completion notice naming the child it can resume"
    );

    runtime.kill_child(&live.child_session_id);
    runtime.join_children().await;
}

/// The delegation call a restart left unanswered is answered by the scan with
/// the envelope the delegation tool itself returns, so the model reads the
/// child's id and partial answer instead of the generic interrupted result the
/// run entry would otherwise write. A notice would be a second record of the
/// same ending, so the answered call gets none.
#[tokio::test]
async fn test_orphan_answers_model_call() {
    let (runtime, store, parent_sid) = runtime_with(
        Arc::new(FakeProvider::text("kept")),
        None,
        ToolRegistry::new(),
    );
    let dead = SessionId::new();
    for event in [
        SessionEvent::AssistantMessage {
            text: "delegating".into(),
            thinking: None,
        },
        SessionEvent::ToolCall {
            call_id: "c_dead".into(),
            tool: "agent".into(),
            input: serde_json::json!({ "prompt": "dead task" }),
        },
    ] {
        store
            .append(entry(parent_sid, event))
            .await
            .expect("seed the model's delegation call");
    }
    seed_dead_child(&store, dead).await;
    store
        .append(entry(
            parent_sid,
            SessionEvent::SubagentSpawn {
                child_session_id: dead.to_string(),
                subagent_type: "explore".into(),
                prompt_summary: "dead task".into(),
                isolation: "none".into(),
                policy: "delegate".into(),
                trigger_source: "model:c_dead".into(),
            },
        ))
        .await
        .expect("seed the dead child's spawn");

    runtime.reconcile_orphans(parent_sid).await;

    let log = store.replay(parent_sid).await.expect("read the parent log");
    let results: Vec<&serde_json::Value> = log
        .iter()
        .filter_map(|e| match &e.event {
            SessionEvent::ToolResult {
                call_id, output, ..
            } if call_id == "c_dead" => Some(output),
            _ => None,
        })
        .collect();
    assert_eq!(
        results.len(),
        1,
        "the delegation call is answered exactly once"
    );
    let output = results[0];
    assert_eq!(output["agentId"], dead.to_string());
    assert_eq!(output["status"], "interrupted");
    assert!(
        output["content"]
            .as_str()
            .is_some_and(|text| text.contains("half of the work") && text.contains("resumable")),
        "the model reads the partial answer and the resume hint: {output}"
    );
    assert!(
        !log.iter()
            .any(|e| matches!(&e.event, SessionEvent::NotificationInjected { .. })),
        "an answered call gets no second record of the same ending"
    );
    let returned = log.iter().find_map(|e| match &e.event {
        SessionEvent::SubagentReturn {
            child_session_id,
            tool_calls,
            tool_failures,
            duration_ms,
            ..
        } if child_session_id == &dead.to_string() => {
            Some((*tool_calls, *tool_failures, *duration_ms))
        }
        _ => None,
    });
    assert_eq!(
        returned,
        Some((1, 1, 100)),
        "the return folds the child's spend from its log"
    );
}

/// A host that stopped between a live child's return and the model's result
/// leaves a settled boundary whose call has no answer. The return says how the
/// child ended and what it said, so the pass answers the call with that instead
/// of letting the generic interrupted result report an ending nobody observed.
#[tokio::test]
async fn test_orphan_answers_returned_call() {
    let (runtime, store, parent_sid) = runtime_with(
        Arc::new(FakeProvider::text("kept")),
        None,
        ToolRegistry::new(),
    );
    let done = SessionId::new();
    for event in [
        SessionEvent::ToolCall {
            call_id: "c_done".into(),
            tool: "agent".into(),
            input: serde_json::json!({ "prompt": "finished task" }),
        },
        SessionEvent::SubagentSpawn {
            child_session_id: done.to_string(),
            subagent_type: "explore".into(),
            prompt_summary: "finished task".into(),
            isolation: "none".into(),
            policy: "delegate".into(),
            trigger_source: "model:c_done".into(),
        },
        SessionEvent::SubagentReturn {
            child_session_id: done.to_string(),
            status: "completed".into(),
            summary: "the finished answer".into(),
            result_ref: done.to_string(),
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
            .expect("seed the settled boundary");
    }

    runtime.reconcile_orphans(parent_sid).await;

    let log = store.replay(parent_sid).await.expect("read the parent log");
    let results: Vec<&serde_json::Value> = log
        .iter()
        .filter_map(|e| match &e.event {
            SessionEvent::ToolResult {
                call_id, output, ..
            } if call_id == "c_done" => Some(output),
            _ => None,
        })
        .collect();
    assert_eq!(
        results.len(),
        1,
        "the unanswered delegation call gets exactly one result"
    );
    assert_eq!(
        results[0]["status"], "completed",
        "the result reports the ending the return recorded, not a generic one"
    );
    assert_eq!(results[0]["agentId"], done.to_string());
    assert!(
        results[0]["content"]
            .as_str()
            .is_some_and(|text| text.contains("the finished answer")),
        "the model reads the child's own answer: {}",
        results[0]
    );
    let returns = log
        .iter()
        .filter(|e| matches!(e.event, SessionEvent::SubagentReturn { .. }))
        .count();
    assert_eq!(returns, 1, "a settled boundary is not written again");
}

/// A boundary another writer holds is not written around, and the pass does
/// not latch: the next entry retries it once the claim is released.
#[tokio::test]
async fn test_orphan_retries_claimed_child() {
    let (runtime, store, parent_sid) = runtime_with(
        Arc::new(FakeProvider::text("kept")),
        None,
        ToolRegistry::new(),
    );
    let dead = SessionId::new();
    store
        .append(entry(
            dead,
            SessionEvent::AssistantMessage {
                text: "half of the work".into(),
                thinking: None,
            },
        ))
        .await
        .expect("seed the dead child's answer");
    store
        .append(entry(parent_sid, spawn_event(&dead)))
        .await
        .expect("seed the dead child's spawn");
    let held = runtime
        .boundary_claims
        .claim(&dead.to_string())
        .expect("the test holds the boundary");

    runtime.reconcile_orphans(parent_sid).await;
    let log = store.replay(parent_sid).await.expect("read the parent log");
    assert!(
        !log.iter()
            .any(|e| matches!(e.event, SessionEvent::SubagentReturn { .. })),
        "a claimed boundary is not written around"
    );

    drop(held);
    runtime.reconcile_orphans(parent_sid).await;
    let log = store.replay(parent_sid).await.expect("read the parent log");
    assert!(
        log.iter().any(|e| matches!(
            &e.event,
            SessionEvent::SubagentReturn { child_session_id, .. }
                if child_session_id == &dead.to_string()
        )),
        "the next entry retries the boundary the claim blocked"
    );
}

/// The run entry reconciles delegation boundaries before the generic
/// unanswered-call pass. Reversed, the pass answers the delegation call with
/// the generic interrupted result and the model never reads the child id or
/// the partial answer.
#[tokio::test]
async fn test_run_entry_answers_orphan() {
    let (runtime, store, parent_sid) = runtime_with(
        Arc::new(FakeProvider::text("kept")),
        None,
        ToolRegistry::new(),
    );
    let dead = SessionId::new();
    for event in [
        SessionEvent::ToolCall {
            call_id: "c_dead".into(),
            tool: "agent".into(),
            input: serde_json::json!({ "prompt": "dead task" }),
        },
        SessionEvent::SubagentSpawn {
            child_session_id: dead.to_string(),
            subagent_type: "explore".into(),
            prompt_summary: "dead task".into(),
            isolation: "none".into(),
            policy: "delegate".into(),
            trigger_source: "model:c_dead".into(),
        },
    ] {
        store
            .append(entry(parent_sid, event))
            .await
            .expect("seed the delegation boundary");
    }
    store
        .append(entry(
            dead,
            SessionEvent::AssistantMessage {
                text: "half of the work".into(),
                thinking: None,
            },
        ))
        .await
        .expect("seed the dead child's answer");
    let log: Arc<dyn SessionLog> = store.clone();
    let runner = Runner::new(
        log,
        Arc::new(FakeProvider::text("kept")),
        ToolRegistry::new(),
        RunnerConfig::default(),
    )
    .with_spawn_handle(Arc::new(runtime));

    runner
        .run(parent_sid, "continue".to_string())
        .await
        .expect("the turn runs");

    let log = store.replay(parent_sid).await.expect("read the parent log");
    let answers: Vec<&serde_json::Value> = log
        .iter()
        .filter_map(|e| match &e.event {
            SessionEvent::ToolResult {
                call_id, output, ..
            } if call_id == "c_dead" => Some(output),
            _ => None,
        })
        .collect();
    assert_eq!(
        answers.len(),
        1,
        "the delegation call gets one answer, not a second generic one"
    );
    assert_eq!(
        answers[0]["agentId"],
        dead.to_string(),
        "the answer names the child so the model can resume it"
    );
}

/// A background launch answers its own call at start, so a restart that
/// orphans that child must not answer the call a second time. The return lands
/// and the notice carries the ending, keeping one result per call.
#[tokio::test]
async fn test_orphan_keeps_answered_call() {
    let (runtime, store, parent_sid) = runtime_with(
        Arc::new(FakeProvider::text("kept")),
        None,
        ToolRegistry::new(),
    );
    let dead = SessionId::new();
    for event in [
        SessionEvent::ToolCall {
            call_id: "c_bg".into(),
            tool: "agent".into(),
            input: serde_json::json!({ "prompt": "background task" }),
        },
        SessionEvent::ToolResult {
            call_id: "c_bg".into(),
            output: serde_json::json!({
                "status": "",
                "content": "Launched in the background; the result arrives as a notification on your next turn.",
                "agentId": dead.to_string(),
                "result_ref": "",
                "color": null,
            }),
            duration_ms: 0,
        },
        SessionEvent::SubagentSpawn {
            child_session_id: dead.to_string(),
            subagent_type: "explore".into(),
            prompt_summary: "background task".into(),
            isolation: "none".into(),
            policy: "delegate".into(),
            trigger_source: "model:c_bg".into(),
        },
    ] {
        store
            .append(entry(parent_sid, event))
            .await
            .expect("seed the background delegation");
    }
    store
        .append(entry(
            dead,
            SessionEvent::AssistantMessage {
                text: "half of the work".into(),
                thinking: None,
            },
        ))
        .await
        .expect("seed the dead child's answer");

    runtime.reconcile_orphans(parent_sid).await;

    let log = store.replay(parent_sid).await.expect("read the parent log");
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
    let notices = log
        .iter()
        .filter(|e| {
            matches!(
                &e.event,
                SessionEvent::NotificationInjected { child_session_id, .. }
                    if child_session_id == &dead.to_string()
            )
        })
        .count();
    assert_eq!(notices, 1, "the ending still reaches the model as a notice");
}

/// A child id the host cannot parse is not shortened for a notice: the pass
/// skips it rather than slicing a durable id at a byte that may split a
/// character.
#[tokio::test]
async fn test_orphan_skips_unreadable_id() {
    let (runtime, store, parent_sid) = runtime_with(
        Arc::new(FakeProvider::text("kept")),
        None,
        ToolRegistry::new(),
    );
    let bogus = "日本語日本語";
    let mut returned = return_event(parent_sid, &SessionId::new());
    if let SessionEvent::SubagentReturn {
        child_session_id,
        orphaned,
        ..
    } = &mut returned.event
    {
        *child_session_id = bogus.to_string();
        *orphaned = true;
    }
    for event in [
        SessionEvent::SubagentSpawn {
            child_session_id: bogus.to_string(),
            subagent_type: "explore".into(),
            prompt_summary: "a task".into(),
            isolation: "none".into(),
            policy: "delegate".into(),
            trigger_source: "system:test".into(),
        },
        returned.event,
    ] {
        store
            .append(entry(parent_sid, event))
            .await
            .expect("seed the unreadable child");
    }

    runtime.reconcile_orphans(parent_sid).await;

    let log = store.replay(parent_sid).await.expect("read the parent log");
    assert!(
        !log.iter().any(|e| matches!(
            &e.event,
            SessionEvent::NotificationInjected { child_session_id, .. }
                if child_session_id == bogus
        )),
        "the pass writes no notice for an id it cannot read"
    );
}

/// A restarted host reads the durable logs, not the resident window: neither
/// the parent's spawn nor the child's partial answer is in memory when the scan
/// starts, and the return it writes still carries the partial text.
#[tokio::test]
async fn test_orphan_survives_restart() {
    let root = temp_dir().join(format!("houyi-orphan-{}", SessionId::new()));
    let first = SessionStore::new(Box::new(LocalFileBackend::new(root.clone())));
    let parent_sid = SessionId::new();
    let dead = SessionId::new();
    first
        .append(entry(
            dead,
            SessionEvent::AssistantMessage {
                text: "durable half".into(),
                thinking: None,
            },
        ))
        .await
        .expect("seed the dead child's answer");
    first
        .append(entry(parent_sid, spawn_event(&dead)))
        .await
        .expect("seed the dead child's spawn");
    drop(first);

    let (runtime, store, _) = runtime_with_backend(
        Box::new(LocalFileBackend::new(root.clone())),
        Arc::new(FakeProvider::text("kept")),
        None,
        ToolRegistry::new(),
    );
    runtime.reconcile_orphans(parent_sid).await;

    let log = store
        .replay(parent_sid)
        .await
        .expect("read the durable parent log");
    let reconciled = log.iter().any(|e| {
        matches!(
            &e.event,
            SessionEvent::SubagentReturn {
                child_session_id,
                summary,
                ..
            } if child_session_id == &dead.to_string()
                && summary.contains("orphaned by host restart")
                && summary.contains("durable half")
        )
    });
    assert!(
        reconciled,
        "a fresh host still finds the spawn and the child's partial text"
    );
    remove_dir_all(&root).ok();
}

/// A resumed delegation writes a second spawn after its first return. The
/// second spawn is the one a crash can orphan, so it gets its own return
/// rather than being settled by the earlier one.
#[tokio::test]
async fn test_resume_writes_second_return() {
    let (runtime, store, parent_sid) = runtime_with(
        Arc::new(FakeProvider::text("kept")),
        None,
        ToolRegistry::new(),
    );
    let child = SessionId::new();
    for event in [
        spawn_event(&child),
        SessionEvent::SubagentReturn {
            child_session_id: child.to_string(),
            status: "completed".into(),
            summary: "the answer".into(),
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
        spawn_event(&child),
    ] {
        store
            .append(entry(parent_sid, event))
            .await
            .expect("seed the delegation history");
    }

    runtime.reconcile_orphans(parent_sid).await;

    let log = store
        .replay(parent_sid)
        .await
        .expect("read the durable parent log");
    let returns = log
        .iter()
        .filter(|e| {
            matches!(
                &e.event,
                SessionEvent::SubagentReturn { child_session_id, .. }
                    if child_session_id == &child.to_string()
            )
        })
        .count();
    assert_eq!(
        returns, 2,
        "the resumed spawn is open and gets its own return"
    );
}

/// A claim is held by the guard the writer acquired and released when that
/// guard drops, including a scan future that was cancelled mid-boundary. A
/// claim that outlived its writer would keep every later writer off the child.
#[tokio::test]
async fn test_boundary_claim_releases() {
    let (runtime, _store, _parent_sid) = runtime_with(
        Arc::new(FakeProvider::text("kept")),
        None,
        ToolRegistry::new(),
    );
    let child = SessionId::new();
    let child_id = child.to_string();
    {
        let first = runtime
            .boundary_claims
            .claim(&child_id)
            .expect("the first writer wins");
        assert!(
            runtime.boundary_claims.claim(&child_id).is_none(),
            "a second writer is refused the same boundary"
        );
        drop(first);
    }

    let resumed = runtime.boundary_claims.claim(&child_id);
    assert!(
        resumed.is_some(),
        "the released claim lets the next writer close the boundary"
    );
}

/// A session whose first act after a restart is to continue a turn parked on
/// an approval reconciles at that entry too. The resumed model reads the same
/// parent log a fresh turn would, so a spawn with no return must not reach it.
#[tokio::test]
async fn test_orphan_reconciles_on_resume() {
    let (runtime, store, parent_sid) = runtime_with(
        Arc::new(FakeProvider::text("kept")),
        None,
        ToolRegistry::new(),
    );
    let dead = SessionId::new();
    store
        .append(entry(
            dead,
            SessionEvent::AssistantMessage {
                text: "half of the work".into(),
                thinking: None,
            },
        ))
        .await
        .expect("seed the dead child's answer");
    store
        .append(entry(parent_sid, spawn_event(&dead)))
        .await
        .expect("seed the dead child's spawn");
    let log: Arc<dyn SessionLog> = store.clone();
    let runner = Runner::new(
        log,
        Arc::new(FakeProvider::text("kept")),
        ToolRegistry::new(),
        RunnerConfig::default(),
    )
    .with_spawn_handle(Arc::new(runtime));

    runner
        .resume(parent_sid, &[])
        .await
        .expect("the resume runs");

    let log = store.replay(parent_sid).await.expect("read the parent log");
    assert!(
        log.iter().any(|e| matches!(
            &e.event,
            SessionEvent::SubagentReturn { child_session_id, .. }
                if child_session_id == &dead.to_string()
        )),
        "the resume entry reconciles the orphan before the model reads the log"
    );
}

/// A scan that could not read the parent log is not a completed pass: the next
/// run entry retries rather than latching a reconciliation that never saw the
/// boundaries. Both calls go through the same runtime, so a latch set on the
/// failed attempt would leave the orphan unreconciled forever.
#[tokio::test]
async fn test_orphan_scan_retries() {
    // Armed after the fixture is written, so the append that seeds the log is
    // not mistaken for the scan under test: the next replay fails once and the
    // one after that succeeds.
    struct FailingReplay {
        inner: InMemoryBackend,
        armed: Arc<AtomicBool>,
    }
    impl ContextBackend for FailingReplay {
        fn append(&self, event: SessionLogEntry) -> PFut<'_, Result<EventId, ContextError>> {
            self.inner.append(event)
        }
        fn read_range(
            &self,
            session: SessionId,
            from: Option<EventId>,
            to: Option<EventId>,
        ) -> PFut<'_, Result<Vec<SessionLogEntry>, ContextError>> {
            self.inner.read_range(session, from, to)
        }
        fn replay(
            &self,
            session: SessionId,
        ) -> PFut<'_, Result<Vec<SessionLogEntry>, ContextError>> {
            if self.armed.swap(false, Ordering::SeqCst) {
                return Box::pin(async { Err(ContextError::Unsupported) });
            }
            self.inner.replay(session)
        }
        fn write_checkpoint(
            &self,
            manifest: CheckpointManifest,
        ) -> PFut<'_, Result<CheckpointId, ContextError>> {
            self.inner.write_checkpoint(manifest)
        }
        fn read_checkpoint(
            &self,
            id: CheckpointId,
        ) -> PFut<'_, Result<CheckpointManifest, ContextError>> {
            self.inner.read_checkpoint(id)
        }
        fn list_checkpoints(
            &self,
            session: SessionId,
        ) -> PFut<'_, Result<Vec<CheckpointId>, ContextError>> {
            self.inner.list_checkpoints(session)
        }
    }
    let armed = Arc::new(AtomicBool::new(false));
    let (runtime, store, parent_sid) = runtime_with_backend(
        Box::new(FailingReplay {
            inner: InMemoryBackend::new(),
            armed: Arc::clone(&armed),
        }),
        Arc::new(FakeProvider::text("kept")),
        None,
        ToolRegistry::new(),
    );
    let dead = SessionId::new();
    store
        .append(entry(
            dead,
            SessionEvent::AssistantMessage {
                text: "half of the work".into(),
                thinking: None,
            },
        ))
        .await
        .expect("seed the dead child's answer");
    store
        .append(entry(parent_sid, spawn_event(&dead)))
        .await
        .expect("seed the dead child's spawn");
    armed.store(true, Ordering::SeqCst);

    runtime.reconcile_orphans(parent_sid).await;
    let after_failure = store
        .replay(parent_sid)
        .await
        .expect("the log is readable once the failure passed");
    assert!(
        !after_failure
            .iter()
            .any(|e| matches!(e.event, SessionEvent::SubagentReturn { .. })),
        "a scan that could not read the log writes no return"
    );

    runtime.reconcile_orphans(parent_sid).await;
    let after_retry = store.replay(parent_sid).await.expect("read the parent log");
    assert!(
        after_retry.iter().any(|e| matches!(
            &e.event,
            SessionEvent::SubagentReturn { child_session_id, .. }
                if child_session_id == &dead.to_string()
        )),
        "the next run entry retries the scan and reconciles the orphan"
    );
}

/// Writes the child's real return while the scan's conditional append checks
/// the parent log: the second parent read is that check, so by then the child's
/// finalize has landed its return.
struct RacingReturn {
    inner: InMemoryBackend,
    parent: SessionId,
    child: SessionId,
    armed: Arc<AtomicBool>,
    reads: Arc<AtomicUsize>,
}

impl ContextBackend for RacingReturn {
    fn append(&self, event: SessionLogEntry) -> PFut<'_, Result<EventId, ContextError>> {
        self.inner.append(event)
    }
    fn read_range(
        &self,
        session: SessionId,
        from: Option<EventId>,
        to: Option<EventId>,
    ) -> PFut<'_, Result<Vec<SessionLogEntry>, ContextError>> {
        self.inner.read_range(session, from, to)
    }
    fn replay(&self, session: SessionId) -> PFut<'_, Result<Vec<SessionLogEntry>, ContextError>> {
        Box::pin(async move {
            let parent_read = session == self.parent
                && self.armed.load(Ordering::SeqCst)
                && self.reads.fetch_add(1, Ordering::SeqCst) == 1;
            if parent_read {
                self.inner
                    .append(return_event(self.parent, &self.child))
                    .await?;
            }
            self.inner.replay(session).await
        })
    }
    fn write_checkpoint(
        &self,
        manifest: CheckpointManifest,
    ) -> PFut<'_, Result<CheckpointId, ContextError>> {
        self.inner.write_checkpoint(manifest)
    }
    fn read_checkpoint(
        &self,
        id: CheckpointId,
    ) -> PFut<'_, Result<CheckpointManifest, ContextError>> {
        self.inner.read_checkpoint(id)
    }
    fn list_checkpoints(
        &self,
        session: SessionId,
    ) -> PFut<'_, Result<Vec<CheckpointId>, ContextError>> {
        self.inner.list_checkpoints(session)
    }
}

/// A child whose own return lands between the scan's read and the scan's write
/// is not recorded as an orphan. The durable append checks the log it is about
/// to extend, so the real terminal wins and the phantom interrupted boundary is
/// never written.
#[tokio::test]
async fn test_orphan_skips_returned_child() {
    let parent_sid = SessionId::new();
    let child = SessionId::new();
    let armed = Arc::new(AtomicBool::new(false));
    let (runtime, store, _) = runtime_with_backend(
        Box::new(RacingReturn {
            inner: InMemoryBackend::new(),
            parent: parent_sid,
            child,
            armed: Arc::clone(&armed),
            reads: Arc::new(AtomicUsize::new(0)),
        }),
        Arc::new(FakeProvider::text("kept")),
        None,
        ToolRegistry::new(),
    );
    store
        .append(entry(parent_sid, spawn_event(&child)))
        .await
        .expect("seed the dead child's spawn");
    armed.store(true, Ordering::SeqCst);

    runtime.reconcile_orphans(parent_sid).await;

    let log = store
        .replay(parent_sid)
        .await
        .expect("read the durable parent log");
    let returns: Vec<&SessionEvent> = log
        .iter()
        .map(|e| &e.event)
        .filter(|e| {
            matches!(
                e,
                SessionEvent::SubagentReturn { child_session_id, .. }
                    if child_session_id == &child.to_string()
            )
        })
        .collect();
    assert_eq!(
        returns.len(),
        1,
        "the scan must not add a second return for a child that already returned"
    );
    assert!(
        matches!(
            returns[0],
            SessionEvent::SubagentReturn { status, .. } if status == "completed"
        ),
        "the child's own terminal survives: {:?}",
        returns[0]
    );
    assert!(
        !log.iter().any(|e| matches!(
            &e.event,
            SessionEvent::NotificationInjected { child_session_id, .. }
                if child_session_id == &child.to_string()
        )),
        "the child's own ending is the only record the model gets"
    );
}

/// Counts every replay, so a test can tell whether a later entry read the
/// parent log again.
struct CountingReplay {
    inner: InMemoryBackend,
    replays: Arc<AtomicUsize>,
}

impl ContextBackend for CountingReplay {
    fn append(&self, event: SessionLogEntry) -> PFut<'_, Result<EventId, ContextError>> {
        self.inner.append(event)
    }
    fn read_range(
        &self,
        session: SessionId,
        from: Option<EventId>,
        to: Option<EventId>,
    ) -> PFut<'_, Result<Vec<SessionLogEntry>, ContextError>> {
        self.inner.read_range(session, from, to)
    }
    fn replay(&self, session: SessionId) -> PFut<'_, Result<Vec<SessionLogEntry>, ContextError>> {
        self.replays.fetch_add(1, Ordering::SeqCst);
        self.inner.replay(session)
    }
    fn write_checkpoint(
        &self,
        manifest: CheckpointManifest,
    ) -> PFut<'_, Result<CheckpointId, ContextError>> {
        self.inner.write_checkpoint(manifest)
    }
    fn read_checkpoint(
        &self,
        id: CheckpointId,
    ) -> PFut<'_, Result<CheckpointManifest, ContextError>> {
        self.inner.read_checkpoint(id)
    }
    fn list_checkpoints(
        &self,
        session: SessionId,
    ) -> PFut<'_, Result<Vec<CheckpointId>, ContextError>> {
        self.inner.list_checkpoints(session)
    }
}

/// The first entry after a restart reconciles; a later entry reads nothing.
/// The runtime serves one parent session, so a completed pass settles
/// everything its log can hold, and rescanning would pay a full read for the
/// same answer.
#[tokio::test]
async fn test_orphan_scan_runs_once() {
    let replays = Arc::new(AtomicUsize::new(0));
    let (runtime, store, parent_sid) = runtime_with_backend(
        Box::new(CountingReplay {
            inner: InMemoryBackend::new(),
            replays: Arc::clone(&replays),
        }),
        Arc::new(FakeProvider::text("kept")),
        None,
        ToolRegistry::new(),
    );
    let dead = SessionId::new();
    store
        .append(entry(
            dead,
            SessionEvent::AssistantMessage {
                text: "half of the work".into(),
                thinking: None,
            },
        ))
        .await
        .expect("seed the dead child's answer");
    store
        .append(entry(parent_sid, spawn_event(&dead)))
        .await
        .expect("seed the dead child's spawn");

    runtime.reconcile_orphans(parent_sid).await;
    let after_first = replays.load(Ordering::SeqCst);
    runtime.reconcile_orphans(parent_sid).await;
    assert_eq!(
        replays.load(Ordering::SeqCst),
        after_first,
        "a completed pass is not run a second time"
    );
    let returns = store
        .replay(parent_sid)
        .await
        .expect("read the parent log")
        .iter()
        .filter(|e| matches!(e.event, SessionEvent::SubagentReturn { .. }))
        .count();
    assert_eq!(returns, 1, "the orphan gains exactly one return");
}

/// Fails the next append once, when armed, so a test can see a pass whose
/// boundary write failed leave the next entry to retry.
struct FailingAppend {
    inner: InMemoryBackend,
    armed: Arc<AtomicBool>,
}

impl ContextBackend for FailingAppend {
    fn append(&self, event: SessionLogEntry) -> PFut<'_, Result<EventId, ContextError>> {
        if self.armed.swap(false, Ordering::SeqCst) {
            return Box::pin(async { Err(ContextError::Unsupported) });
        }
        self.inner.append(event)
    }
    fn read_range(
        &self,
        session: SessionId,
        from: Option<EventId>,
        to: Option<EventId>,
    ) -> PFut<'_, Result<Vec<SessionLogEntry>, ContextError>> {
        self.inner.read_range(session, from, to)
    }
    fn replay(&self, session: SessionId) -> PFut<'_, Result<Vec<SessionLogEntry>, ContextError>> {
        self.inner.replay(session)
    }
    fn write_checkpoint(
        &self,
        manifest: CheckpointManifest,
    ) -> PFut<'_, Result<CheckpointId, ContextError>> {
        self.inner.write_checkpoint(manifest)
    }
    fn read_checkpoint(
        &self,
        id: CheckpointId,
    ) -> PFut<'_, Result<CheckpointManifest, ContextError>> {
        self.inner.read_checkpoint(id)
    }
    fn list_checkpoints(
        &self,
        session: SessionId,
    ) -> PFut<'_, Result<Vec<CheckpointId>, ContextError>> {
        self.inner.list_checkpoints(session)
    }
}

/// A pass whose boundary write failed has not reconciled anything, so the next
/// entry runs again and writes the boundary the failed pass could not.
#[tokio::test]
async fn test_orphan_retries_failed_return() {
    let armed = Arc::new(AtomicBool::new(false));
    let (runtime, store, parent_sid) = runtime_with_backend(
        Box::new(FailingAppend {
            inner: InMemoryBackend::new(),
            armed: Arc::clone(&armed),
        }),
        Arc::new(FakeProvider::text("kept")),
        None,
        ToolRegistry::new(),
    );
    let dead = SessionId::new();
    store
        .append(entry(
            dead,
            SessionEvent::AssistantMessage {
                text: "half of the work".into(),
                thinking: None,
            },
        ))
        .await
        .expect("seed the dead child's answer");
    store
        .append(entry(parent_sid, spawn_event(&dead)))
        .await
        .expect("seed the dead child's spawn");
    armed.store(true, Ordering::SeqCst);

    runtime.reconcile_orphans(parent_sid).await;
    let after_failure = store.replay(parent_sid).await.expect("read the log");
    assert!(
        !after_failure
            .iter()
            .any(|e| matches!(e.event, SessionEvent::SubagentReturn { .. })),
        "a failed boundary write leaves no half-written return"
    );

    runtime.reconcile_orphans(parent_sid).await;
    let after_retry = store.replay(parent_sid).await.expect("read the log");
    assert!(
        after_retry.iter().any(|e| matches!(
            &e.event,
            SessionEvent::SubagentReturn { child_session_id, .. }
                if child_session_id == &dead.to_string()
        )),
        "the next entry retries the boundary write"
    );
}

/// A poisoned child registry must not read as "no child is running" and must
/// not stop the pass: the guard the writer left is recovered, and the orphan
/// still gets its boundary.
#[tokio::test]
async fn test_orphan_tolerates_poisoned_lock() {
    let (runtime, store, parent_sid) = runtime_with(
        Arc::new(FakeProvider::text("kept")),
        None,
        ToolRegistry::new(),
    );
    let dead = SessionId::new();
    store
        .append(entry(
            dead,
            SessionEvent::AssistantMessage {
                text: "half of the work".into(),
                thinking: None,
            },
        ))
        .await
        .expect("seed the dead child's answer");
    store
        .append(entry(parent_sid, spawn_event(&dead)))
        .await
        .expect("seed the dead child's spawn");
    let children = Arc::clone(&runtime.children);
    let poisoned = thread::spawn(move || {
        let _guard = children.lock().expect("the registry locks");
        panic!("poison the registry");
    })
    .join();
    assert!(poisoned.is_err(), "the fixture poisons the registry lock");

    runtime.reconcile_orphans(parent_sid).await;

    let log = store.replay(parent_sid).await.expect("read the parent log");
    assert!(
        log.iter().any(|e| matches!(
            &e.event,
            SessionEvent::SubagentReturn { child_session_id, .. }
                if child_session_id == &dead.to_string()
        )),
        "the orphan is reconciled despite the poisoned lock"
    );
}

/// An id that names no session cannot be read or continued, so the pass skips
/// it rather than writing a boundary that points nowhere.
#[tokio::test]
async fn test_orphan_skips_unparsable_child() {
    let (runtime, store, parent_sid) = runtime_with(
        Arc::new(FakeProvider::text("kept")),
        None,
        ToolRegistry::new(),
    );
    store
        .append(entry(
            parent_sid,
            SessionEvent::SubagentSpawn {
                child_session_id: "not-a-session".into(),
                subagent_type: "explore".into(),
                prompt_summary: "a task".into(),
                isolation: "none".into(),
                policy: "delegate".into(),
                trigger_source: "model:c1".into(),
            },
        ))
        .await
        .expect("seed the unparseable spawn");

    runtime.reconcile_orphans(parent_sid).await;

    let log = store.replay(parent_sid).await.expect("read the parent log");
    assert!(
        !log.iter().any(|e| matches!(
            e.event,
            SessionEvent::SubagentReturn { .. } | SessionEvent::NotificationInjected { .. }
        )),
        "an id that names no session gets no boundary and no notice"
    );
}

/// Fails every replay of one child session, so a test can see the pass close a
/// boundary whose child log cannot be read.
struct FailingChildReplay {
    inner: InMemoryBackend,
    child: SessionId,
}

impl ContextBackend for FailingChildReplay {
    fn append(&self, event: SessionLogEntry) -> PFut<'_, Result<EventId, ContextError>> {
        self.inner.append(event)
    }
    fn read_range(
        &self,
        session: SessionId,
        from: Option<EventId>,
        to: Option<EventId>,
    ) -> PFut<'_, Result<Vec<SessionLogEntry>, ContextError>> {
        self.inner.read_range(session, from, to)
    }
    fn replay(&self, session: SessionId) -> PFut<'_, Result<Vec<SessionLogEntry>, ContextError>> {
        if session == self.child {
            return Box::pin(async { Err(ContextError::Unsupported) });
        }
        self.inner.replay(session)
    }
    fn write_checkpoint(
        &self,
        manifest: CheckpointManifest,
    ) -> PFut<'_, Result<CheckpointId, ContextError>> {
        self.inner.write_checkpoint(manifest)
    }
    fn read_checkpoint(
        &self,
        id: CheckpointId,
    ) -> PFut<'_, Result<CheckpointManifest, ContextError>> {
        self.inner.read_checkpoint(id)
    }
    fn list_checkpoints(
        &self,
        session: SessionId,
    ) -> PFut<'_, Result<Vec<CheckpointId>, ContextError>> {
        self.inner.list_checkpoints(session)
    }
}

/// A child log this host cannot read still gets its boundary closed: the
/// return carries the restart marker without the child's text, its figures are
/// marked unmeasured, and the notice says what could not be read.
#[tokio::test]
async fn test_orphan_closes_unreadable_child() {
    let dead = SessionId::new();
    let (runtime, store, parent_sid) = runtime_with_backend(
        Box::new(FailingChildReplay {
            inner: InMemoryBackend::new(),
            child: dead,
        }),
        Arc::new(FakeProvider::text("kept")),
        None,
        ToolRegistry::new(),
    );
    store
        .append(entry(parent_sid, spawn_event(&dead)))
        .await
        .expect("seed the dead child's spawn");

    runtime.reconcile_orphans(parent_sid).await;

    let log = store.replay(parent_sid).await.expect("read the parent log");
    assert!(
        log.iter().any(|e| matches!(
            &e.event,
            SessionEvent::SubagentReturn {
                child_session_id,
                summary,
                ledger_measured,
                orphaned,
                ..
            } if child_session_id == &dead.to_string()
                && summary.contains("orphaned by host restart")
                && !ledger_measured
                && *orphaned
        )),
        "the boundary closes and its figures read as unmeasured"
    );
    let notices: Vec<&str> = log
        .iter()
        .filter_map(|e| match &e.event {
            SessionEvent::NotificationInjected { summary, .. } => Some(summary.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        notices
            .iter()
            .any(|summary| summary.contains("could not be read")),
        "the notice says what the restart prevented: {notices:?}"
    );
}

/// A pass that closed the boundary but crashed before writing the record the
/// model reads resumes on the next entry: the orphan marker says the return
/// landed, so the call is answered without a second return.
#[tokio::test]
async fn test_orphan_retries_model_record() {
    let (runtime, store, parent_sid) = runtime_with(
        Arc::new(FakeProvider::text("kept")),
        None,
        ToolRegistry::new(),
    );
    let dead = SessionId::new();
    for event in [
        SessionEvent::ToolCall {
            call_id: "c1".into(),
            tool: "agent".into(),
            input: serde_json::json!({ "task": "a task" }),
        },
        spawn_event(&dead),
        SessionEvent::SubagentReturn {
            child_session_id: dead.to_string(),
            status: "interrupted".into(),
            summary: format!("[orphaned by host restart, resumable via {dead}]"),
            result_ref: dead.to_string(),
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
            ledger_measured: false,
            orphaned: true,
        },
    ] {
        store
            .append(entry(parent_sid, event))
            .await
            .expect("seed the half-closed boundary");
    }

    runtime.reconcile_orphans(parent_sid).await;

    let log = store.replay(parent_sid).await.expect("read the parent log");
    let returns = log
        .iter()
        .filter(|e| {
            matches!(
                &e.event,
                SessionEvent::SubagentReturn { child_session_id, .. }
                    if child_session_id == &dead.to_string()
            )
        })
        .count();
    assert_eq!(returns, 1, "the closed boundary is not written again");
    let answered = log.iter().any(|e| {
        matches!(
            &e.event,
            SessionEvent::ToolResult { call_id, output, .. }
                if call_id == "c1" && output["agentId"] == dead.to_string()
        )
    });
    assert!(answered, "the model still gets its call answered");
}

/// The orphan's figures come from the child's own log, judged by the failure
/// rule the child's header applies, so a command whose exit status is its
/// answer counts the same on both paths.
#[tokio::test]
async fn test_orphan_ledger_matches_child() {
    let (runtime, store, parent_sid) = runtime_with(
        Arc::new(FakeProvider::text("kept")),
        None,
        ToolRegistry::new(),
    );
    let dead = SessionId::new();
    let child_log = [
        (
            100,
            SessionEvent::ToolCall {
                call_id: "t1".into(),
                tool: "bash".into(),
                input: serde_json::json!({ "command": "git diff --exit-code" }),
            },
        ),
        (
            110,
            SessionEvent::ToolResult {
                call_id: "t1".into(),
                output: serde_json::json!({ "success": false, "exit_code": 1 }),
                duration_ms: 5,
            },
        ),
        (
            120,
            SessionEvent::ToolCall {
                call_id: "t2".into(),
                tool: "read".into(),
                input: serde_json::json!({ "path": "notes.md" }),
            },
        ),
        (
            130,
            SessionEvent::ToolResult {
                call_id: "t2".into(),
                output: serde_json::json!({ "error": "missing" }),
                duration_ms: 5,
            },
        ),
        (
            200,
            SessionEvent::AssistantMessage {
                text: "the answer".into(),
                thinking: None,
            },
        ),
    ];
    for (ts, event) in child_log {
        let mut record = entry(dead, event);
        record.ts = ts;
        store
            .append(record)
            .await
            .expect("seed the dead child's log");
    }
    store
        .append(entry(parent_sid, spawn_event(&dead)))
        .await
        .expect("seed the dead child's spawn");

    runtime.reconcile_orphans(parent_sid).await;

    let log = store.replay(parent_sid).await.expect("read the parent log");
    let returned = log.iter().find_map(|e| match &e.event {
        SessionEvent::SubagentReturn {
            child_session_id,
            tool_calls,
            tool_failures,
            ledger_measured,
            ..
        } if child_session_id == &dead.to_string() => {
            Some((*tool_calls, *tool_failures, *ledger_measured))
        }
        _ => None,
    });
    assert_eq!(
        returned,
        Some((2, 1, true)),
        "a git difference is not a failure and the figures are measured"
    );
    // The child's own header folds the same log, so the two paths must agree
    // about what ran and what failed.
    let folded = store.trajectory_head(dead).summary.usage;
    assert_eq!(
        (folded.tool_calls, folded.failures),
        (2, 1),
        "the replayed ledger matches what the child's header reports"
    );
}

/// Seed a dead child's own log with one measured, failing tool call and the
/// partial answer the scan reports, so its return has figures to fold.
async fn seed_dead_child(store: &SessionStore, child: SessionId) {
    for (ts, event) in [
        (
            100,
            SessionEvent::AssistantMessage {
                text: "working".into(),
                thinking: None,
            },
        ),
        (
            110,
            SessionEvent::ToolCall {
                call_id: "t1".into(),
                tool: "read".into(),
                input: serde_json::json!({ "path": "notes.md" }),
            },
        ),
        (
            120,
            SessionEvent::ToolResult {
                call_id: "t1".into(),
                output: serde_json::json!({ "error": "missing" }),
                duration_ms: 5,
            },
        ),
        (
            200,
            SessionEvent::AssistantMessage {
                text: "half of the work".into(),
                thinking: None,
            },
        ),
    ] {
        let mut record = entry(child, event);
        record.ts = ts;
        store
            .append(record)
            .await
            .expect("seed the dead child's log");
    }
}

fn entry(session: SessionId, event: SessionEvent) -> SessionLogEntry {
    SessionLogEntry {
        id: EventId::new(),
        session,
        ts: 0,
        prev_hash: None,
        event,
    }
}

fn return_event(session: SessionId, child: &SessionId) -> SessionLogEntry {
    entry(
        session,
        SessionEvent::SubagentReturn {
            child_session_id: child.to_string(),
            status: "completed".into(),
            summary: "the answer".into(),
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
    )
}

/// The same return, written by a reconciliation pass instead of the host that
/// ran the child.
fn orphan_return_event(session: SessionId, child: &SessionId) -> SessionLogEntry {
    let mut seeded = return_event(session, child);
    if let SessionEvent::SubagentReturn { orphaned, .. } = &mut seeded.event {
        *orphaned = true;
    }
    seeded
}

fn spawn_event(child: &SessionId) -> SessionEvent {
    SessionEvent::SubagentSpawn {
        child_session_id: child.to_string(),
        subagent_type: "explore".into(),
        prompt_summary: "a task".into(),
        isolation: "none".into(),
        policy: "delegate".into(),
        trigger_source: "model:c1".into(),
    }
}
