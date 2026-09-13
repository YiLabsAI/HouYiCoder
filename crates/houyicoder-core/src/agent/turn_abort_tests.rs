//! Per-turn abort: the viewed-child Esc path cancels the in-flight model
//! fetch without killing the run. The drive loop appends a TurnAborted
//! marker + starts the next turn with a fresh token (non-terminal).

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use houyicoder_api::provider::ModelProvider;
use houyicoder_async::{PFut, PStream};
use houyicoder_context::{SessionEvent, SessionId};
use houyicoder_protocol::llm::{
    CompletionRequest, CompletionResponse, LlmEvent, ModelCapabilities, OutputItem, ProviderError,
    Usage,
};
use tokio::sync::Notify;

use crate::agent::runner_tests::runner_with;
use crate::agent::{RunError, RunOutcome, ToolRegistry};
use crate::provider::test_support::FakeProvider;

/// A provider whose first stream call never yields (the model fetch is
/// in-flight, waiting) so a per-turn cancel can race the stream select.
/// Later calls delegate to a real fake response so the run can finish.
struct StallOnceProvider {
    inner: Arc<FakeProvider>,
    calls: AtomicU32,
}

impl StallOnceProvider {
    fn new(inner: Arc<FakeProvider>) -> Self {
        Self {
            inner,
            calls: AtomicU32::new(0),
        }
    }
}

impl ModelProvider for StallOnceProvider {
    fn complete(
        &self,
        req: CompletionRequest,
    ) -> PFut<'_, Result<CompletionResponse, ProviderError>> {
        self.inner.complete(req)
    }
    fn stream(&self, req: CompletionRequest) -> PStream<'_, Result<LlmEvent, ProviderError>> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            // The first model call stalls forever so the per-turn cancel
            // can fire while the stream select waits on stream.next().
            Box::pin(futures::stream::pending())
        } else {
            self.inner.stream(req)
        }
    }
    fn capabilities(&self) -> ModelCapabilities {
        self.inner.capabilities()
    }
}

/// A per-turn cancel on a stalled model fetch aborts the turn without
/// killing the run: the drive loop appends a TurnAborted marker + starts
/// the next turn, which finishes (FinalOutput). The run resolves
/// FinalOutput, not Interrupted (the lifecycle token was never cancelled).
#[tokio::test]
async fn test_per_turn_abort_continues() {
    let finish = CompletionResponse {
        output: vec![OutputItem::Text {
            text: "done".into(),
        }],
        usage: Usage::default(),
        model: "test".into(),
    };
    let inner = Arc::new(FakeProvider::new(vec![finish]));
    let provider: Arc<dyn ModelProvider> = Arc::new(StallOnceProvider::new(inner));
    let runner = Arc::new(runner_with(provider, ToolRegistry::new()));
    let session = SessionId::new();
    let r = runner.clone();
    let task = tokio::spawn(async move { r.run(session, "hi".into()).await });
    // Let the run enter the first model call + stall on the pending stream.
    for _ in 0..30 {
        tokio::task::yield_now().await;
    }
    runner.cancel_turn();
    let result = tokio::time::timeout(std::time::Duration::from_secs(3), task)
        .await
        .expect("run resolved within 3s after the per-turn abort")
        .expect("run task")
        .expect("run ok");
    assert!(
        matches!(result.outcome, RunOutcome::FinalOutput { .. }),
        "per-turn abort is non-terminal; the run should finish, got {:?}",
        result.outcome
    );
    let events = runner.store().replay(session).await.expect("replay");
    let aborted = events
        .iter()
        .filter(|e| matches!(e.event, SessionEvent::TurnAborted { .. }))
        .count();
    assert!(
        aborted >= 1,
        "a TurnAborted marker lands for the interrupted turn"
    );
}

/// cancel_turn is a no-op when no turn is in flight (the token is cleared
/// between turns). A runner at idle cancels nothing + does not panic.
#[tokio::test]
async fn test_cancel_turn_idle_noop() {
    let inner = Arc::new(FakeProvider::text("ok"));
    let runner = runner_with(
        Arc::new(StallOnceProvider::new(inner)) as Arc<dyn ModelProvider>,
        ToolRegistry::new(),
    );
    // No run in flight: the turn-cancel slot is empty, so cancel_turn
    // finds no token + returns. No panic, no state change.
    runner.cancel_turn();
}

/// A provider that emits the first stream event then stalls forever, so a
/// per-turn cancel lands mid-stream (past the pre-first-event select).
struct StallMidStreamProvider {
    finish: Arc<FakeProvider>,
    calls: AtomicU32,
}

impl StallMidStreamProvider {
    fn new(finish: Arc<FakeProvider>) -> Self {
        Self {
            finish,
            calls: AtomicU32::new(0),
        }
    }
}

impl ModelProvider for StallMidStreamProvider {
    fn complete(
        &self,
        req: CompletionRequest,
    ) -> PFut<'_, Result<CompletionResponse, ProviderError>> {
        self.finish.complete(req)
    }
    fn stream(&self, req: CompletionRequest) -> PStream<'_, Result<LlmEvent, ProviderError>> {
        use futures::StreamExt;
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            // Emit the first event of the real stream, then stall forever
            // so the drive loop is mid-stream when cancel_turn fires.
            let first = self.finish.stream(req);
            Box::pin(first.take(1).chain(futures::stream::pending()))
        } else {
            self.finish.stream(req)
        }
    }
    fn capabilities(&self) -> ModelCapabilities {
        self.finish.capabilities()
    }
}

/// A per-turn cancel mid-stream (after the first event, while stream.next()
/// stalls) aborts the turn without killing the run: the drive loop appends a
/// TurnAborted marker + the next turn finishes. Covers the mid-stream
/// select! turn_token branch the pre-first-event test does not reach.
#[tokio::test]
async fn test_mid_stream_abort() {
    let finish = CompletionResponse {
        output: vec![OutputItem::Text {
            text: "done".into(),
        }],
        usage: Usage::default(),
        model: "test".into(),
    };
    let inner = Arc::new(FakeProvider::new(vec![finish]));
    let provider: Arc<dyn ModelProvider> = Arc::new(StallMidStreamProvider::new(inner));
    let runner = Arc::new(runner_with(provider, ToolRegistry::new()));
    let session = SessionId::new();
    let r = runner.clone();
    let task = tokio::spawn(async move { r.run(session, "hi".into()).await });
    // Let the run emit the first event + enter the mid-stream stall.
    for _ in 0..40 {
        tokio::task::yield_now().await;
    }
    runner.cancel_turn();
    let result = tokio::time::timeout(std::time::Duration::from_secs(3), task)
        .await
        .expect("run resolved within 3s after the mid-stream abort")
        .expect("run task")
        .expect("run ok");
    assert!(
        matches!(result.outcome, RunOutcome::FinalOutput { .. }),
        "mid-stream per-turn abort is non-terminal; the run should finish, got {:?}",
        result.outcome
    );
    let events = runner.store().replay(session).await.expect("replay");
    assert!(
        events
            .iter()
            .any(|e| matches!(e.event, SessionEvent::TurnAborted { .. })),
        "a TurnAborted marker lands for the mid-stream-interrupted turn"
    );
}

/// A provider whose stream never yields — every call stalls forever. Used
/// to exercise the stream_idle_timeout path: the idle timeout fires, retries
/// exhaust, and the run fails with ProviderFatal(Network).
struct AlwaysStallProvider;

impl ModelProvider for AlwaysStallProvider {
    fn complete(
        &self,
        _req: CompletionRequest,
    ) -> PFut<'_, Result<CompletionResponse, ProviderError>> {
        Box::pin(async { Err(ProviderError::Network) })
    }
    fn stream(&self, _req: CompletionRequest) -> PStream<'_, Result<LlmEvent, ProviderError>> {
        Box::pin(futures::stream::pending())
    }
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
}

/// The stream_idle_timeout (50ms in test) fires when the provider stalls,
/// retries exhaust, and the run fails with ProviderFatal(Network). A
/// perpetually stalling provider must fail, not hang the run indefinitely.
#[tokio::test]
async fn test_idle_timeout_to_fatal() {
    let provider: Arc<dyn ModelProvider> = Arc::new(AlwaysStallProvider);
    let runner = Arc::new(runner_with(provider, ToolRegistry::new()));
    let session = SessionId::new();
    let r = runner.clone();
    let task = tokio::spawn(async move { r.run(session, "hi".into()).await });
    let result = tokio::time::timeout(std::time::Duration::from_secs(10), task)
        .await
        .expect("run resolved within 10s (idle timeout retries)")
        .expect("run task");
    assert!(
        result.is_err(),
        "a perpetually stalling provider must fail, not hang"
    );
    match result {
        Err(RunError::ProviderFatal(ProviderError::Network)) => {}
        other => panic!("expected ProviderFatal(Network) from idle timeout, got {other:?}"),
    }
}

/// A provider whose first stream call yields one event, then parks at a
/// test-controlled gate before draining the rest, so a test can make the
/// mid-stream select's cancel branch and stream branch ready at once and
/// let the select's priority — not polling luck — decide.
struct RaceMidStreamProvider {
    finish: Arc<FakeProvider>,
    at_gate: Arc<Notify>,
    release: Arc<Notify>,
    calls: AtomicU32,
}

impl RaceMidStreamProvider {
    fn new(finish: Arc<FakeProvider>) -> Self {
        Self {
            finish,
            at_gate: Arc::new(Notify::new()),
            release: Arc::new(Notify::new()),
            calls: AtomicU32::new(0),
        }
    }
}

/// The gated stream's position: yield the first event, hold at the gate,
/// then drain the remainder.
enum GateStep {
    First,
    Gated,
    Rest,
}

impl ModelProvider for RaceMidStreamProvider {
    fn complete(
        &self,
        req: CompletionRequest,
    ) -> PFut<'_, Result<CompletionResponse, ProviderError>> {
        self.finish.complete(req)
    }
    fn stream(&self, req: CompletionRequest) -> PStream<'_, Result<LlmEvent, ProviderError>> {
        use futures::StreamExt;
        if self.calls.fetch_add(1, Ordering::SeqCst) != 0 {
            return self.finish.stream(req);
        }
        let inner = self.finish.stream(req);
        let at_gate = self.at_gate.clone();
        let release = self.release.clone();
        Box::pin(futures::stream::unfold(
            (inner, at_gate, release, GateStep::First),
            |(mut inner, at_gate, release, step)| async move {
                match step {
                    GateStep::First => {
                        let ev = inner.next().await?;
                        Some((ev, (inner, at_gate, release, GateStep::Gated)))
                    }
                    GateStep::Gated => {
                        at_gate.notify_one();
                        release.notified().await;
                        let ev = inner.next().await?;
                        Some((ev, (inner, at_gate, release, GateStep::Rest)))
                    }
                    GateStep::Rest => {
                        let ev = inner.next().await?;
                        Some((ev, (inner, at_gate, release, GateStep::Rest)))
                    }
                }
            },
        ))
    }
    fn capabilities(&self) -> ModelCapabilities {
        self.finish.capabilities()
    }
}

/// A lifecycle abort landing while the run parks in the mid-stream select,
/// with the stream simultaneously ready, must resolve to Interrupted. The
/// biased priority puts cancel first; without it the ready stream branch
/// wins about half the time. Looped so polling-order luck cannot hide.
#[tokio::test]
async fn test_lifecycle_abort_wins_race() {
    for _ in 0..25 {
        let provider = Arc::new(RaceMidStreamProvider::new(Arc::new(FakeProvider::text(
            "done",
        ))));
        let at_gate = provider.at_gate.clone();
        let release = provider.release.clone();
        let runner = Arc::new(runner_with(
            provider as Arc<dyn ModelProvider>,
            ToolRegistry::new(),
        ));
        let session = SessionId::new();
        let r = runner.clone();
        let task = tokio::spawn(async move { r.run(session, "hi".into()).await });
        // Wait until the run parks at the gate inside the mid-stream select.
        at_gate.notified().await;
        // Cancel the lifecycle token and release the stream with no await
        // between, so both select branches are ready in the same re-poll.
        runner.abort();
        release.notify_one();
        let result = tokio::time::timeout(std::time::Duration::from_secs(3), task)
            .await
            .expect("run resolved within 3s of the lifecycle abort")
            .expect("run task")
            .expect("run ok");
        assert!(
            matches!(result.outcome, RunOutcome::Interrupted(_)),
            "lifecycle abort must win the mid-stream race, got {:?}",
            result.outcome
        );
    }
}
