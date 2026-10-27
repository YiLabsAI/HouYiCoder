//! Tests for this module.

use super::*;

use houyicoder_api::provider::ModelProvider;
use houyicoder_async::{PFut, PStream};
use houyicoder_memory::InMemoryBackend;
use houyicoder_protocol::llm::{
    CompletionRequest, CompletionResponse, LlmEvent, ModelCapabilities, ProviderError,
};
use houyicoder_session::SessionStore;

struct UnusedProvider;

impl ModelProvider for UnusedProvider {
    fn complete(
        &self,
        _req: CompletionRequest,
    ) -> PFut<'_, Result<CompletionResponse, ProviderError>> {
        Box::pin(async { panic!("the state machine never calls the provider") })
    }

    fn stream(&self, _req: CompletionRequest) -> PStream<'_, Result<LlmEvent, ProviderError>> {
        panic!("the state machine never streams")
    }

    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
}

fn enabled() -> RecapSettings {
    RecapSettings {
        enabled: true,
        model: Some("small".into()),
    }
}

fn session(settings: RecapSettings) -> RecapSession {
    let store = SessionStore::new(Box::new(InMemoryBackend::new()));
    RecapSession::new(
        SessionId::new(),
        settings,
        Arc::new(UnusedProvider),
        Arc::new(store),
    )
}

#[test]
fn test_away_starts_only_unfocused() {
    let recap = session(enabled());
    let now = Instant::now();
    recap.attach(7);
    assert_eq!(
        recap.next_due(now),
        None,
        "an unknown client blocks the timer"
    );

    recap.report_focus(7, false, now);
    let deadline = recap.next_due(now).expect("the away clock is armed");
    assert_eq!(deadline, now + AWAY_AFTER);

    assert!(
        recap.take_due(now).is_none(),
        "nothing fires before the deadline"
    );
    let job = recap
        .take_due(now + AWAY_AFTER)
        .expect("the away entry fires at the deadline");
    assert_eq!(job.entry, Entry::Away);
}

#[test]
fn test_unknown_client_blocks_away() {
    let recap = session(enabled());
    recap.attach(1);
    recap.attach(2);
    recap.report_focus(1, false, Instant::now());
    assert!(
        recap.next_due(Instant::now()).is_none(),
        "a client that never reported keeps the session out of the away entry"
    );
}

#[test]
fn test_focus_cancels_away_job() {
    let recap = session(enabled());
    let now = Instant::now();
    recap.attach(1);
    recap.report_focus(1, false, now);
    let job = recap.take_due(now + AWAY_AFTER).expect("away fires");
    assert!(recap.is_running(Entry::Away));

    recap.report_focus(1, true, now + AWAY_AFTER);
    assert!(job.cancel.is_cancelled(), "coming back stops the request");
    assert!(!recap.is_running(Entry::Away));
}

#[test]
fn test_resume_survives_focus() {
    let recap = session(enabled());
    recap.attach(1);
    recap.report_focus(1, true, Instant::now());
    recap.resume_entry();

    let job = recap
        .take_due(Instant::now())
        .expect("the resume entry runs while focused");
    assert_eq!(job.entry, Entry::Resume);
    recap.report_focus(1, true, Instant::now());
    assert!(
        !job.cancel.is_cancelled(),
        "opening a session is what gaining focus does"
    );
}

#[test]
fn test_turn_start_cancels_resume() {
    let recap = session(enabled());
    recap.attach(1);
    recap.report_focus(1, true, Instant::now());
    recap.resume_entry();
    recap.turn_started();
    assert!(
        recap.take_due(Instant::now()).is_none(),
        "a new turn makes the idle entry stale"
    );
}

#[test]
fn test_disabled_stays_inert() {
    let recap = session(RecapSettings {
        enabled: false,
        model: Some("small".into()),
    });
    recap.attach(1);
    recap.report_focus(1, false, Instant::now());
    assert_eq!(recap.next_due(Instant::now()), None);
    assert!(recap.take_due(Instant::now() + AWAY_AFTER).is_none());
}

#[test]
fn test_last_detach_cancels() {
    let recap = session(enabled());
    recap.attach(1);
    recap.report_focus(1, false, Instant::now());
    let job = recap
        .take_due(Instant::now() + AWAY_AFTER)
        .expect("away fires");
    recap.detach(1, Instant::now());
    assert!(
        job.cancel.is_cancelled(),
        "a closing session is not an absent user"
    );
}

#[test]
fn test_turn_finish_starts_clock() {
    let recap = session(enabled());
    let now = Instant::now();
    recap.attach(1);
    recap.report_focus(1, false, now);
    recap.turn_started();
    assert_eq!(
        recap.next_due(now),
        None,
        "the clock does not run during a turn"
    );
    recap.turn_finished(now);
    let deadline = recap.next_due(now).expect("the clock starts at turn end");
    assert!(deadline >= now + AWAY_AFTER);
}

#[test]
fn test_second_client_stays_away() {
    let recap = session(enabled());
    recap.attach(1);
    recap.attach(2);
    recap.report_focus(1, false, Instant::now());
    recap.report_focus(2, false, Instant::now());
    assert!(recap.next_due(Instant::now()).is_some(), "both left");

    recap.report_focus(2, true, Instant::now());
    assert!(
        recap.next_due(Instant::now()).is_none(),
        "one attached client still looking keeps the session attended"
    );
}
