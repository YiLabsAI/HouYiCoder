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
        ..Default::default()
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
        ..Default::default()
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

/// A session assembled with the feature off still tracks its clients, so a
/// switch turned on later arms the clock from the focus they already
/// reported instead of waiting for a new report.
#[test]
fn test_reconfigure_arms_away() {
    let recap = session(RecapSettings::default());
    let now = Instant::now();
    recap.attach(1);
    recap.report_focus(1, false, now);
    assert_eq!(recap.next_due(now), None, "the switch is off");

    recap.reconfigure(
        RecapSettings {
            enabled: true,
            model: Some("small".into()),
            idle_trigger_secs: 180,
        },
        now,
    );
    let deadline = recap.next_due(now).expect("the switch arms the clock");
    assert_eq!(deadline, now + Duration::from_secs(180));
    let job = recap
        .take_due(now + Duration::from_secs(180))
        .expect("the away entry fires on the configured window");
    assert_eq!(job.entry, Entry::Away);
}

/// Turning the switch off stops the clock and cancels a request already on
/// its way, so a change made in the Config tab takes effect at once.
#[test]
fn test_reconfigure_cancels_job() {
    let recap = session(enabled());
    let now = Instant::now();
    recap.attach(1);
    recap.report_focus(1, false, now);
    let job = recap.take_due(now + AWAY_AFTER).expect("away fires");

    recap.reconfigure(RecapSettings::default(), now);
    assert!(job.cancel.is_cancelled(), "the switch stops the request");
    assert_eq!(recap.next_due(now), None);
}

/// A window change mid-absence shifts the deadline against when the user
/// actually left: shortening from 5m to 3m after 4m of absence fires at once,
/// and lengthening to 10m after 4m waits only the remaining 6m.
#[test]
fn test_window_change_restarts() {
    let recap = session(enabled());
    let start = Instant::now();
    recap.attach(1);
    recap.report_focus(1, false, start);
    assert_eq!(recap.next_due(start), Some(start + AWAY_AFTER));

    let four_min = start + Duration::from_secs(240);
    recap.reconfigure(
        RecapSettings {
            enabled: true,
            model: Some("small".into()),
            idle_trigger_secs: 600,
        },
        four_min,
    );
    assert_eq!(
        recap.next_due(four_min),
        Some(start + Duration::from_secs(600)),
        "lengthening to 10m after 4m leaves 6m remaining from the original start"
    );
    assert!(
        recap.take_due(start + AWAY_AFTER).is_none(),
        "the old 5m deadline no longer fires"
    );

    recap.reconfigure(
        RecapSettings {
            enabled: true,
            model: Some("small".into()),
            idle_trigger_secs: 180,
        },
        four_min,
    );
    assert_eq!(
        recap.next_due(four_min),
        Some(start + Duration::from_secs(180)),
        "shortening to 3m after 4m is already past due"
    );
    let job = recap
        .take_due(four_min)
        .expect("4m elapsed exceeds the 3m window, so the away entry fires immediately");
    assert_eq!(job.entry, Entry::Away);
    recap.finish(Entry::Away);

    recap.reconfigure(
        RecapSettings {
            enabled: true,
            model: Some("small".into()),
            idle_trigger_secs: 300,
        },
        four_min,
    );
    assert_eq!(
        recap.next_due(four_min),
        None,
        "an absence already summarized does not re-arm until focus returns or a turn runs"
    );
}

/// A window the loader refused is never a zero-length wait.
#[test]
fn test_window_never_zero() {
    let settings = RecapSettings {
        enabled: true,
        model: Some("small".into()),
        idle_trigger_secs: 0,
    };
    assert_eq!(settings.away_after(), Duration::from_secs(1));
}

/// The live settings are readable, so the status snapshot reports what the
/// trigger uses rather than what the file said at startup.
#[test]
fn test_settings_read_live() {
    let recap = session(enabled());
    assert!(recap.settings().usable());
    recap.reconfigure(RecapSettings::default(), Instant::now());
    assert!(!recap.settings().usable());
}
