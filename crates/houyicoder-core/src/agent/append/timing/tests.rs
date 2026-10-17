//! Tests for durable model-step timing and context-clear events.
//!
//! These assert the appended event payloads, not just that the append path
//! ran: without the assertions a change that wrote zeroed or mislabelled
//! timing would still pass the suite.

use crate::provider::test_support::FakeProvider;
use houyicoder_context::{SessionEvent, SessionId, SessionLogEntry};
use houyicoder_memory::InMemoryBackend;
use houyicoder_protocol::llm::{CompletionResponse, OutputItem, Usage};
use houyicoder_session::SessionStore;
use std::sync::Arc;

use super::{BlockSpans, is_first_output};
use crate::agent::{Runner, ToolRegistry, runner_config::RunnerConfig};
use houyicoder_protocol::llm::LlmEvent;
use std::time::{Duration, Instant};

/// The timing facts one model call recorded, in the order the tests read them.
#[derive(Debug)]
struct ModelStepTiming {
    turn: u32,
    step: u32,
    total_ms: u64,
    ttft_ms: Option<u64>,
    decode_ms: Option<u64>,
    reasoning_ms: Option<u64>,
    response_ms: Option<u64>,
}

fn test_runner(provider: FakeProvider) -> Arc<Runner> {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    Arc::new(Runner::new(
        store,
        Arc::new(provider),
        ToolRegistry::new(),
        RunnerConfig {
            model: "test".into(),
            ..RunnerConfig::default()
        },
    ))
}

fn response(output: Vec<OutputItem>) -> CompletionResponse {
    CompletionResponse {
        output,
        usage: Usage::default(),
        model: "test".into(),
    }
}

fn timing_events(events: &[SessionLogEntry]) -> Vec<ModelStepTiming> {
    events
        .iter()
        .filter_map(|e| match e.event {
            SessionEvent::ModelStepTiming {
                reasoning_ms,
                response_ms,
                turn,
                step,
                total_ms,
                ttft_ms,
                decode_ms,
            } => Some(ModelStepTiming {
                turn,
                step,
                total_ms,
                ttft_ms,
                decode_ms,
                reasoning_ms,
                response_ms,
            }),
            _ => None,
        })
        .collect()
}

/// A completed model call appends one timing event whose decode time is the
/// remainder after the first token, never larger than the total.
#[tokio::test]
async fn test_model_step_timing_appended() {
    let runner = test_runner(FakeProvider::text("done"));
    let session = SessionId::new();
    runner
        .run(session, "hi".into())
        .await
        .expect("run completes");

    let events = runner.store().trajectory_tail(session);
    let timings = timing_events(&events);
    assert_eq!(timings.len(), 1, "one model call appends one timing event");
    let first = &timings[0];
    assert_eq!(first.turn, 1, "the first model call is turn 1");
    assert_eq!(
        first.step, 1,
        "the first round-trip is step 1, matching TurnUsage"
    );
    // A fake provider answers instantly, so the measured durations are zero.
    // The invariant under test is the split, not the magnitude.
    assert_eq!(
        (first.ttft_ms.is_some(), first.decode_ms.is_some()),
        (true, true),
        "a stream that produced a first token records both ttft and decode"
    );
    let (ttft, decode) = (first.ttft_ms.unwrap_or(0), first.decode_ms.unwrap_or(0));
    assert!(
        ttft + decode <= first.total_ms,
        "ttft {ttft} + decode {decode} must not exceed the total {}",
        first.total_ms
    );
}

/// Each model call in a multi-call run appends its own timing event, so the
/// per-step latency series is complete rather than only the last step's.
#[tokio::test]
async fn test_timing_per_model_call() {
    let provider = FakeProvider::new(vec![
        response(vec![OutputItem::ToolCall {
            id: "c1".into(),
            name: "bash".into(),
            input: serde_json::json!({"command": "true"}),
        }]),
        response(vec![OutputItem::Text {
            text: "done".into(),
        }]),
    ]);
    let runner = test_runner(provider);
    let session = SessionId::new();
    drop(runner.run(session, "hi".into()).await);

    let events = runner.store().trajectory_tail(session);
    let timings = timing_events(&events);
    assert!(
        timings.len() >= 2,
        "a tool round-trip issues a second model call with its own timing: {timings:?}"
    );
    assert!(
        timings
            .iter()
            .all(|t| t.ttft_ms.is_some() && t.decode_ms.is_some()),
        "every appended timing splits into ttft and decode: {timings:?}"
    );
    assert_eq!(
        timings.iter().map(|t| t.step).collect::<Vec<_>>(),
        vec![1, 1],
        "each model call reports its own round-trip index"
    );
}

/// Clearing the session appends exactly one ContextCleared marker carrying the
/// turn count at the moment of the clear.
#[tokio::test]
async fn test_clear_appends_marker() {
    let runner = test_runner(FakeProvider::text("done"));
    let session = SessionId::new();
    runner
        .run(session, "hi".into())
        .await
        .expect("run completes");

    runner.reset_trajectory(session).await;

    let events = runner.store().trajectory_tail(session);
    let cleared: Vec<u32> = events
        .iter()
        .filter_map(|e| match e.event {
            SessionEvent::ContextCleared { prior_turn } => Some(prior_turn),
            _ => None,
        })
        .collect();
    assert_eq!(
        cleared.len(),
        1,
        "one clear appends exactly one marker, visible to an in-process reader"
    );
    assert_eq!(
        cleared[0], 1,
        "the marker records the turn count at the clear"
    );
}

/// A stream that closed its reasoning and reply blocks reports both spans, and
/// two blocks are summed rather than measured end to end.
///
/// The clock is handed in, so the test states exact durations instead of
/// sleeping for them.
#[test]
fn test_spans_sum_closed_blocks() {
    let t0 = Instant::now();
    let at = |ms: u64| t0 + Duration::from_millis(ms);
    let mut spans = BlockSpans::default();
    assert_eq!(spans.reasoning_ms(), None, "nothing closed yet");
    assert_eq!(spans.response_ms(), None, "nothing closed yet");

    spans.note_at(&LlmEvent::ReasoningStart { id: "r".into() }, at(0));
    spans.note_at(&LlmEvent::ReasoningEnd { id: "r".into() }, at(10));
    spans.note_at(&LlmEvent::TextStart { id: "t".into() }, at(10));
    spans.note_at(&LlmEvent::TextEnd { id: "t".into() }, at(35));

    assert_eq!(
        spans.reasoning_ms(),
        Some(10),
        "the closed block's own span"
    );
    assert_eq!(spans.response_ms(), Some(25), "and the reply's");

    spans.note_at(&LlmEvent::ReasoningStart { id: "r".into() }, at(40));
    spans.note_at(&LlmEvent::ReasoningEnd { id: "r".into() }, at(45));
    assert_eq!(
        spans.reasoning_ms(),
        Some(15),
        "a second block adds to the first rather than replacing it"
    );
}

/// A stream that ended inside a block reports neither span: an unfinished span
/// is unknown, and zero would claim the model thought for no time.
#[test]
fn test_spans_open_block_unknown() {
    let mut spans = BlockSpans::default();
    spans.note(&LlmEvent::ReasoningStart { id: "r".into() });
    assert_eq!(
        spans.reasoning_ms(),
        None,
        "a block the call ended inside is not a measurement"
    );

    let mut reply = BlockSpans::default();
    reply.note(&LlmEvent::TextStart { id: "t".into() });
    reply.note(&LlmEvent::TextEnd { id: "t".into() });
    assert_eq!(reply.reasoning_ms(), None, "no reasoning block was opened");
    assert!(
        reply.response_ms().is_some(),
        "and the reply that closed is measured"
    );
}

/// The spans a stream closed reach the durable event: a call that reasoned and
/// then answered records both, and a call that only answered records the reply
/// alone rather than a reasoning span of zero.
#[tokio::test]
async fn test_spans_reach_the_event() {
    let runner = test_runner(FakeProvider::new(vec![CompletionResponse {
        output: vec![
            OutputItem::Reasoning {
                text: "let me think".into(),
            },
            OutputItem::Text {
                text: "done".into(),
            },
        ],
        usage: Usage::default(),
        model: "test".into(),
    }]));
    let session = SessionId::new();
    runner
        .run(session, "hi".into())
        .await
        .expect("run completes");

    let timings = timing_events(&runner.store().trajectory_tail(session));
    assert_eq!(timings.len(), 1, "one model call appends one timing event");
    let first = &timings[0];
    assert!(
        first.reasoning_ms.is_some(),
        "the reasoning block the stream closed is recorded: {first:?}"
    );
    assert!(
        first.response_ms.is_some(),
        "and so is the reply block: {first:?}"
    );
    assert!(
        first.ttft_ms.is_some() && first.decode_ms.is_some(),
        "the split the header already read is unchanged: {first:?}"
    );
}

/// A tool call's argument stream opens the output clock: a reply that answers
/// with a call and no text is timed from the moment its arguments start
/// arriving, not from the closed stream that carries the call. The events that
/// only announce a block or end one do not start it.
#[test]
fn test_tool_input_starts_output() {
    let (id, name) = ("c1".to_string(), "read_file".to_string());
    assert!(
        is_first_output(&LlmEvent::ToolInputStart {
            id: id.clone(),
            name: name.clone()
        }),
        "the fragment that names the call opens its arguments"
    );
    assert!(
        is_first_output(&LlmEvent::ToolInputDelta {
            id: id.clone(),
            name: name.clone(),
            text: "{\"path\":".into()
        }),
        "a later fragment of the same arguments is output too"
    );
    assert!(
        is_first_output(&LlmEvent::ToolCall {
            id: id.clone(),
            name: name.clone(),
            input: serde_json::json!({"path": "a.rs"})
        }),
        "a provider that publishes the call without fragments still counts"
    );
    assert!(
        is_first_output(&LlmEvent::TextDelta {
            id: "t1".into(),
            text: "hi".into()
        }),
        "reply text is unchanged"
    );
    assert!(
        !is_first_output(&LlmEvent::StepStart { index: 0 }),
        "the step opening carries no output"
    );
    assert!(
        !is_first_output(&LlmEvent::TextStart { id: "t1".into() }),
        "announcing a block is not output arriving"
    );
    assert!(
        !is_first_output(&LlmEvent::ToolInputEnd { id, name }),
        "closing the arguments is the end of the output, not its start"
    );
    assert!(
        !is_first_output(&LlmEvent::Finish {
            reason: "stop".into(),
            usage: None
        }),
        "the stream closing is not output either"
    );
}

/// A tool call's arguments close the output block they opened, so the time the
/// model spent producing them is measured alongside a reply's text.
#[test]
fn test_tool_input_closes_output() {
    let t0 = Instant::now();
    let at = |ms: u64| t0 + Duration::from_millis(ms);
    let mut spans = BlockSpans::default();
    spans.note_at(
        &LlmEvent::ToolInputStart {
            id: "c1".into(),
            name: "read_file".into(),
        },
        at(5),
    );
    spans.note_at(
        &LlmEvent::ToolInputEnd {
            id: "c1".into(),
            name: "read_file".into(),
        },
        at(45),
    );
    assert_eq!(
        spans.response_ms(),
        Some(40),
        "the arguments' own span reaches the output time"
    );

    spans.note_at(&LlmEvent::TextStart { id: "t1".into() }, at(50));
    spans.note_at(&LlmEvent::TextEnd { id: "t1".into() }, at(60));
    assert_eq!(
        spans.response_ms(),
        Some(50),
        "a reply block adds to it, the way two reasoning blocks sum"
    );
}

/// Two calls streaming their arguments at the same time are one output span,
/// measured from the first opener to the last closer. Reading the clock at each
/// opener would measure the last call alone and lose the time the first spent.
#[test]
fn test_tool_calls_share_span() {
    let t0 = Instant::now();
    let at = |ms: u64| t0 + Duration::from_millis(ms);
    let mut spans = BlockSpans::default();
    spans.note_at(
        &LlmEvent::ToolInputStart {
            id: "c1".into(),
            name: "read_file".into(),
        },
        at(0),
    );
    spans.note_at(
        &LlmEvent::ToolInputStart {
            id: "c2".into(),
            name: "grep".into(),
        },
        at(30),
    );
    spans.note_at(
        &LlmEvent::ToolInputEnd {
            id: "c1".into(),
            name: "read_file".into(),
        },
        at(60),
    );
    assert_eq!(
        spans.response_ms(),
        None,
        "one call still streaming keeps the span open"
    );
    spans.note_at(
        &LlmEvent::ToolInputEnd {
            id: "c2".into(),
            name: "grep".into(),
        },
        at(60),
    );
    assert_eq!(
        spans.response_ms(),
        Some(60),
        "the span runs from the first opener to the last closer, counted once"
    );
}

/// A call whose stream ended inside its arguments reports no output time: the
/// block never closed, and zero would claim the model produced nothing.
#[test]
fn test_open_tool_input_unknown() {
    let mut spans = BlockSpans::default();
    spans.note(&LlmEvent::ToolInputStart {
        id: "c1".into(),
        name: "read_file".into(),
    });
    spans.note(&LlmEvent::ToolInputDelta {
        id: "c1".into(),
        name: "read_file".into(),
        text: "{\"path\":".into(),
    });
    assert_eq!(
        spans.response_ms(),
        None,
        "an argument stream the call ended inside is not a measurement"
    );
}

/// A call that never reasoned records no reasoning span: the field stays
/// unknown, which is not the same as a model that thought for no time.
#[tokio::test]
async fn test_no_reasoning_stays_unknown() {
    let runner = test_runner(FakeProvider::text("done"));
    let session = SessionId::new();
    runner
        .run(session, "hi".into())
        .await
        .expect("run completes");

    let timings = timing_events(&runner.store().trajectory_tail(session));
    let first = &timings[0];
    assert_eq!(
        first.reasoning_ms, None,
        "no reasoning block was opened, so the span is unknown"
    );
    assert!(
        first.response_ms.is_some(),
        "the reply that closed is measured: {first:?}"
    );
}
