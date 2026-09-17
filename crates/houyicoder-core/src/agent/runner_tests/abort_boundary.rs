//! Abort observed at the run's terminal boundary.
//!
//! The token is checked while the model stream is live and after tool
//! dispatch, but the work that follows the stream's last event (usage fill,
//! truncation classification, answer parsing) never reads it. A cancel
//! landing in that window must still end the run as interrupted, not as a
//! successful answer. The same holds for the answer's tail: the verify gate
//! and the memory work after it read no token, so a stop landing in either
//! must still end the run interrupted.

use super::runner_with;
use crate::agent::extractor::MemoryExtractor;
use crate::agent::memory::{MemoryGates, MemoryRuntime};
use crate::agent::runner_config::RunnerConfig;
use crate::agent::tool::ToolRegistry;
use crate::agent::{RunOutcome, Runner, VerifyFailure, VerifyGate};
use crate::provider::test_support::FakeProvider;
use futures::StreamExt;
use houyicoder_api::provider::ModelProvider;
use houyicoder_api::session::SessionLog;
use houyicoder_async::{PFut, PStream};
use houyicoder_context::{
    CheckpointId, CheckpointManifest, ContextBackend, ContextError, EventId, SessionId,
    SessionLogEntry,
};
use houyicoder_memory::{InMemoryBackend, MarkdownMemoryProvider};
use houyicoder_protocol::llm::{
    CompletionRequest, CompletionResponse, LlmEvent, ModelCapabilities, ProviderError, Usage,
};
use houyicoder_session::SessionStore;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

/// A provider that cancels the run on the stream's final poll, once its last
/// chunk is out — the window a real Esc lands in when the user presses it
/// while the answer is already complete on the provider side.
pub(super) struct AbortOnStreamEnd {
    events: Vec<LlmEvent>,
    runner: Mutex<Weak<Runner>>,
}

impl AbortOnStreamEnd {
    pub(super) fn new(events: Vec<LlmEvent>) -> Self {
        Self {
            events,
            runner: Mutex::new(Weak::new()),
        }
    }

    /// Point the provider at the runner it must cancel. Called after the
    /// runner is built: the provider is a constructor argument, so the two
    /// cannot hold each other directly.
    pub(super) fn bind(&self, runner: &Arc<Runner>) {
        *self.runner.lock().expect("runner slot") = Arc::downgrade(runner);
    }
}

impl ModelProvider for AbortOnStreamEnd {
    fn complete(
        &self,
        _req: CompletionRequest,
    ) -> PFut<'_, Result<CompletionResponse, ProviderError>> {
        Box::pin(async { Err(ProviderError::Network) })
    }
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
    fn stream(&self, _req: CompletionRequest) -> PStream<'_, Result<LlmEvent, ProviderError>> {
        let prefix = futures::stream::iter(self.events.clone().into_iter().map(Ok));
        let runner = self.runner.lock().expect("runner slot").clone();
        let mut fired = false;
        let tail = futures::stream::poll_fn(move |_| {
            if !fired {
                fired = true;
                if let Some(runner) = runner.upgrade() {
                    runner.abort();
                }
            }
            std::task::Poll::Ready(None)
        });
        Box::pin(prefix.chain(tail))
    }
}

/// A complete answer whose stream ends normally, with the abort landing as
/// the stream ends. The run must resolve Interrupted; reporting the answer
/// as a finished turn is what renders a reasoning summary for a run the
/// user stopped.
#[tokio::test]
async fn test_abort_at_answer_boundary() {
    let p = Arc::new(AbortOnStreamEnd::new(vec![
        LlmEvent::StepStart { index: 0 },
        LlmEvent::TextStart { id: "t1".into() },
        LlmEvent::TextDelta {
            id: "t1".into(),
            text: "the answer".into(),
        },
        LlmEvent::TextEnd { id: "t1".into() },
        LlmEvent::StepFinish {
            index: 0,
            reason: "stop".into(),
            usage: None,
        },
        LlmEvent::Finish {
            reason: "stop".into(),
            usage: Some(Usage::default()),
        },
    ]));
    let runner = Arc::new(runner_with(p.clone(), ToolRegistry::new()));
    p.bind(&runner);
    let session = SessionId::new();
    let result = runner.run(session, "hi".into()).await.expect("run ok");
    assert!(
        matches!(result.outcome, RunOutcome::Interrupted(_)),
        "abort observed before the turn was reported ends the run interrupted: {:?}",
        result.outcome
    );
}

/// A verify gate that cancels the run from inside verify, then reports its
/// verdict. The cancel lands in the gate's own window, after the last token
/// check before FinalOutput.
pub(super) struct AbortDuringVerify {
    runner: Mutex<Weak<Runner>>,
    fail: bool,
}

impl AbortDuringVerify {
    pub(super) fn new(fail: bool) -> Self {
        Self {
            runner: Mutex::new(Weak::new()),
            fail,
        }
    }

    /// Point the gate at the runner it must cancel. Called after the runner is
    /// built: the gate is a constructor argument, so the two cannot hold each
    /// other directly.
    pub(super) fn bind(&self, runner: &Arc<Runner>) {
        *self.runner.lock().expect("runner slot") = Arc::downgrade(runner);
    }
}

impl VerifyGate for AbortDuringVerify {
    fn verify(
        &self,
        _session: SessionId,
        _store: &dyn SessionLog,
    ) -> PFut<'_, Result<(), VerifyFailure>> {
        Box::pin(async move {
            if let Some(runner) = self.runner.lock().expect("runner slot").upgrade() {
                runner.abort();
            }
            if self.fail {
                Err(VerifyFailure {
                    checks: vec!["late check failed".into()],
                    suggestions: vec!["fix it".into()],
                })
            } else {
                Ok(())
            }
        })
    }
}

async fn run_with_gate_abort(fail: bool) -> RunOutcome {
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::text("done"));
    let gate = Arc::new(AbortDuringVerify::new(fail));
    let runner = Arc::new(
        runner_with(provider, ToolRegistry::new())
            .with_verify_gate(gate.clone() as Arc<dyn VerifyGate>),
    );
    gate.bind(&runner);
    runner
        .run(SessionId::new(), "hi".into())
        .await
        .expect("run ok")
        .outcome
}

/// A stop that lands while the gate runs outranks the gate's failure: the run
/// the user stopped is interrupted, not reported as a failed verification.
#[tokio::test]
async fn test_abort_outranks_verify_failure() {
    let outcome = run_with_gate_abort(true).await;
    assert!(
        matches!(outcome, RunOutcome::Interrupted(_)),
        "the gate's verdict must not outrank the stop: {outcome:?}"
    );
}

/// The same window with a passing verdict: the run was stopped, so it is
/// interrupted rather than reported as a finished turn.
#[tokio::test]
async fn test_abort_outranks_verify_pass() {
    let outcome = run_with_gate_abort(false).await;
    assert!(
        matches!(outcome, RunOutcome::Interrupted(_)),
        "a stopped run is not a finished turn: {outcome:?}"
    );
}

/// A session log that stops the run when the answer's tail replays the
/// session. The tail is the last await before the answer is reported, and the
/// log is the only collaborator in it a test can drive.
struct StopOnReplayLog {
    runner: Arc<Mutex<Weak<Runner>>>,
    replayed: Arc<AtomicBool>,
}

impl ContextBackend for StopOnReplayLog {
    fn append(&self, event: SessionLogEntry) -> PFut<'_, Result<EventId, ContextError>> {
        let id = event.id;
        Box::pin(async move { Ok(id) })
    }

    fn read_range(
        &self,
        _session: SessionId,
        _from: Option<EventId>,
        _to: Option<EventId>,
    ) -> PFut<'_, Result<Vec<SessionLogEntry>, ContextError>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    fn replay(&self, _session: SessionId) -> PFut<'_, Result<Vec<SessionLogEntry>, ContextError>> {
        self.replayed.store(true, Ordering::Release);
        if let Some(runner) = self.runner.lock().expect("runner slot").upgrade() {
            runner.abort();
        }
        Box::pin(async { Ok(Vec::new()) })
    }

    fn write_checkpoint(
        &self,
        manifest: CheckpointManifest,
    ) -> PFut<'_, Result<CheckpointId, ContextError>> {
        let id = manifest.id;
        Box::pin(async move { Ok(id) })
    }

    fn read_checkpoint(
        &self,
        _id: CheckpointId,
    ) -> PFut<'_, Result<CheckpointManifest, ContextError>> {
        Box::pin(async { Err(ContextError::Unsupported) })
    }

    fn list_checkpoints(
        &self,
        _session: SessionId,
    ) -> PFut<'_, Result<Vec<CheckpointId>, ContextError>> {
        Box::pin(async { Ok(Vec::new()) })
    }
}

/// The extractor whose session read is the tail's own await.
fn tail_extractor(provider: Arc<dyn ModelProvider>) -> (Arc<MemoryExtractor>, PathBuf) {
    let root = std::env::temp_dir().join(format!("abort-tail-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("mkdir");
    let store: Arc<dyn SessionLog> = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let extractor = Arc::new(MemoryExtractor::new(
        store,
        provider,
        Arc::new(MarkdownMemoryProvider::new(root.clone())),
        root.clone(),
        RunnerConfig::default(),
    ));
    (extractor, root)
}

/// A stop that lands while the answer's memory tail runs outranks the answer.
/// The tail replays the session without reading the token, so without a check
/// after it the run the user stopped is reported as a finished turn.
#[tokio::test]
async fn test_abort_outranks_memory_tail() {
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::text("done"));
    let (extractor, root) = tail_extractor(Arc::clone(&provider));
    let slot = Arc::new(Mutex::new(Weak::new()));
    let replayed = Arc::new(AtomicBool::new(false));
    let log: Arc<dyn SessionLog> = Arc::new(SessionStore::new(Box::new(StopOnReplayLog {
        runner: Arc::clone(&slot),
        replayed: Arc::clone(&replayed),
    })));
    let runner = Arc::new(
        runner_with(Arc::clone(&provider), ToolRegistry::new()).install_memory(
            MemoryRuntime::from_parts(
                log,
                None,
                MemoryGates::new(true, false),
                Some(extractor),
                None,
            ),
        ),
    );
    *slot.lock().expect("runner slot") = Arc::downgrade(&runner);
    let outcome = runner
        .run(SessionId::new(), "hi".into())
        .await
        .expect("run ok")
        .outcome;
    // The window exists only if the tail read the session: with the memory
    // work turned off the run never reaches it.
    assert!(
        replayed.load(Ordering::Acquire),
        "the answer's tail did not reach its session read"
    );
    assert!(
        matches!(outcome, RunOutcome::Interrupted(_)),
        "a stop during the answer's tail is not a finished turn: {outcome:?}"
    );
    std::fs::remove_dir_all(&root).ok();
}
