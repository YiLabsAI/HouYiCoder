//! Abort observed at the run's terminal boundary.
//!
//! The token is checked while the model stream is live and after tool
//! dispatch, but the work that follows the stream's last event (usage fill,
//! truncation classification, answer parsing) never reads it. A cancel
//! landing in that window must still end the run as interrupted, not as a
//! successful answer.

use super::runner_with;
use crate::agent::tool::ToolRegistry;
use crate::agent::{RunOutcome, Runner};
use futures::StreamExt;
use houyicoder_api::provider::ModelProvider;
use houyicoder_async::{PFut, PStream};
use houyicoder_context::SessionId;
use houyicoder_protocol::llm::{
    CompletionRequest, CompletionResponse, LlmEvent, ModelCapabilities, ProviderError, Usage,
};
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
