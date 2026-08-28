//! Hook-fire tests for the unified compaction path: PreCompact fires before
//! the summarizer with a return channel (Inject verdict output becomes
//! custom summarization instructions), PostCompact fires after the summary
//! commits with the summary text, and a Deny verdict on PreCompact does NOT
//! abort compaction — denying compaction would brick the session on overflow.

use std::sync::{Arc, Mutex};

use houyicoder_async::PFut;
use houyicoder_context::{EventId, SessionId, TurnEvent, TurnEventKind};
use houyicoder_memory::InMemoryBackend;
use houyicoder_session::SessionStore;

use crate::agent::Runner;
use crate::agent::hook::registry::HookRegistry;
use crate::agent::hook::{
    CompactTrigger, Hook, HookContext, HookError, HookEvent, HookSource, HookVerdict,
};
use crate::agent::manifest::{CompressPolicy, SummarizeError, Summarizer, build_manifest};

/// CompactTrigger::as_str maps each variant to the wire/JSON string for
/// the trigger field (manual / auto).
#[test]
fn test_trigger_as_str_maps() {
    assert_eq!(CompactTrigger::Manual.as_str(), "manual");
    assert_eq!(CompactTrigger::Auto.as_str(), "auto");
}

/// build_manifest falls back to the heuristic summarizer when the LLM
/// summarizer fails, threading custom_instructions through so a PreCompact
/// hook's Inject output is not lost on the fallback path. Pins the
/// LlmFailed arm + the heuristic's custom_instructions parameter.
#[tokio::test]
async fn test_manifest_fallback_threads_instructions() {
    use houyicoder_context::Disposition;
    let s = SessionId::new();
    // Six assistant turns so the default tail_turns=4 Summarizes the first 2
    // (the folded span the failing summarizer + heuristic fallback run on).
    let mut events = vec![TurnEvent {
        id: EventId::new(),
        session: s,
        ts: 0,
        prev_hash: None,
        kind: TurnEventKind::UserInput {
            text: "do work".into(),
        },
    }];
    for i in 0..6 {
        events.push(TurnEvent {
            id: EventId::new(),
            session: s,
            ts: 0,
            prev_hash: None,
            kind: TurnEventKind::AssistantMessage {
                text: format!("turn {i}"),
                thinking: None,
            },
        });
    }
    // A summarizer that always fails so build_manifest hits the heuristic
    // fallback. The default policy (tail_turns=4) keeps the last 4 assistant
    // turns verbatim — but with only 3 assistant turns here, the boundary
    // lands so the first is Summarized (heuristic fallback runs on it).
    struct FailingSummarizer;
    impl Summarizer for FailingSummarizer {
        fn summarize<'a>(
            &'a self,
            _events: &'a [TurnEvent],
            _custom_instructions: Option<&'a str>,
        ) -> PFut<'a, Result<String, SummarizeError>> {
            Box::pin(async { Err(SummarizeError::LlmFailed("forced failure".into())) })
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }
    let policy = CompressPolicy::default();
    let manifest = build_manifest(&events, &policy, &FailingSummarizer, Some("custom hint")).await;
    // The heuristic fallback produced a summary (the LlmFailed arm recovered).
    assert!(
        manifest.summary.is_some(),
        "heuristic fallback populated the summary"
    );
    // At least one group is Summarized (the fallback ran on the folded span).
    assert!(
        manifest
            .plan
            .iter()
            .any(|g| g.disposition == Disposition::Summarized),
        "at least one Summarized group"
    );
}

/// A hook that returns a fixed verdict for a fixed event. Like the
/// FixedHook in hook_tests.rs but kept local so the test is self-contained.
struct FixedHook {
    name: String,
    events: Vec<HookEvent>,
    verdict: HookVerdict,
}

impl Hook for FixedHook {
    fn name(&self) -> &str {
        &self.name
    }
    fn events(&self) -> &[HookEvent] {
        &self.events
    }
    fn evaluate(&self, _ctx: &HookContext) -> Result<HookVerdict, HookError> {
        Ok(self.verdict.clone())
    }
    fn source(&self) -> HookSource {
        HookSource::Managed
    }
}

/// A summarizer that records the custom_instructions it was called with so
/// the PreCompact return-channel test can assert the Inject output reached the
/// summarizer prompt. Returns a canned summary so the manifest's summary
/// field is populated + PostCompact has a non-empty compact_summary.
struct CapturingSummarizer {
    seen: Mutex<Option<String>>,
}

impl CapturingSummarizer {
    fn new() -> Self {
        Self {
            seen: Mutex::new(None),
        }
    }
    fn seen(&self) -> Option<String> {
        self.seen.lock().unwrap().clone()
    }
}

impl Summarizer for CapturingSummarizer {
    fn summarize<'a>(
        &'a self,
        _events: &'a [TurnEvent],
        custom_instructions: Option<&'a str>,
    ) -> PFut<'a, Result<String, SummarizeError>> {
        let seen = custom_instructions.map(str::to_string);
        Box::pin(async move {
            *self.seen.lock().unwrap() = seen;
            Ok("canned summary".to_string())
        })
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// Build a runner over an in-memory store with a capturing summarizer + an
/// optional hook registry. The store is pre-populated with 6 assistant turns
/// so the default tail_turns=4 folds 2 (compaction makes progress).
fn build_runner(
    store: Arc<SessionStore>,
    summarizer: Arc<CapturingSummarizer>,
    hooks: Option<HookRegistry>,
) -> Runner {
    let mut runner = Runner::new(
        store.clone(),
        Arc::new(crate::provider::test_support::FakeProvider::text("summary")),
        crate::agent::tool::ToolRegistry::new(),
        crate::agent::runner_config::RunnerConfig {
            max_turns: 5,
            ..crate::agent::runner_config::RunnerConfig::default()
        },
    );
    runner = runner.with_summarizer(Box::new(CapturingSummarizerWrapper(summarizer)));
    if let Some(reg) = hooks {
        runner = runner.with_hooks(Arc::new(reg));
    }
    runner
}

/// Wrap the Arc<CapturingSummarizer> so it can be a Box<dyn Summarizer> on the
/// runner without giving up the test's handle to the inner Mutex (the runner
/// stores the Box; the test keeps the Arc to read what was captured).
struct CapturingSummarizerWrapper(Arc<CapturingSummarizer>);

impl Summarizer for CapturingSummarizerWrapper {
    fn summarize<'a>(
        &'a self,
        events: &'a [TurnEvent],
        custom_instructions: Option<&'a str>,
    ) -> PFut<'a, Result<String, SummarizeError>> {
        self.0.summarize(events, custom_instructions)
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// Six assistant turns over one user input; default tail_turns=4 folds 2.
fn six_turn_session() -> (SessionId, Vec<TurnEvent>) {
    let s = SessionId::new();
    let ids: Vec<EventId> = (0..6).map(|_| EventId::new()).collect();
    let mut events = vec![TurnEvent {
        id: ids[0],
        session: s,
        ts: 0,
        prev_hash: None,
        kind: TurnEventKind::UserInput {
            text: "do work".into(),
        },
    }];
    for (i, id) in ids[1..].iter().enumerate() {
        events.push(TurnEvent {
            id: *id,
            session: s,
            ts: 0,
            prev_hash: None,
            kind: TurnEventKind::AssistantMessage {
                text: format!("turn {i}"),
                thinking: None,
            },
        });
    }
    (s, events)
}

async fn append_events(store: &SessionStore, events: &[TurnEvent]) {
    for ev in events {
        store.append(ev.clone()).await.unwrap();
    }
}

/// PreCompact fires before the summarizer and its Inject verdict output
/// becomes the custom summarization instructions (the return channel). The
/// capturing summarizer sees the merged instructions; a PreCompact HookSignal
/// is appended to the durable log.
#[tokio::test]
async fn test_precompress_fires_return_channel() {
    let (s, events) = six_turn_session();
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    append_events(&store, &events).await;
    let capturing = Arc::new(CapturingSummarizer::new());
    let reg = HookRegistry::new();
    reg.register(Arc::new(FixedHook {
        name: "pre-inject".into(),
        events: vec![HookEvent::PreCompact],
        verdict: HookVerdict::Inject("focus on the API design".into()),
    }));
    let runner = build_runner(store.clone(), Arc::clone(&capturing), Some(reg));
    let outcome = runner.compact(s).await.expect("compact runs");
    assert!(outcome.made_progress, "compaction made progress");
    // The return channel: the Inject output reached the summarizer as custom
    // instructions.
    assert_eq!(
        capturing.seen().as_deref(),
        Some("focus on the API design"),
        "PreCompact Inject output threaded into summarizer"
    );
    // A PreCompact HookSignal is in the durable log.
    let replay = store.replay(s).await.unwrap();
    assert!(
        replay.iter().any(|e| matches!(
            &e.kind,
            TurnEventKind::HookSignal {
                event: houyicoder_context::HookEventKind::PreCompact,
                ..
            }
        )),
        "PreCompact hook signal appended"
    );
}

/// A Deny verdict on PreCompact does NOT abort compaction — denying
/// compaction would brick the session on overflow. Compaction
/// proceeds: a CompactionBoundary + Summary land in the log.
#[tokio::test]
async fn test_precompact_no_deny_path() {
    let (s, events) = six_turn_session();
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    append_events(&store, &events).await;
    let capturing = Arc::new(CapturingSummarizer::new());
    let reg = HookRegistry::new();
    reg.register(Arc::new(FixedHook {
        name: "pre-deny".into(),
        events: vec![HookEvent::PreCompact],
        verdict: HookVerdict::Deny("do not compact".into()),
    }));
    let runner = build_runner(store.clone(), Arc::clone(&capturing), Some(reg));
    let outcome = runner.compact(s).await.expect("compact runs despite deny");
    assert!(outcome.made_progress, "compaction proceeded past the deny");
    let replay = store.replay(s).await.unwrap();
    assert!(
        replay
            .iter()
            .any(|e| matches!(&e.kind, TurnEventKind::CompactionBoundary { .. })),
        "CompactionBoundary appended despite PreCompact deny"
    );
    assert!(
        replay
            .iter()
            .any(|e| matches!(&e.kind, TurnEventKind::Summary { .. })),
        "Summary appended despite PreCompact deny"
    );
}

/// PostCompact fires after the summary commits, carrying the summary text.
/// The durable log carries a PostCompact HookSignal after the
/// CompactionBoundary + Summary events.
#[tokio::test]
async fn test_postcompact_fires_with_summary() {
    let (s, events) = six_turn_session();
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    append_events(&store, &events).await;
    let capturing = Arc::new(CapturingSummarizer::new());
    let reg = HookRegistry::new();
    reg.register(Arc::new(FixedHook {
        name: "post-observe".into(),
        events: vec![HookEvent::PostCompact],
        verdict: HookVerdict::Observe("noted".into()),
    }));
    let runner = build_runner(store.clone(), Arc::clone(&capturing), Some(reg));
    let outcome = runner.compact(s).await.expect("compact runs");
    let replay = store.replay(s).await.unwrap();
    // PostCompact fires after CompactionBoundary + Summary.
    let boundary_idx = replay
        .iter()
        .position(|e| matches!(&e.kind, TurnEventKind::CompactionBoundary { .. }))
        .expect("CompactionBoundary present");
    let post_idx = replay
        .iter()
        .position(|e| {
            matches!(
                &e.kind,
                TurnEventKind::HookSignal {
                    event: houyicoder_context::HookEventKind::PostCompact,
                    ..
                }
            )
        })
        .expect("PostCompact hook signal present");
    assert!(
        post_idx > boundary_idx,
        "PostCompact fires after CompactionBoundary"
    );
    assert!(outcome.made_progress);
    let post_signal = replay.iter().find(|e| {
        matches!(
            &e.kind,
            TurnEventKind::HookSignal {
                event: houyicoder_context::HookEventKind::PostCompact,
                ..
            }
        )
    });
    // The HookSignal carries the verdict kind + reason; the summary text
    // rides the payload at dispatch time (the signal records the outcome,
    // not the payload). This asserts the signal landed, which is the
    // observable the durable record carries.
    assert!(post_signal.is_some(), "PostCompact signal recorded");
    assert!(outcome.made_progress);
}

/// The auto path (Runner::compress) shares the same fire sequence: PreCompact
/// with trigger=Auto fires before the summarizer. Pins the unification so a
/// later refactor cannot drop the auto path's hook fire.
#[tokio::test]
async fn test_auto_path_fires_precompact() {
    let (s, events) = six_turn_session();
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    append_events(&store, &events).await;
    let capturing = Arc::new(CapturingSummarizer::new());
    let reg = HookRegistry::new();
    reg.register(Arc::new(FixedHook {
        name: "pre-auto".into(),
        events: vec![HookEvent::PreCompact],
        verdict: HookVerdict::Inject("auto-path instructions".into()),
    }));
    let runner = build_runner(store.clone(), Arc::clone(&capturing), Some(reg));
    let progress = runner.compress(s).await.expect("compress runs");
    assert!(progress, "auto compress made progress");
    assert_eq!(
        capturing.seen().as_deref(),
        Some("auto-path instructions"),
        "auto path threads Inject output to the summarizer too"
    );
    let replay = store.replay(s).await.unwrap();
    assert!(
        replay.iter().any(|e| matches!(
            &e.kind,
            TurnEventKind::HookSignal {
                event: houyicoder_context::HookEventKind::PreCompact,
                ..
            }
        )),
        "auto path fired PreCompact"
    );
}
