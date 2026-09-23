//! The durable run-completion record: one per finished turn, read by the
//! frontend's turn summary row for the duration and for the end of the turn.
//! A missing record leaves the turn folded into the next one; a duplicate
//! renders the row twice.
//!
//! One per turn is a property of the call sites, not of the writer: each
//! point that finishes or abandons a turn records there, once for one turn,
//! and a paused leg writes none because the turn it left open is not over.

use super::*;
use crate::agent::abort_tool_tests::GuardedHangingTool;
use crate::agent::runner_tests::{GuardedTool, runner_with, runner_with_cfg0};
use crate::provider::test_support::FakeProvider;
use houyicoder_async::PFut;
use houyicoder_context::{
    CheckpointId, CheckpointManifest, ContextBackend, ContextError, EventId, SessionEvent,
    SessionId, SessionLogEntry,
};
use houyicoder_memory::InMemoryBackend;
use houyicoder_protocol::llm::{CompletionResponse, OutputItem};
use houyicoder_session::SessionStore;

/// The durations of every completion record in the log, in order.
fn recorded_ms(events: &[houyicoder_context::SessionLogEntry]) -> Vec<Option<u64>> {
    events
        .iter()
        .filter_map(|e| match &e.event {
            SessionEvent::RunCompleted { ms } => Some(*ms),
            _ => None,
        })
        .collect()
}

/// A log that refuses the result a released approval writes and keeps
/// everything else in memory, so a resume leg that fails part-way can be told
/// from one that finished on a store the test still replays.
struct RefusesResults {
    inner: InMemoryBackend,
}

impl ContextBackend for RefusesResults {
    fn append(&self, event: SessionLogEntry) -> PFut<'_, Result<EventId, ContextError>> {
        if matches!(event.event, SessionEvent::ToolResult { .. }) {
            return Box::pin(async { Err(ContextError::Io) });
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

/// A provider that repeats one guarded call keeps every turn pausing, so a
/// run crosses the cap without ever producing an answer.
fn capped_runner() -> Runner {
    guarded_runner(Arc::new(SessionStore::new(
        Box::new(InMemoryBackend::new()),
    )))
}

/// The paused-run shape on a caller's store, so a test can choose what the log
/// does when the released call is written.
fn guarded_runner(store: Arc<SessionStore>) -> Runner {
    let resp = CompletionResponse {
        output: vec![OutputItem::ToolCall {
            id: "c1".into(),
            name: "guarded".into(),
            input: serde_json::json!({}),
        }],
        usage: Usage::default(),
        model: "test".into(),
    };
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(GuardedTool::new()));
    Runner::new(
        store,
        Arc::new(FakeProvider::new(vec![resp])),
        tools,
        RunnerConfig {
            max_turns: 1,
            ..runner_with_cfg0()
        },
    )
}

fn approvals_of(outcome: RunOutcome) -> Vec<ApprovalRequest> {
    match outcome {
        RunOutcome::Interruption(a) => a,
        other => panic!("expected the guarded call to pause for approval, got {other:?}"),
    }
}

#[tokio::test]
async fn test_finished_run_records_end() {
    let p = Arc::new(FakeProvider::text("done"));
    let runner = runner_with(p, ToolRegistry::new());
    let session = SessionId::new();
    runner.run(session, "hi".into()).await.expect("run");
    let events = runner.store().replay(session).await.expect("replay");
    assert!(
        matches!(recorded_ms(&events).as_slice(), [Some(_)]),
        "one turn, one record: {events:?}"
    );
}

#[tokio::test]
async fn test_paused_leg_records_none() {
    // The pause is not the end of the turn: the record would close a turn the
    // resumed leg is still driving.
    let runner = capped_runner();
    let session = SessionId::new();
    let _paused = approvals_of(runner.run(session, "hi".into()).await.expect("run").outcome);
    let events = runner.store().replay(session).await.expect("replay");
    assert!(
        recorded_ms(&events).is_empty(),
        "a paused leg records nothing: {events:?}"
    );
}

#[tokio::test]
async fn test_aborted_ask_records_work() {
    // A cancel lands while the run waits on the approval, and the resume that
    // follows short-circuits without driving a loop. The turn still ends here,
    // and the leg that reached the ask is work the turn did, so the record
    // carries a measured duration rather than reporting the turn unmeasured.
    let runner = capped_runner();
    let session = SessionId::new();
    let _paused = approvals_of(runner.run(session, "hi".into()).await.expect("run").outcome);
    runner.abort();
    let stopped = runner.resume(session, &[]).await.expect("resume");
    assert!(matches!(stopped.outcome, RunOutcome::Interrupted(_)));
    let events = runner.store().replay(session).await.expect("replay");
    assert!(
        matches!(recorded_ms(&events).as_slice(), [Some(_)]),
        "the cancelled ask closes the turn with the work it did: {events:?}"
    );
}

#[tokio::test]
async fn test_failed_resume_records_end() {
    // The released call's result cannot be written, so the leg fails part-way
    // through applying the decision. The turn ends there all the same: the
    // record closes it and reports the work the leg did, so a failed resume is
    // not mistaken for a turn the next one should still be driving.
    let runner = guarded_runner(Arc::new(SessionStore::new(Box::new(RefusesResults {
        inner: InMemoryBackend::new(),
    }))));
    let session = SessionId::new();
    let paused = approvals_of(runner.run(session, "hi".into()).await.expect("run").outcome);
    let decisions: Vec<ApprovalDecision> = paused
        .iter()
        .map(|a| ApprovalDecision::approve(&a.call_id))
        .collect();
    runner
        .resume(session, &decisions)
        .await
        .expect_err("the refused result fails the leg");
    let events = runner.store().replay(session).await.expect("replay");
    assert!(
        matches!(recorded_ms(&events).as_slice(), [Some(_)]),
        "the failed leg closes the turn: {events:?}"
    );
}

#[tokio::test]
async fn test_aborted_failure_records_end() {
    // The abort short-circuit reconciles the call the pause left unanswered,
    // and the log refuses that write, so the resume fails before it drives
    // anything at all. The turn still ends here, and the record it leaves
    // reports the work the paused leg accounted.
    let runner = guarded_runner(Arc::new(SessionStore::new(Box::new(RefusesResults {
        inner: InMemoryBackend::new(),
    }))));
    let session = SessionId::new();
    let _paused = approvals_of(runner.run(session, "hi".into()).await.expect("run").outcome);
    runner.abort();
    runner
        .resume(session, &[])
        .await
        .expect_err("the refused reconciliation fails the resume");
    let events = runner.store().replay(session).await.expect("replay");
    assert!(
        matches!(recorded_ms(&events).as_slice(), [Some(_)]),
        "the failed resume closes the turn: {events:?}"
    );
}

#[tokio::test]
async fn test_capped_leg_records_end() {
    let runner = capped_runner();
    let session = SessionId::new();
    let paused = approvals_of(runner.run(session, "hi".into()).await.expect("run").outcome);
    let decisions: Vec<ApprovalDecision> = paused
        .iter()
        .map(|a| ApprovalDecision::approve(&a.call_id))
        .collect();
    let capped = runner.resume(session, &decisions).await.expect("resume");
    assert!(matches!(capped.outcome, RunOutcome::MaxTurnsReached { .. }));
    let events = runner.store().replay(session).await.expect("replay");
    assert_eq!(
        recorded_ms(&events).len(),
        1,
        "the leg that finished the turn records it once: {events:?}"
    );
}

#[tokio::test]
async fn test_stopped_run_records_end() {
    // A stop during an approved call ends the turn there. The record still
    // lands, so the frontend does not fold the aborted turn's reasoning into
    // the following one.
    let resp = CompletionResponse {
        output: vec![OutputItem::ToolCall {
            id: "c1".into(),
            name: "guarded_hanging".into(),
            input: serde_json::json!({}),
        }],
        usage: Usage::default(),
        model: "test".into(),
    };
    let started = Arc::new(tokio::sync::Notify::new());
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(GuardedHangingTool::new(started.clone())));
    let runner = Arc::new(runner_with(Arc::new(FakeProvider::new(vec![resp])), tools));
    let session = SessionId::new();
    let paused = approvals_of(
        runner
            .run(session, "run it".into())
            .await
            .expect("run")
            .outcome,
    );
    let decisions: Vec<ApprovalDecision> = paused
        .iter()
        .map(|a| ApprovalDecision::approve(&a.call_id))
        .collect();
    let r = runner.clone();
    let task = tokio::spawn(async move { r.resume(session, &decisions).await });
    started.notified().await;
    runner.abort();
    let stopped = tokio::time::timeout(std::time::Duration::from_secs(2), task)
        .await
        .expect("resume resolved after the stop")
        .expect("resume task")
        .expect("resume ok");
    assert!(matches!(stopped.outcome, RunOutcome::Interrupted(_)));
    let events = runner.store().replay(session).await.expect("replay");
    assert!(
        matches!(recorded_ms(&events).as_slice(), [Some(_)]),
        "the stop closes the turn with one record: {events:?}"
    );
}
