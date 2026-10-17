//! What a model call reports about its own latency, driven end-to-end through
//! the public Runner API. The provider here scripts its events with the delays
//! a real stream has, so the split the durable timing event records can be
//! read against a known shape: a reply that answers with a tool call and no
//! text still has a first token early, and a decode phase after it.
//!
//! The delay is the measurement itself, not a wait for a side effect: the test
//! states how long the provider took before and after its first output, and
//! asserts the recorded split follows that shape.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use futures::future::pending;
use futures::stream::unfold;
use houyicoder_api::provider::ModelProvider;
use houyicoder_api::tool::{Tool, ToolCtx};
use houyicoder_async::{PFut, PStream};
use houyicoder_context::{SessionEvent, SessionId};
use houyicoder_core::agent::runner_config::RunnerConfig;
use houyicoder_core::agent::{RunOutcome, Runner, ToolRegistry};
use houyicoder_memory::InMemoryBackend;
use houyicoder_protocol::extension::ToolError;
use houyicoder_protocol::llm::{
    CompletionRequest, CompletionResponse, LlmEvent, ModelCapabilities, ProviderError, Usage,
};
use houyicoder_session::SessionStore;
use tokio::sync::oneshot;
use tokio::time::{sleep, timeout};

/// A provider that answers the first call with a tool call and every later one
/// with text. Its tool-call reply spends a known delay before the first
/// argument fragment and a longer one after it, so the split is readable.
struct SlowToolProvider {
    calls: AtomicUsize,
}

impl SlowToolProvider {
    fn new() -> Self {
        Self {
            calls: AtomicUsize::new(0),
        }
    }
}

/// The events of one reply, each with the delay before it is sent.
fn tool_call_steps() -> Vec<(u64, LlmEvent)> {
    let (id, name) = ("c1".to_string(), "echo".to_string());
    vec![
        (0, LlmEvent::StepStart { index: 0 }),
        (
            40,
            LlmEvent::ToolInputStart {
                id: id.clone(),
                name: name.clone(),
            },
        ),
        (
            0,
            LlmEvent::ToolInputDelta {
                id: id.clone(),
                name: name.clone(),
                text: "{}".into(),
            },
        ),
        (
            120,
            LlmEvent::ToolInputEnd {
                id: id.clone(),
                name: name.clone(),
            },
        ),
        (
            0,
            LlmEvent::ToolCall {
                id,
                name,
                input: serde_json::json!({}),
            },
        ),
        (
            0,
            LlmEvent::StepFinish {
                index: 0,
                reason: "tool_calls".into(),
                usage: None,
            },
        ),
        (
            0,
            LlmEvent::Finish {
                reason: "tool_calls".into(),
                usage: Some(Usage::default()),
            },
        ),
    ]
}

/// The reply that closes the run once the tool result is in.
fn text_steps() -> Vec<(u64, LlmEvent)> {
    vec![
        (0, LlmEvent::StepStart { index: 0 }),
        (0, LlmEvent::TextStart { id: "t1".into() }),
        (
            0,
            LlmEvent::TextDelta {
                id: "t1".into(),
                text: "done".into(),
            },
        ),
        (0, LlmEvent::TextEnd { id: "t1".into() }),
        (
            0,
            LlmEvent::StepFinish {
                index: 0,
                reason: "stop".into(),
                usage: None,
            },
        ),
        (
            0,
            LlmEvent::Finish {
                reason: "stop".into(),
                usage: Some(Usage::default()),
            },
        ),
    ]
}

impl ModelProvider for SlowToolProvider {
    fn complete(
        &self,
        _req: CompletionRequest,
    ) -> PFut<'_, Result<CompletionResponse, ProviderError>> {
        Box::pin(async {
            Ok(CompletionResponse {
                output: vec![],
                usage: Usage::default(),
                model: "test".into(),
            })
        })
    }

    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }

    fn stream(&self, _req: CompletionRequest) -> PStream<'_, Result<LlmEvent, ProviderError>> {
        let steps = match self.calls.fetch_add(1, Ordering::Relaxed) {
            0 => tool_call_steps(),
            _ => text_steps(),
        };
        let mut steps = steps.into_iter();
        Box::pin(unfold((), move |()| {
            let next = steps.next();
            async move {
                let (delay_ms, event) = next?;
                if delay_ms > 0 {
                    sleep(Duration::from_millis(delay_ms)).await;
                }
                Some((Ok(event), ()))
            }
        }))
    }
}

/// A provider that opens a call's arguments and then stops: the first reply
/// yields the opener and waits, signalling on the way so the test knows the
/// event was consumed. Later replies finish at once, so a run that did not stop
/// still ends rather than waiting on a stream that never resumes.
struct HangingProvider {
    started: Mutex<Option<oneshot::Sender<()>>>,
}

impl HangingProvider {
    fn new(started: oneshot::Sender<()>) -> Self {
        Self {
            started: Mutex::new(Some(started)),
        }
    }
}

impl ModelProvider for HangingProvider {
    fn complete(
        &self,
        _req: CompletionRequest,
    ) -> PFut<'_, Result<CompletionResponse, ProviderError>> {
        Box::pin(async {
            Ok(CompletionResponse {
                output: vec![],
                usage: Usage::default(),
                model: "test".into(),
            })
        })
    }

    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }

    fn stream(&self, _req: CompletionRequest) -> PStream<'_, Result<LlmEvent, ProviderError>> {
        let started = self.started.lock().ok().and_then(|mut guard| guard.take());
        let (id, name) = ("c1".to_string(), "echo".to_string());
        Box::pin(unfold((0u8, started), move |(step, started)| {
            let (id, name) = (id.clone(), name.clone());
            async move {
                match step {
                    0 => Some((Ok(LlmEvent::ToolInputStart { id, name }), (1u8, started))),
                    1 => {
                        if let Some(signal) = started {
                            let _ = signal.send(());
                        }
                        pending().await
                    }
                    _ => Some((
                        Ok(LlmEvent::Finish {
                            reason: "stop".into(),
                            usage: Some(Usage::default()),
                        }),
                        (3u8, None),
                    )),
                }
            }
        }))
    }
}

/// A read-only echo tool, so the scripted call has something to run.
struct EchoTool;

impl Tool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }

    fn description(&self) -> &str {
        "echoes its input back"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }

    fn execute(
        &self,
        _ctx: ToolCtx,
        input: serde_json::Value,
    ) -> PFut<'_, Result<serde_json::Value, ToolError>> {
        Box::pin(async move { Ok(input) })
    }

    fn is_read_only(&self) -> bool {
        true
    }
}

/// A call cancelled inside its argument stream records no timing: the call
/// never reached the point where the split is taken, and a figure would claim a
/// measurement the run did not make. The opener is consumed first, so the
/// cancel lands while the arguments are open rather than before they started.
#[tokio::test]
async fn test_cancelled_call_no_timing() {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let (started_tx, started_rx) = oneshot::channel();
    let runner = Arc::new(Runner::new(
        store,
        Arc::new(HangingProvider::new(started_tx)),
        ToolRegistry::new(),
        RunnerConfig {
            model: "test".into(),
            ..RunnerConfig::default()
        },
    ));
    let session = SessionId::new();
    let run = tokio::spawn({
        let runner = Arc::clone(&runner);
        async move { runner.run(session, "hi into a tool".into()).await }
    });
    timeout(Duration::from_secs(10), started_rx)
        .await
        .expect("the arguments open before the cancel")
        .expect("the stream is still live");

    runner.abort();
    let outcome = timeout(Duration::from_secs(10), run)
        .await
        .expect("the cancel ends the run")
        .expect("the run task did not panic")
        .expect("the run returns rather than failing");
    assert!(
        matches!(outcome.outcome, RunOutcome::Interrupted(_)),
        "the cancel ends the run as interrupted: {:?}",
        outcome.outcome
    );

    let timings: Vec<SessionEvent> = runner
        .store()
        .trajectory_snapshot(session)
        .into_iter()
        .filter(|entry| matches!(entry.event, SessionEvent::ModelStepTiming { .. }))
        .map(|entry| entry.event)
        .collect();
    assert!(
        timings.is_empty(),
        "a cancelled call reports no timing: {timings:?}"
    );
}

/// A reply that answers with a tool call and no text reports its first token
/// at the moment its arguments started arriving, so the decode phase is the
/// time it spent producing them rather than nothing. Reading the clock at the
/// call instead would put the whole call inside the first token.
#[tokio::test]
async fn test_tool_reply_keeps_decode() {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(EchoTool));
    let runner = Runner::new(
        store,
        Arc::new(SlowToolProvider::new()),
        tools,
        RunnerConfig {
            model: "test".into(),
            ..RunnerConfig::default()
        },
    );
    let session = SessionId::new();
    runner
        .run(session, "hi into a tool".into())
        .await
        .expect("the run completes");

    let timing = runner
        .store()
        .trajectory_snapshot(session)
        .into_iter()
        .find_map(|entry| match entry.event {
            SessionEvent::ModelStepTiming {
                ttft_ms,
                decode_ms,
                total_ms,
                ..
            } => Some((ttft_ms, decode_ms, total_ms)),
            _ => None,
        })
        .expect("a model call recorded its timing");
    let (ttft, decode, total) = timing;
    let (ttft, decode) = (
        ttft.expect("the call produced output, so it has a first token"),
        decode.expect("the call ended, so it has a decode phase"),
    );
    assert!(
        ttft < decode,
        "the arguments took longer to produce than the wait before them: \
         ttft {ttft}ms, decode {decode}ms, total {total}ms"
    );
    assert!(
        ttft + decode <= total,
        "the split stays inside the call: ttft {ttft}ms + decode {decode}ms \
         against total {total}ms"
    );
}
