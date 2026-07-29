//! Length-truncation recovery tests. The drive loop's finish_reason
//! "length" path: resume-direct recovery (bounded), exhaustion marker,
//! and the no-answer (reasoning-only) note.

use super::runner_with;
use super::*;
use houyicoder_protocol::llm::CompletionRequest;

/// A provider whose stream emits the given LlmEvents then ends, so a run can
/// complete against a raw event sequence. Used to inject a finish_reason the
/// canned stream path always emits as stop (e.g. length truncation).
struct RawProvider {
    events: Vec<houyicoder_protocol::llm::LlmEvent>,
}

impl RawProvider {
    fn new(events: Vec<houyicoder_protocol::llm::LlmEvent>) -> Self {
        Self { events }
    }
}

impl ModelProvider for RawProvider {
    fn complete(
        &self,
        _req: CompletionRequest,
    ) -> houyicoder_async::PFut<'_, Result<CompletionResponse, ProviderError>> {
        Box::pin(async {
            Ok(CompletionResponse {
                output: vec![],
                usage: Usage::default(),
                model: "test".into(),
            })
        })
    }
    fn capabilities(&self) -> houyicoder_protocol::llm::ModelCapabilities {
        houyicoder_protocol::llm::ModelCapabilities::default()
    }
    fn stream(
        &self,
        _req: CompletionRequest,
    ) -> houyicoder_async::PStream<
        '_,
        Result<houyicoder_protocol::llm::LlmEvent, houyicoder_protocol::llm::ProviderError>,
    > {
        Box::pin(futures::stream::iter(
            self.events.clone().into_iter().map(Ok),
        ))
    }
}

/// A provider that pops a different raw event sequence per stream call (last
/// script repeats). Lets a test script a length-cut on call 1 then a natural
/// stop on call 2 to exercise the resume-direct recovery loop end-to-end.
pub(crate) struct ScriptRawProvider {
    scripts: std::sync::Mutex<Vec<Vec<houyicoder_protocol::llm::LlmEvent>>>,
    next: std::sync::atomic::AtomicUsize,
}

impl ScriptRawProvider {
    pub(crate) fn new(scripts: Vec<Vec<houyicoder_protocol::llm::LlmEvent>>) -> Self {
        Self {
            scripts: std::sync::Mutex::new(scripts),
            next: std::sync::atomic::AtomicUsize::new(0),
        }
    }
}

impl ModelProvider for ScriptRawProvider {
    fn complete(
        &self,
        _req: CompletionRequest,
    ) -> houyicoder_async::PFut<'_, Result<CompletionResponse, ProviderError>> {
        Box::pin(async {
            Ok(CompletionResponse {
                output: vec![],
                usage: Usage::default(),
                model: "test".into(),
            })
        })
    }
    fn capabilities(&self) -> houyicoder_protocol::llm::ModelCapabilities {
        houyicoder_protocol::llm::ModelCapabilities::default()
    }
    fn stream(
        &self,
        _req: CompletionRequest,
    ) -> houyicoder_async::PStream<
        '_,
        Result<houyicoder_protocol::llm::LlmEvent, houyicoder_protocol::llm::ProviderError>,
    > {
        let idx = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let events = {
            let guard = self.scripts.lock().expect("script mutex");
            // Last script repeats so an under-scripted test fails bounded,
            // not with an empty stream.
            guard[idx.min(guard.len() - 1)].clone()
        };
        Box::pin(futures::stream::iter(events.into_iter().map(Ok)))
    }
}

#[tokio::test]
async fn test_max_tokens_alias_recovers() {
    // A provider that spells the cap-cut finish reason max_tokens (the
    // Anthropic dialect) instead of length must trigger the same recovery:
    // partial + nudge + continuation, no truncation marker (bug-log #29 —
    // the unnormalized alias silently disabled recovery and the reply
    // rendered cut mid-sentence).
    use houyicoder_protocol::llm::LlmEvent;
    let p = Arc::new(ScriptRawProvider::new(vec![
        vec![
            LlmEvent::StepStart { index: 0 },
            LlmEvent::TextStart { id: "t1".into() },
            LlmEvent::TextDelta {
                id: "t1".into(),
                text: "partial".into(),
            },
            LlmEvent::TextEnd { id: "t1".into() },
            LlmEvent::Finish {
                reason: "max_tokens".into(),
                usage: None,
            },
        ],
        vec![
            LlmEvent::StepStart { index: 0 },
            LlmEvent::TextStart { id: "t2".into() },
            LlmEvent::TextDelta {
                id: "t2".into(),
                text: " continued".into(),
            },
            LlmEvent::TextEnd { id: "t2".into() },
            LlmEvent::Finish {
                reason: "end_turn".into(),
                usage: None,
            },
        ],
    ]));
    let runner = runner_with(p, ToolRegistry::new());
    let session = SessionId::new();
    let result = runner.run(session, "hi".into()).await.expect("run");
    let text = match result.outcome {
        RunOutcome::FinalOutput(t) => t,
        _ => panic!("expected final output, got {:?}", result.outcome),
    };
    assert_eq!(text, " continued", "recovery must fire on max_tokens");
    assert!(
        !text.contains("truncated at the token cap"),
        "no marker when recovery succeeded, got: {text}"
    );
}

#[tokio::test]
async fn test_length_recovery_resumes_completes() {
    // Call 1: the model streams a partial reply then hits the token cap
    // (finish_reason length). The drive loop must append the partial + a
    // resume-direct nudge, then re-call. Call 2: the model continues with a
    // natural stop. The session must hold partial + nudge + continuation,
    // the nudge must NOT surface as a readable transcript line, and the final
    // output must be the continuation with NO truncation marker (recovery
    // succeeded, so the notice is withheld).
    use houyicoder_context::TurnEventKind;
    use houyicoder_protocol::llm::LlmEvent;
    let p = Arc::new(ScriptRawProvider::new(vec![
        vec![
            LlmEvent::StepStart { index: 0 },
            LlmEvent::TextStart { id: "t1".into() },
            LlmEvent::TextDelta {
                id: "t1".into(),
                text: "partial".into(),
            },
            LlmEvent::TextEnd { id: "t1".into() },
            LlmEvent::Finish {
                reason: "length".into(),
                usage: None,
            },
        ],
        vec![
            LlmEvent::StepStart { index: 0 },
            LlmEvent::TextStart { id: "t2".into() },
            LlmEvent::TextDelta {
                id: "t2".into(),
                text: " continued".into(),
            },
            LlmEvent::TextEnd { id: "t2".into() },
            LlmEvent::Finish {
                reason: "stop".into(),
                usage: None,
            },
        ],
    ]));
    let runner = runner_with(p, ToolRegistry::new());
    let session = SessionId::new();
    let result = runner.run(session, "hi".into()).await.expect("run");
    // Recovery succeeded: the final output is the continuation, with no
    // truncation marker (the notice is withheld while recovery can succeed).
    let text = match result.outcome {
        RunOutcome::FinalOutput(t) => t,
        _ => panic!("expected final output, got {:?}", result.outcome),
    };
    assert_eq!(text, " continued");
    assert!(
        !text.contains("truncated at the token cap"),
        "no marker when recovery succeeded, got: {text}"
    );
    // The session holds the partial, a resume nudge (MetaUser, not UserInput),
    // and the continuation. The nudge must be MetaUser so it never surfaces as
    // a readable transcript line.
    let replay = runner.store().replay(session).await.expect("replay");
    let mut kinds = replay.iter().map(|e| &e.kind);
    let has_partial = kinds
        .by_ref()
        .any(|k| matches!(k, TurnEventKind::AssistantMessage { text, .. } if text == "partial"));
    let has_nudge = kinds.by_ref().any(|k| {
        matches!(
            k,
            TurnEventKind::MetaUser { text } if text.contains("Resume directly")
        )
    });
    let has_continuation = kinds.any(|k| {
        matches!(
            k,
            TurnEventKind::AssistantMessage { text, .. } if text == " continued"
        )
    });
    assert!(has_partial, "partial assistant message must persist");
    assert!(has_nudge, "resume nudge (MetaUser) must be in the log");
    assert!(has_continuation, "continuation must persist");
    // The nudge is MetaUser, never UserInput — so the host skips it in the
    // readable transcript.
    let leak = replay.iter().any(|e| {
        matches!(&e.kind, TurnEventKind::UserInput { text } if text.contains("Resume directly"))
    });
    assert!(!leak, "nudge must be MetaUser, not a readable UserInput");
    // Bounded: exactly 2 provider calls (1 partial + 1 continuation).
    assert_eq!(
        result.turns, 1,
        "recovery happens within one turn, not extra turns"
    );
}

#[tokio::test]
async fn test_stop_finish_no_recovery() {
    // A natural stop finish must NOT trigger recovery: no nudge appended, the
    // reply returns directly. Guards against the recovery loop firing on
    // every turn.
    use houyicoder_context::TurnEventKind;
    use houyicoder_protocol::llm::LlmEvent;
    let p = Arc::new(RawProvider::new(vec![
        LlmEvent::StepStart { index: 0 },
        LlmEvent::TextStart { id: "t1".into() },
        LlmEvent::TextDelta {
            id: "t1".into(),
            text: "a complete reply".into(),
        },
        LlmEvent::TextEnd { id: "t1".into() },
        LlmEvent::Finish {
            reason: "stop".into(),
            usage: None,
        },
    ]));
    let runner = runner_with(p, ToolRegistry::new());
    let session = SessionId::new();
    let result = runner.run(session, "hi".into()).await.expect("run");
    let text = match result.outcome {
        RunOutcome::FinalOutput(t) => t,
        _ => panic!("expected final output, got {:?}", result.outcome),
    };
    assert_eq!(text, "a complete reply");
    assert!(
        !text.contains("truncated"),
        "stop finish must not surface a truncation marker: {text}"
    );
    let replay = runner.store().replay(session).await.expect("replay");
    let has_nudge = replay.iter().any(
        |e| matches!(&e.kind, TurnEventKind::MetaUser { text } if text.contains("Resume directly")),
    );
    assert!(!has_nudge, "stop finish must not append a resume nudge");
}

#[tokio::test]
async fn test_silent_trunc_recovers() {
    // A proxy cuts the stream at the token cap but signals "stop" instead of
    // "length". The silent-truncation heuristic must catch this via two
    // signals: output_tokens at the cap (token-cap proximity) and an odd
    // triple-backtick count (open code fence). Either suffices; both fire
    // here. The heuristic synthesizes "length" so the existing resume loop
    // appends the partial + nudge and re-calls. Call 2 is a clean stop with
    // low tokens and an even fence count — no synthesis, natural end.
    use houyicoder_context::TurnEventKind;
    use houyicoder_protocol::llm::LlmEvent;
    let p = Arc::new(ScriptRawProvider::new(vec![
        vec![
            LlmEvent::StepStart { index: 0 },
            LlmEvent::TextStart { id: "t1".into() },
            LlmEvent::TextDelta {
                id: "t1".into(),
                text: "```rust\nfn main() {".into(),
            },
            LlmEvent::TextEnd { id: "t1".into() },
            LlmEvent::Finish {
                reason: "stop".into(),
                usage: Some(Usage {
                    output_tokens: 8_000,
                    ..Default::default()
                }),
            },
        ],
        vec![
            LlmEvent::StepStart { index: 0 },
            LlmEvent::TextStart { id: "t2".into() },
            LlmEvent::TextDelta {
                id: "t2".into(),
                text: " continued".into(),
            },
            LlmEvent::TextEnd { id: "t2".into() },
            LlmEvent::Finish {
                reason: "stop".into(),
                usage: Some(Usage {
                    output_tokens: 10,
                    ..Default::default()
                }),
            },
        ],
    ]));
    let runner = runner_with(p, ToolRegistry::new());
    let session = SessionId::new();
    let result = runner.run(session, "hi".into()).await.expect("run");
    let text = match result.outcome {
        RunOutcome::FinalOutput(t) => t,
        _ => panic!("expected final output, got {:?}", result.outcome),
    };
    assert_eq!(text, " continued");
    assert!(
        !text.contains("truncated at the token cap"),
        "recovery succeeded, no marker expected, got: {text}"
    );
    let replay = runner.store().replay(session).await.expect("replay");
    let has_partial = replay.iter().any(|e| {
        matches!(
            &e.kind,
            TurnEventKind::AssistantMessage { text, .. } if text.contains("fn main")
        )
    });
    let has_nudge = replay.iter().any(|e| {
        matches!(
            &e.kind,
            TurnEventKind::MetaUser { text } if text.contains("Resume directly")
        )
    });
    let has_continuation = replay.iter().any(|e| {
        matches!(
            &e.kind,
            TurnEventKind::AssistantMessage { text, .. } if text == " continued"
        )
    });
    assert!(has_partial, "partial assistant message must persist");
    assert!(has_nudge, "resume nudge must be in the log");
    assert!(has_continuation, "continuation must persist");
}

#[tokio::test]
async fn test_stop_clean_skips_recovery() {
    // A clean stop with low output tokens and an even fence count must NOT
    // trigger the silent-truncation heuristic. No nudge appended, the reply
    // returns directly. Guards against false-positive recovery on every
    // normal turn.
    use houyicoder_context::TurnEventKind;
    use houyicoder_protocol::llm::LlmEvent;
    let p = Arc::new(RawProvider::new(vec![
        LlmEvent::StepStart { index: 0 },
        LlmEvent::TextStart { id: "t1".into() },
        LlmEvent::TextDelta {
            id: "t1".into(),
            text: "a complete reply".into(),
        },
        LlmEvent::TextEnd { id: "t1".into() },
        LlmEvent::Finish {
            reason: "stop".into(),
            usage: Some(Usage {
                output_tokens: 10,
                ..Default::default()
            }),
        },
    ]));
    let runner = runner_with(p, ToolRegistry::new());
    let session = SessionId::new();
    let result = runner.run(session, "hi".into()).await.expect("run");
    let text = match result.outcome {
        RunOutcome::FinalOutput(t) => t,
        _ => panic!("expected final output, got {:?}", result.outcome),
    };
    assert_eq!(text, "a complete reply");
    assert!(
        !text.contains("truncated"),
        "clean stop must not surface a marker: {text}"
    );
    let replay = runner.store().replay(session).await.expect("replay");
    let has_nudge = replay.iter().any(
        |e| matches!(&e.kind, TurnEventKind::MetaUser { text } if text.contains("Resume directly")),
    );
    assert!(!has_nudge, "clean stop must not append a resume nudge");
}

#[tokio::test]
async fn test_length_finish_truncation_marker() {
    // A length finish_reason means the provider cut the reply at the token
    // cap mid-sentence. The fold must append a visible marker so the cut is
    // not silent -- the caller cannot otherwise tell length from stop, so a
    // truncated reply would read as a complete one.
    use houyicoder_protocol::llm::LlmEvent;
    let p = Arc::new(RawProvider::new(vec![
        LlmEvent::StepStart { index: 0 },
        LlmEvent::TextStart { id: "t1".into() },
        LlmEvent::TextDelta {
            id: "t1".into(),
            text: "partial reply cut short".into(),
        },
        LlmEvent::TextEnd { id: "t1".into() },
        LlmEvent::Finish {
            reason: "length".into(),
            usage: None,
        },
    ]));
    let runner = runner_with(p, ToolRegistry::new());
    let session = SessionId::new();
    let result = runner.run(session, "hi".into()).await.expect("run");
    let text = match result.outcome {
        RunOutcome::FinalOutput(t) => t,
        _ => panic!("expected final output, got {:?}", result.outcome),
    };
    assert!(
        text.contains("truncated at the token cap"),
        "length finish must surface a truncation marker, got: {text}"
    );
    assert!(
        text.contains("partial reply cut short"),
        "the streamed text must precede the marker, got: {text}"
    );
}

#[tokio::test]
async fn test_length_finish_surfaces_note() {
    // finish_reason length with NO visible answer: reasoning (or the model
    // itself) consumed the budget before any content landed. The fold must
    // surface a note rather than return an empty FinalOutput that reads as a
    // complete (if terse) reply.
    use houyicoder_protocol::llm::LlmEvent;
    let p = Arc::new(RawProvider::new(vec![
        LlmEvent::StepStart { index: 0 },
        LlmEvent::ReasoningStart { id: "r1".into() },
        LlmEvent::ReasoningDelta {
            id: "r1".into(),
            text: "thinking hard about the answer".into(),
        },
        LlmEvent::ReasoningEnd { id: "r1".into() },
        LlmEvent::Finish {
            reason: "length".into(),
            usage: None,
        },
    ]));
    let runner = runner_with(p, ToolRegistry::new());
    let session = SessionId::new();
    let result = runner.run(session, "hi".into()).await.expect("run");
    let text = match result.outcome {
        RunOutcome::FinalOutput(t) => t,
        _ => panic!("expected final output, got {:?}", result.outcome),
    };
    assert!(
        !text.is_empty(),
        "length finish with no answer must surface a note, not an empty reply"
    );
    assert!(
        text.contains("token cap"),
        "the no-answer note must mention the token cap, got: {text}"
    );
}

#[tokio::test]
async fn test_stop_reply_skips_recovery() {
    // A short reply that finishes "stop" with no server-reported usage must
    // NOT synthesize a length cut: the shared Tokenizer fallback counts the
    // short text, finds it well under the cap, and the run completes normally.
    // Guards against a regression that false-fires recovery on every short
    // reply when the proxy omits usage (the path the silent-truncation
    // heuristic's else branch covers).
    use houyicoder_protocol::llm::LlmEvent;
    let p = Arc::new(ScriptRawProvider::new(vec![vec![
        LlmEvent::StepStart { index: 0 },
        LlmEvent::TextStart { id: "t1".into() },
        LlmEvent::TextDelta {
            id: "t1".into(),
            text: "a short reply".into(),
        },
        LlmEvent::TextEnd { id: "t1".into() },
        LlmEvent::Finish {
            reason: "stop".into(),
            usage: None,
        },
    ]]));
    let runner = runner_with(p, ToolRegistry::new());
    let session = SessionId::new();
    let result = runner.run(session, "hi".into()).await.expect("run");
    let text = match result.outcome {
        RunOutcome::FinalOutput(t) => t,
        _ => panic!("expected final output, got {:?}", result.outcome),
    };
    assert_eq!(text, "a short reply");
    assert!(
        !text.contains("truncated"),
        "no truncation marker on a short stop reply: {text}"
    );
}

#[tokio::test]
async fn test_recovery_verdict_preserves_dialect() {
    // The verdict's raw_finish_reason must carry the provider's original
    // dialect (max_tokens) before normalization, while normalized_reason
    // carries the flattened form (length) the loop keys on. Recovery fires
    // on call 1 (recovery_fired true, recovery_attempts 1); call 2 is a clean
    // stop (recovery_fired false). Two verdicts in the replay.
    use houyicoder_context::{TruncationSignal, TurnEventKind};
    use houyicoder_protocol::llm::LlmEvent;
    let p = Arc::new(ScriptRawProvider::new(vec![
        vec![
            LlmEvent::StepStart { index: 0 },
            LlmEvent::TextStart { id: "t1".into() },
            LlmEvent::TextDelta {
                id: "t1".into(),
                text: "partial".into(),
            },
            LlmEvent::TextEnd { id: "t1".into() },
            LlmEvent::Finish {
                reason: "max_tokens".into(),
                usage: None,
            },
        ],
        vec![
            LlmEvent::StepStart { index: 0 },
            LlmEvent::TextStart { id: "t2".into() },
            LlmEvent::TextDelta {
                id: "t2".into(),
                text: " continued".into(),
            },
            LlmEvent::TextEnd { id: "t2".into() },
            LlmEvent::Finish {
                reason: "end_turn".into(),
                usage: None,
            },
        ],
    ]));
    let runner = runner_with(p, ToolRegistry::new());
    let session = SessionId::new();
    runner.run(session, "hi".into()).await.expect("run");
    let replay = runner.store().replay(session).await.expect("replay");
    let verdicts: Vec<&TurnEventKind> = replay
        .iter()
        .map(|e| &e.kind)
        .filter(|k| matches!(k, TurnEventKind::TruncationVerdict { .. }))
        .collect();
    assert_eq!(verdicts.len(), 2, "one verdict per recovery + final turn");
    // First verdict: recovery fired, raw dialect preserved.
    let first = verdicts[0];
    let (raw, norm, fired, attempts) = match first {
        TurnEventKind::TruncationVerdict {
            raw_finish_reason,
            normalized_reason,
            recovery_fired,
            recovery_attempts,
            ..
        } => (
            raw_finish_reason.as_deref(),
            normalized_reason.as_deref(),
            *recovery_fired,
            *recovery_attempts,
        ),
        _ => unreachable!(),
    };
    assert_eq!(
        raw,
        Some("max_tokens"),
        "raw dialect must survive normalize"
    );
    assert_eq!(norm, Some("length"), "normalized is the flattened form");
    assert!(fired, "first verdict records a recovery that fired");
    assert_eq!(attempts, 1, "first recovery attempt is 1");
    // Second verdict: clean success, no recovery, raw stop preserved.
    let second = verdicts[1];
    let (raw, norm, fired, signal) = match second {
        TurnEventKind::TruncationVerdict {
            raw_finish_reason,
            normalized_reason,
            recovery_fired,
            signal,
            ..
        } => (
            raw_finish_reason.as_deref(),
            normalized_reason.as_deref(),
            *recovery_fired,
            *signal,
        ),
        _ => unreachable!(),
    };
    assert_eq!(
        raw,
        Some("end_turn"),
        "final verdict raw is the stop dialect"
    );
    assert_eq!(norm, Some("end_turn"), "no normalization on a stop reason");
    assert!(!fired, "final verdict is terminal, not a recovery");
    assert_eq!(signal, TruncationSignal::None, "no signal on a clean stop");
}

#[tokio::test]
async fn test_stop_verdict_signals_none() {
    // A clean stop with low tokens must emit exactly one verdict with no
    // signal and recovery_fired false. Guards against false-positive signal
    // classification on every normal turn.
    use houyicoder_context::{TruncationSignal, TurnEventKind};
    use houyicoder_protocol::llm::LlmEvent;
    let p = Arc::new(RawProvider::new(vec![
        LlmEvent::StepStart { index: 0 },
        LlmEvent::TextStart { id: "t1".into() },
        LlmEvent::TextDelta {
            id: "t1".into(),
            text: "a complete reply".into(),
        },
        LlmEvent::TextEnd { id: "t1".into() },
        LlmEvent::Finish {
            reason: "stop".into(),
            usage: Some(Usage {
                output_tokens: 10,
                ..Default::default()
            }),
        },
    ]));
    let runner = runner_with(p, ToolRegistry::new());
    let session = SessionId::new();
    runner.run(session, "hi".into()).await.expect("run");
    let replay = runner.store().replay(session).await.expect("replay");
    let verdicts: Vec<&TurnEventKind> = replay
        .iter()
        .map(|e| &e.kind)
        .filter(|k| matches!(k, TurnEventKind::TruncationVerdict { .. }))
        .collect();
    assert_eq!(verdicts.len(), 1, "exactly one verdict on a clean turn");
    let (signal, fired) = match verdicts[0] {
        TurnEventKind::TruncationVerdict {
            signal,
            recovery_fired,
            ..
        } => (*signal, *recovery_fired),
        _ => unreachable!(),
    };
    assert_eq!(signal, TruncationSignal::None, "no signal on clean stop");
    assert!(!fired, "clean stop is terminal, not a recovery");
}

#[tokio::test]
async fn test_verdict_records_truncation_signal() {
    // A proxy that cuts at the cap but signals stop must emit a verdict
    // whose signal is ServerUsageNearCap (the server-reported count reached
    // the cap). The raw finish_reason stays stop (the proxy's dialect),
    // the normalized becomes length (synthesized), and recovery fires.
    use houyicoder_context::{TruncationSignal, TurnEventKind};
    use houyicoder_protocol::llm::LlmEvent;
    let p = Arc::new(ScriptRawProvider::new(vec![
        vec![
            LlmEvent::StepStart { index: 0 },
            LlmEvent::TextStart { id: "t1".into() },
            LlmEvent::TextDelta {
                id: "t1".into(),
                text: "```rust\nfn main() {".into(),
            },
            LlmEvent::TextEnd { id: "t1".into() },
            LlmEvent::Finish {
                reason: "stop".into(),
                usage: Some(Usage {
                    output_tokens: 8_000,
                    ..Default::default()
                }),
            },
        ],
        vec![
            LlmEvent::StepStart { index: 0 },
            LlmEvent::TextStart { id: "t2".into() },
            LlmEvent::TextDelta {
                id: "t2".into(),
                text: " continued".into(),
            },
            LlmEvent::TextEnd { id: "t2".into() },
            LlmEvent::Finish {
                reason: "stop".into(),
                usage: Some(Usage {
                    output_tokens: 10,
                    ..Default::default()
                }),
            },
        ],
    ]));
    let runner = runner_with(p, ToolRegistry::new());
    let session = SessionId::new();
    runner.run(session, "hi".into()).await.expect("run");
    let replay = runner.store().replay(session).await.expect("replay");
    let verdicts: Vec<&TurnEventKind> = replay
        .iter()
        .map(|e| &e.kind)
        .filter(|k| matches!(k, TurnEventKind::TruncationVerdict { .. }))
        .collect();
    assert_eq!(verdicts.len(), 2, "recovery verdict + final verdict");
    let first = verdicts[0];
    let (raw, norm, signal, server_tokens, fired) = match first {
        TurnEventKind::TruncationVerdict {
            raw_finish_reason,
            normalized_reason,
            signal,
            server_output_tokens,
            recovery_fired,
            ..
        } => (
            raw_finish_reason.as_deref(),
            normalized_reason.as_deref(),
            *signal,
            *server_output_tokens,
            *recovery_fired,
        ),
        _ => unreachable!(),
    };
    assert_eq!(raw, Some("stop"), "raw is the proxy dialect");
    assert_eq!(norm, Some("length"), "normalized is the synthesized length");
    assert_eq!(
        signal,
        TruncationSignal::ServerUsageNearCap,
        "server usage near cap detected"
    );
    assert_eq!(server_tokens, 8_000, "server-reported count preserved");
    assert!(fired, "recovery fired on the silent truncation");
}
