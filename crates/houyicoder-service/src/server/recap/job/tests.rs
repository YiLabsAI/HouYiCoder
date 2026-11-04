//! Tests for this module.

use super::*;
use std::future::pending;
use std::sync::Arc;
use std::sync::Mutex;

use houyicoder_api::provider::ModelProvider;
use houyicoder_async::{PFut, PStream};
use houyicoder_context::RecapSource;
use houyicoder_memory::InMemoryBackend;
use houyicoder_protocol::llm::CompletionResponse;
use houyicoder_protocol::llm::LlmEvent;
use houyicoder_protocol::llm::ModelCapabilities;
use houyicoder_protocol::llm::ProviderError;
use houyicoder_protocol::llm::UsageProvenance;
use houyicoder_session::SessionStore;

use crate::server::recap::AWAY_AFTER;
use crate::server::recap::RecapSettings;

/// A provider that answers with one canned text and counts how often it was
/// asked. It can also append a user event while the request is out, standing
/// for the user coming back and typing before the answer lands.
struct ScriptProvider {
    reply: String,
    calls: Mutex<u32>,
    append_on_call: Option<(Arc<SessionStore>, SessionId)>,
}

impl ScriptProvider {
    fn new(reply: &str) -> Self {
        Self {
            reply: reply.to_string(),
            calls: Mutex::new(0),
            append_on_call: None,
        }
    }

    fn calls(&self) -> u32 {
        *self.calls.lock().expect("call count lock")
    }
}

impl ModelProvider for ScriptProvider {
    fn complete(
        &self,
        _req: CompletionRequest,
    ) -> houyicoder_async::PFut<'_, Result<CompletionResponse, ProviderError>> {
        Box::pin(async move {
            *self.calls.lock().expect("call count lock") += 1;
            if let Some((store, session)) = &self.append_on_call {
                store
                    .append(new_entry(
                        *session,
                        SessionEvent::UserInput {
                            text: "never mind, do this instead".into(),
                        },
                    ))
                    .await
                    .expect("append during the request");
            }
            Ok(CompletionResponse {
                output: vec![OutputItem::Text {
                    text: self.reply.clone(),
                }],
                usage: Usage::default(),
                model: "small".into(),
            })
        })
    }

    fn stream(&self, _req: CompletionRequest) -> PStream<'_, Result<LlmEvent, ProviderError>> {
        panic!("a recap never streams")
    }

    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
}

/// A provider that never answers, standing for an endpoint that stops
/// responding after the request is out.
struct SilentProvider;

impl ModelProvider for SilentProvider {
    fn complete(
        &self,
        _req: CompletionRequest,
    ) -> PFut<'_, Result<CompletionResponse, ProviderError>> {
        Box::pin(pending())
    }

    fn stream(&self, _req: CompletionRequest) -> PStream<'_, Result<LlmEvent, ProviderError>> {
        panic!("a recap never streams")
    }

    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
}

/// A provider that cancels the caller's token as the request reaches it and
/// never answers: the terminal reported focus while the request was out.
struct CancellingProvider {
    cancel: CancellationToken,
}

impl ModelProvider for CancellingProvider {
    fn complete(
        &self,
        _req: CompletionRequest,
    ) -> PFut<'_, Result<CompletionResponse, ProviderError>> {
        self.cancel.cancel();
        Box::pin(pending())
    }

    fn stream(&self, _req: CompletionRequest) -> PStream<'_, Result<LlmEvent, ProviderError>> {
        panic!("a recap never streams")
    }

    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
}

fn settings() -> RecapSettings {
    RecapSettings {
        enabled: true,
        model: Some("small".into()),
        ..Default::default()
    }
}

async fn seeded() -> (Arc<SessionStore>, SessionId, EventId) {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let session = SessionId::new();
    let user = store
        .append(new_entry(
            session,
            SessionEvent::UserInput {
                text: "fix the build".into(),
            },
        ))
        .await
        .expect("seed the user event");
    store
        .append(new_entry(
            session,
            SessionEvent::AssistantMessage {
                text: "reading the failing test".into(),
                thinking: None,
            },
        ))
        .await
        .expect("seed the reply");
    (store, session, user)
}

#[tokio::test]
async fn test_run_records_recap() {
    let (store, session, user) = seeded().await;
    let provider: Arc<dyn ModelProvider> = Arc::new(ScriptProvider::new(
        "Fixing the build. Next: rerun the test.",
    ));
    let log: Arc<dyn SessionLog> = store.clone();
    let recap = RecapSession::new(session, settings(), provider, log);

    let outcome = run(&recap, Entry::Away, CancellationToken::new()).await;
    assert_eq!(outcome, Outcome::Written);

    let events = store.replay(session).await.expect("replay");
    assert_eq!(events.len(), 4, "the attempt and the recap follow the work");
    assert!(
        matches!(events[2].event, SessionEvent::RecapAttempt { .. }),
        "the attempt is durable before the request is billed"
    );
    let SessionEvent::AwayRecap {
        text,
        last_user_event,
        source,
        usage,
        ..
    } = &events[3].event
    else {
        panic!("the last event is the recap");
    };
    assert_eq!(text, "Fixing the build. Next: rerun the test.");
    assert_eq!(*last_user_event, user);
    assert_eq!(*source, RecapSource::Away);
    let usage = usage.as_ref().expect("the host recorded what it knew");
    assert_eq!(
        usage.input_tokens, None,
        "an endpoint that reported no input count leaves it unknown"
    );
    assert_eq!(
        usage.output_tokens, None,
        "a response with no usage account reports no output count, so a zero is not a measurement"
    );
    assert_eq!(
        usage.duration_ms,
        Some(usage.duration_ms.expect("the host timed the request")),
        "the duration is the host's own measurement"
    );
}

#[tokio::test]
async fn test_run_skips_attempted() {
    let (store, session, _) = seeded().await;
    let provider = Arc::new(ScriptProvider::new("Fixing the build."));
    let typed: Arc<dyn ModelProvider> = provider.clone();
    let log: Arc<dyn SessionLog> = store.clone();
    let recap = RecapSession::new(session, settings(), typed, log);

    assert_eq!(
        run(&recap, Entry::Away, CancellationToken::new()).await,
        Outcome::Written
    );
    let after_first = store.replay(session).await.expect("replay").len();
    assert_eq!(
        run(&recap, Entry::Resume, CancellationToken::new()).await,
        Outcome::Duplicate
    );
    assert_eq!(provider.calls(), 1, "the same point is never billed twice");
    assert_eq!(
        store.replay(session).await.expect("replay").len(),
        after_first
    );
}

/// The user came back and typed while the answer was on its way. The answer
/// describes work that request replaced, so it is dropped rather than shown.
#[tokio::test]
async fn test_run_drops_stale_answer() {
    let (store, session, _) = seeded().await;
    let provider = Arc::new(ScriptProvider {
        append_on_call: Some((store.clone(), session)),
        ..ScriptProvider::new("Fixing the build.")
    });
    let typed: Arc<dyn ModelProvider> = provider.clone();
    let log: Arc<dyn SessionLog> = store.clone();
    let recap = RecapSession::new(session, settings(), typed, log);

    let outcome = run(&recap, Entry::Away, CancellationToken::new()).await;
    assert!(
        matches!(outcome, Outcome::Refused(ref reason) if reason.contains("moved on")),
        "{outcome:?}"
    );
    assert_eq!(provider.calls(), 1);
    assert!(
        !store
            .replay(session)
            .await
            .expect("replay")
            .iter()
            .any(|entry| matches!(entry.event, SessionEvent::AwayRecap { .. })),
        "no recap is written for work the user replaced"
    );
}

#[tokio::test]
async fn test_run_refuses_empty_answer() {
    let (store, session, _) = seeded().await;
    let provider: Arc<dyn ModelProvider> = Arc::new(ScriptProvider::new("   "));
    let log: Arc<dyn SessionLog> = store.clone();
    let recap = RecapSession::new(session, settings(), provider, log);

    let outcome = run(&recap, Entry::Away, CancellationToken::new()).await;
    assert!(matches!(outcome, Outcome::Refused(_)), "{outcome:?}");
    assert!(
        !store
            .replay(session)
            .await
            .expect("replay")
            .iter()
            .any(|entry| matches!(entry.event, SessionEvent::AwayRecap { .. })),
        "an answer with no text leaves no row"
    );
}

/// A log whose last activity is older than the away window, so the resume
/// entry has work to summarize.
async fn seeded_aged(age_ms: u64) -> (Arc<SessionStore>, SessionId) {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let session = SessionId::new();
    let ts = now_millis().saturating_sub(age_ms);
    let mut user = new_entry(
        session,
        SessionEvent::UserInput {
            text: "fix the build".into(),
        },
    );
    user.ts = ts;
    store.append(user).await.expect("seed the user event");
    let mut reply = new_entry(
        session,
        SessionEvent::AssistantMessage {
            text: "reading the failing test".into(),
            thinking: None,
        },
    );
    reply.ts = ts;
    store.append(reply).await.expect("seed the reply");
    (store, session)
}

/// The endpoint never answers. The attempt gives up at the deadline rather
/// than holding the job open; the caller sees a failure with no row.
#[tokio::test(start_paused = true)]
async fn test_run_stops_at_deadline() {
    let (store, session, _) = seeded().await;
    let log: Arc<dyn SessionLog> = store.clone();
    let recap = RecapSession::new(session, settings(), Arc::new(SilentProvider), log);

    let outcome = run(&recap, Entry::Away, CancellationToken::new()).await;
    assert!(
        matches!(
            outcome,
            Outcome::Failed(ref reason) if reason.contains("did not answer")
        ),
        "{outcome:?}"
    );
    assert!(
        !store
            .replay(session)
            .await
            .expect("replay")
            .iter()
            .any(|entry| matches!(entry.event, SessionEvent::AwayRecap { .. })),
        "a request that timed out leaves no row"
    );
}

#[tokio::test]
async fn test_run_refuses_oversized_answer() {
    let (store, session, _) = seeded().await;
    let reply = "x".repeat(OUTPUT_BYTES + 1);
    let provider: Arc<dyn ModelProvider> = Arc::new(ScriptProvider::new(&reply));
    let log: Arc<dyn SessionLog> = store.clone();
    let recap = RecapSession::new(session, settings(), provider, log);

    let outcome = run(&recap, Entry::Away, CancellationToken::new()).await;
    assert!(
        matches!(outcome, Outcome::Refused(ref reason) if reason.contains("over the")),
        "{outcome:?}"
    );
    assert!(
        !store
            .replay(session)
            .await
            .expect("replay")
            .iter()
            .any(|entry| matches!(entry.event, SessionEvent::AwayRecap { .. })),
        "an answer over the ceiling is refused rather than cut"
    );
}

/// The terminal reported focus before the request went out, so the token is
/// already cancelled. The attempt stands, since it was recorded first; no
/// request is billed and no row appears.
#[tokio::test]
async fn test_run_observes_cancellation() {
    let (store, session, _) = seeded().await;
    let log: Arc<dyn SessionLog> = store.clone();
    let recap = RecapSession::new(session, settings(), Arc::new(SilentProvider), log);
    let cancel = CancellationToken::new();
    cancel.cancel();

    assert_eq!(run(&recap, Entry::Away, cancel).await, Outcome::Cancelled);
    let events = store.replay(session).await.expect("replay");
    assert!(
        matches!(
            events.last().map(|entry| &entry.event),
            Some(SessionEvent::RecapAttempt { .. })
        ),
        "the attempt is durable before the request, so a cancelled one still shows"
    );
    assert!(
        !events
            .iter()
            .any(|entry| matches!(entry.event, SessionEvent::AwayRecap { .. }))
    );
}

/// The terminal reports focus while the request is out. The cancellation
/// reaches the waiting call rather than being seen only before it, and the
/// attempt stands since it was recorded first: nothing is billed twice and no
/// row appears.
#[tokio::test]
async fn test_run_cancels_running_request() {
    let (store, session, _) = seeded().await;
    let log: Arc<dyn SessionLog> = store.clone();
    let cancel = CancellationToken::new();
    let provider: Arc<dyn ModelProvider> = Arc::new(CancellingProvider {
        cancel: cancel.clone(),
    });
    let recap = RecapSession::new(session, settings(), provider, log);

    assert_eq!(run(&recap, Entry::Away, cancel).await, Outcome::Cancelled);
    let events = store.replay(session).await.expect("replay");
    assert!(
        matches!(
            events.last().map(|entry| &entry.event),
            Some(SessionEvent::RecapAttempt { .. })
        ),
        "the attempt is durable before the request, so a cancelled one still shows"
    );
    assert!(
        !events
            .iter()
            .any(|entry| matches!(entry.event, SessionEvent::AwayRecap { .. })),
        "a request cancelled mid-flight leaves no row"
    );
}

#[tokio::test]
async fn test_resume_needs_stale_work() {
    let (store, session, _) = seeded().await;
    let provider = Arc::new(ScriptProvider::new("Fixing the build."));
    let typed: Arc<dyn ModelProvider> = provider.clone();
    let log: Arc<dyn SessionLog> = store.clone();
    let recap = RecapSession::new(session, settings(), typed, log);

    assert_eq!(
        run(&recap, Entry::Resume, CancellationToken::new()).await,
        Outcome::NoWindow,
        "a session worked in moments ago has nothing to recap"
    );
    assert_eq!(provider.calls(), 0, "no request is sent for fresh work");
}

#[tokio::test]
async fn test_resume_covers_stale_work() {
    let away = u64::try_from(AWAY_AFTER.as_millis()).expect("the window fits");
    let (store, session) = seeded_aged(away + 1).await;
    let provider: Arc<dyn ModelProvider> = Arc::new(ScriptProvider::new("Fixing the build."));
    let log: Arc<dyn SessionLog> = store.clone();
    let recap = RecapSession::new(session, settings(), provider, log);

    assert_eq!(
        run(&recap, Entry::Resume, CancellationToken::new()).await,
        Outcome::Written
    );
}

/// A response with no usage account is not a zero-cost call. Every token
/// figure stays absent; the host's own duration stays.
#[test]
fn test_unreported_usage_stays_unknown() {
    let usage = usage_of(&Usage::default(), 12);
    assert_eq!(usage.input_tokens, None);
    assert_eq!(usage.output_tokens, None);
    assert_eq!(usage.cache_read_input_tokens, None);
    assert_eq!(usage.cache_write_input_tokens, None);
    assert_eq!(usage.duration_ms, Some(12));
}

/// A reported zero is a measurement, not a missing figure.
#[test]
fn test_accounted_usage_is_recorded() {
    let usage = usage_of(
        &Usage {
            input_tokens: 900,
            output_tokens: 40,
            total_tokens: 940,
            non_cached_input_tokens: 100,
            cache_read_input_tokens: 800,
            cache_write_input_tokens: 0,
            reasoning_tokens: 0,
            provenance: UsageProvenance::ALL_REPORTED,
        },
        250,
    );
    assert_eq!(usage.input_tokens, Some(900));
    assert_eq!(usage.output_tokens, Some(40));
    assert_eq!(usage.cache_read_input_tokens, Some(800));
    assert_eq!(
        usage.cache_write_input_tokens,
        Some(0),
        "a reported zero is a measurement"
    );
    assert_eq!(usage.duration_ms, Some(250));
}

#[test]
fn test_missing_point_is_replaced() {
    assert!(
        superseded(&[], EventId::new()),
        "an answer for a log this host cannot see anymore is dropped"
    );
}

#[test]
fn test_dedup_key_ignores_recaps() {
    let session = SessionId::new();
    let user = new_entry(session, SessionEvent::UserInput { text: "go".into() });
    let reply = new_entry(
        session,
        SessionEvent::AssistantMessage {
            text: "done".into(),
            thinking: None,
        },
    );
    let recap = new_entry(
        session,
        SessionEvent::AwayRecap {
            text: "was building".into(),
            last_user_event: user.id,
            as_of_event: reply.id,
            source: RecapSource::Away,
            model: "small".into(),
            usage: None,
        },
    );
    let key = dedup_key(&[user.clone(), reply.clone(), recap]).expect("a point to summarize");
    assert_eq!(key.last_user, user.id);
    assert_eq!(
        key.as_of, reply.id,
        "a recap is not part of the work it describes"
    );
    assert!(!attempted(&[user, reply], key), "nothing tried it yet");
}

#[test]
fn test_dedup_key_needs_request() {
    let session = SessionId::new();
    let reply = new_entry(
        session,
        SessionEvent::AssistantMessage {
            text: "done".into(),
            thinking: None,
        },
    );
    assert!(
        dedup_key(&[reply]).is_none(),
        "a session with no user request has nothing to summarize"
    );
}

/// A recap is prose the user reads at a glance. Code formatting is the habit a
/// summary of a coding session falls into, so the marks are dropped whatever
/// the answer asked for.
#[test]
fn test_recap_drops_marks() {
    let output = vec![OutputItem::Text {
        text: "Goal was fixing `server.rs:679`. Next: rerun `make check`.".into(),
    }];
    assert_eq!(
        answer_text(&output).expect("usable text"),
        "Goal was fixing server.rs:679. Next: rerun make check.",
        "the marks are gone, the words stay"
    );
}

/// The instruction has to name what it forbids: an instruction that only asks
/// for a high-level task leaves the register of the conversation in place.
#[test]
fn test_instruction_names_limits() {
    for phrase in ["no file paths", "no line numbers", "no code formatting"] {
        assert!(
            INSTRUCTION.contains(phrase),
            "the instruction names the prohibition: {phrase}"
        );
    }
}
