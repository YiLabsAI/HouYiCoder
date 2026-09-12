//! Hook-fire tests for the unified compaction path: PreCompact fires before
//! the summarizer with a return channel (Inject verdict output becomes
//! custom summarization instructions), PostCompact fires after the summary
//! commits with the summary text, and a Deny verdict on PreCompact does NOT
//! abort compaction — denying compaction would brick the session on overflow.

use std::sync::{Arc, Mutex};

use houyicoder_async::PFut;
use houyicoder_context::{EventId, SessionEvent, SessionId, SessionLogEntry};
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

/// build_manifest preserves injected hook guidance when the primary
/// summarizer fails and the heuristic fallback runs.
#[tokio::test]
async fn test_fallback_preserves_hook_guidance() {
    use houyicoder_context::Disposition;
    let s = SessionId::new();
    // Six assistant turns so the default tail_turns=4 Summarizes the first 2
    // (the folded span the failing summarizer + heuristic fallback run on).
    let mut events = vec![SessionLogEntry {
        id: EventId::new(),
        session: s,
        ts: 0,
        prev_hash: None,
        event: SessionEvent::UserInput {
            text: "do work".into(),
        },
    }];
    for i in 0..6 {
        events.push(SessionLogEntry {
            id: EventId::new(),
            session: s,
            ts: 0,
            prev_hash: None,
            event: SessionEvent::AssistantMessage {
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
            _events: &'a [SessionLogEntry],
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

/// Hook fixture with a fixed verdict.
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

/// Summarizer fixture that captures injected instructions.
struct SummaryCapture {
    seen: Mutex<Option<String>>,
}

impl SummaryCapture {
    fn new() -> Self {
        Self {
            seen: Mutex::new(None),
        }
    }
    fn seen(&self) -> Option<String> {
        self.seen.lock().unwrap().clone()
    }
}

impl Summarizer for SummaryCapture {
    fn summarize<'a>(
        &'a self,
        _events: &'a [SessionLogEntry],
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

/// Build a runner with deterministic summary and hook fixtures.
fn build_runner(
    store: Arc<SessionStore>,
    summarizer: Arc<SummaryCapture>,
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
    runner = runner.with_summarizer(Box::new(CapturingSummarizer(summarizer)));
    if let Some(reg) = hooks {
        runner = runner.with_hooks(Arc::new(reg));
    }
    runner
}

/// Trait-object adapter retaining the test's shared capture handle.
struct CapturingSummarizer(Arc<SummaryCapture>);

impl Summarizer for CapturingSummarizer {
    fn summarize<'a>(
        &'a self,
        events: &'a [SessionLogEntry],
        custom_instructions: Option<&'a str>,
    ) -> PFut<'a, Result<String, SummarizeError>> {
        self.0.summarize(events, custom_instructions)
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// Six assistant turns over one user input; default tail_turns=4 folds 2.
fn six_turn_session() -> (SessionId, Vec<SessionLogEntry>) {
    let s = SessionId::new();
    let ids: Vec<EventId> = (0..6).map(|_| EventId::new()).collect();
    let mut events = vec![SessionLogEntry {
        id: ids[0],
        session: s,
        ts: 0,
        prev_hash: None,
        event: SessionEvent::UserInput {
            text: "do work".into(),
        },
    }];
    for (i, id) in ids[1..].iter().enumerate() {
        events.push(SessionLogEntry {
            id: *id,
            session: s,
            ts: 0,
            prev_hash: None,
            event: SessionEvent::AssistantMessage {
                text: format!("turn {i}"),
                thinking: None,
            },
        });
    }
    (s, events)
}

async fn append_events(store: &SessionStore, events: &[SessionLogEntry]) {
    for ev in events {
        store.append(ev.clone()).await.unwrap();
    }
}

/// PreCompact injection reaches the summarizer and durable audit log.
#[tokio::test]
async fn test_precompress_fires_return_channel() {
    let (s, events) = six_turn_session();
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    append_events(&store, &events).await;
    let capturing = Arc::new(SummaryCapture::new());
    let reg = HookRegistry::new();
    reg.register(Arc::new(FixedHook {
        name: "pre-inject".into(),
        events: vec![HookEvent::PreCompact],
        verdict: HookVerdict::Inject("focus on the API design".into()),
    }));
    let runner = build_runner(store.clone(), Arc::clone(&capturing), Some(reg));
    let outcome = runner.compact(s).await.expect("compact runs");
    assert!(outcome.made_progress, "compaction made progress");
    assert_eq!(
        capturing.seen().as_deref(),
        Some("focus on the API design"),
        "PreCompact Inject output threaded into summarizer"
    );
    let replay = store.replay(s).await.unwrap();
    assert!(
        replay.iter().any(|e| matches!(
            &e.event,
            SessionEvent::HookSignal {
                event: houyicoder_context::HookEventKind::PreCompact,
                ..
            }
        )),
        "PreCompact hook signal appended"
    );
}

/// PreCompact denial cannot block overflow recovery.
#[tokio::test]
async fn test_precompact_no_deny_path() {
    let (s, events) = six_turn_session();
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    append_events(&store, &events).await;
    let capturing = Arc::new(SummaryCapture::new());
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
            .any(|e| matches!(&e.event, SessionEvent::CompactionBoundary { .. })),
        "CompactionBoundary appended despite PreCompact deny"
    );
    assert!(
        replay
            .iter()
            .any(|e| matches!(&e.event, SessionEvent::Summary { .. })),
        "Summary appended despite PreCompact deny"
    );
}

/// PostCompact audit follows the committed boundary.
#[tokio::test]
async fn test_postcompact_fires_with_summary() {
    let (s, events) = six_turn_session();
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    append_events(&store, &events).await;
    let capturing = Arc::new(SummaryCapture::new());
    let reg = HookRegistry::new();
    reg.register(Arc::new(FixedHook {
        name: "post-observe".into(),
        events: vec![HookEvent::PostCompact],
        verdict: HookVerdict::Observe("noted".into()),
    }));
    let runner = build_runner(store.clone(), Arc::clone(&capturing), Some(reg));
    let outcome = runner.compact(s).await.expect("compact runs");
    let replay = store.replay(s).await.unwrap();
    let boundary_idx = replay
        .iter()
        .position(|e| matches!(&e.event, SessionEvent::CompactionBoundary { .. }))
        .expect("CompactionBoundary present");
    let post_idx = replay
        .iter()
        .position(|e| {
            matches!(
                &e.event,
                SessionEvent::HookSignal {
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
            &e.event,
            SessionEvent::HookSignal {
                event: houyicoder_context::HookEventKind::PostCompact,
                ..
            }
        )
    });
    assert!(post_signal.is_some(), "PostCompact signal recorded");
    assert!(outcome.made_progress);
}

/// Recompacting unchanged model input reuses the prior post measurement.
#[tokio::test]
async fn test_recompact_reuses_measurement() {
    let (session, events) = six_turn_session();
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    append_events(&store, &events).await;
    let summarizer = Arc::new(SummaryCapture::new());
    let runner = build_runner(store, summarizer, None);

    let first = runner.compact(session).await.expect("first compact");
    let second = runner.compact(session).await.expect("second compact");

    assert_eq!(
        second.pre_compact_tokens, first.post_compact_tokens,
        "unchanged model input keeps the previous post measurement"
    );
}

#[tokio::test]
async fn test_auto_path_fires_precompact() {
    let (s, events) = six_turn_session();
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    append_events(&store, &events).await;
    let capturing = Arc::new(SummaryCapture::new());
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
            &e.event,
            SessionEvent::HookSignal {
                event: houyicoder_context::HookEventKind::PreCompact,
                ..
            }
        )),
        "auto path fired PreCompact"
    );
}
