//! Automatic compaction retry-suppression tests.

use std::sync::Arc;

use houyicoder_context::SessionId;
use houyicoder_memory::InMemoryBackend;
use houyicoder_session::SessionStore;

use super::{CompactionSuppression, SuppressionCause};
use crate::agent::runner_config::RunnerConfig;
use crate::agent::{Runner, ToolRegistry};

fn runner() -> Runner {
    Runner::new(
        std::sync::Arc::new(SessionStore::new(Box::new(InMemoryBackend::new()))),
        Arc::new(crate::provider::test_support::FakeProvider::text("ok")),
        ToolRegistry::new(),
        RunnerConfig {
            max_turns: 5,
            ..RunnerConfig::default()
        },
    )
}

#[test]
fn test_cause_maps_to_scope() {
    // Transient failures skip one turn; terminal causes remain suppressed
    // until the context budget changes.
    assert_eq!(
        SuppressionCause::Transient.suppression_state(),
        CompactionSuppression::Turn
    );
    assert_eq!(
        SuppressionCause::CorruptLog.suppression_state(),
        CompactionSuppression::Sticky
    );
    assert_eq!(
        SuppressionCause::NoProgress.suppression_state(),
        CompactionSuppression::Sticky
    );
}

#[test]
fn test_suppression_round_trips_u8() {
    for level in [
        CompactionSuppression::None,
        CompactionSuppression::Turn,
        CompactionSuppression::Sticky,
    ] {
        assert_eq!(CompactionSuppression::from_u8(level.as_u8()), level);
    }
    // Unknown values cannot disable automatic compaction.
    assert_eq!(
        CompactionSuppression::from_u8(99),
        CompactionSuppression::None
    );
    assert_eq!(
        CompactionSuppression::from_u8(3),
        CompactionSuppression::None
    );
    assert_eq!(
        CompactionSuppression::from_u8(4),
        CompactionSuppression::None
    );
}

#[test]
fn test_turn_clears_at_start() {
    assert!(CompactionSuppression::Turn.clears_at_turn_start());
    assert!(!CompactionSuppression::Sticky.clears_at_turn_start());
    assert!(!CompactionSuppression::None.clears_at_turn_start());
}

#[test]
fn test_runner_set_get_clear() {
    let r = runner();
    assert_eq!(r.compaction_suppression(), CompactionSuppression::None);

    r.set_compaction_suppression(CompactionSuppression::Sticky);
    assert_eq!(r.compaction_suppression(), CompactionSuppression::Sticky);

    // clear_sticky leaves a Turn-level suppress (the turn-start self-heal
    // owns that) but clears sticky/credit/auth.
    r.set_compaction_suppression(CompactionSuppression::Turn);
    r.clear_sticky_compaction_suppression();
    assert_eq!(r.compaction_suppression(), CompactionSuppression::Turn);
}

#[test]
fn test_model_switch_clears_sticky() {
    // A model switch may resolve a larger window, so a sticky suppress set
    // under the old (smaller) window lifts. Turn-level is left to the
    // turn-start self-heal.
    let r = runner();
    r.set_compaction_suppression(CompactionSuppression::Sticky);
    r.set_model("glm-4.6[1m]".into());
    assert_eq!(r.compaction_suppression(), CompactionSuppression::None);

    r.set_compaction_suppression(CompactionSuppression::Turn);
    r.set_model("glm-4.6".into());
    assert_eq!(
        r.compaction_suppression(),
        CompactionSuppression::Turn,
        "model switch leaves Turn to the turn-start self-heal"
    );
}

#[test]
fn test_heal_clears_turn_level() {
    let r = runner();
    r.set_compaction_suppression(CompactionSuppression::Turn);
    r.heal_turn_start_suppression();
    assert_eq!(
        r.compaction_suppression(),
        CompactionSuppression::None,
        "Turn self-heals"
    );

    r.set_compaction_suppression(CompactionSuppression::Sticky);
    r.heal_turn_start_suppression();
    assert_eq!(
        r.compaction_suppression(),
        CompactionSuppression::Sticky,
        "Sticky survives the turn-start heal"
    );
}

#[test]
fn test_three_failures_trip_breaker() {
    // A transient failure (one that might clear on its own) self-heals each
    // turn, so the streak counter is the only thing stopping a persistently
    // failing transient cause from retrying every turn. Two failures stay
    // per-turn; the third trips the circuit breaker so auto-compact stops
    // hammering a doomed compact.
    let r = runner();
    r.record_compaction_failure(SuppressionCause::Transient);
    assert_eq!(
        r.compaction_suppression(),
        CompactionSuppression::Turn,
        "first transient failure self-heals next turn"
    );
    r.record_compaction_failure(SuppressionCause::Transient);
    assert_eq!(r.compaction_suppression(), CompactionSuppression::Turn);
    r.record_compaction_failure(SuppressionCause::Transient);
    assert_eq!(
        r.compaction_suppression(),
        CompactionSuppression::Sticky,
        "third consecutive transient failure trips the circuit breaker"
    );
}

#[test]
fn test_fatal_failure_persists_immediately() {
    // A fatal cause (a corrupt log, or a no-progress compact that is still
    // over-window) persists on the first failure — retrying cannot help, so
    // no streak is needed.
    let r = runner();
    r.record_compaction_failure(SuppressionCause::CorruptLog);
    assert_eq!(r.compaction_suppression(), CompactionSuppression::Sticky);
    r.set_compaction_suppression(CompactionSuppression::None);
    r.record_compaction_failure(SuppressionCause::NoProgress);
    assert_eq!(r.compaction_suppression(), CompactionSuppression::Sticky);
}

#[test]
fn test_success_resets_failure_streak() {
    // A successful compact wipes the streak so a future transient blip starts
    // the count fresh, not one failure from a circuit-break trip.
    let r = runner();
    r.record_compaction_failure(SuppressionCause::Transient);
    r.record_compaction_failure(SuppressionCause::Transient);
    // Simulate the success path: clear suppress + reset the streak.
    r.set_compaction_suppression(CompactionSuppression::None);
    r.compaction_transient_failures
        .store(0, std::sync::atomic::Ordering::Relaxed);
    r.record_compaction_failure(SuppressionCause::Transient);
    assert_eq!(
        r.compaction_suppression(),
        CompactionSuppression::Turn,
        "streak reset on success — one failure is per-turn, not a trip"
    );
}

#[test]
fn test_model_switch_resets_streak() {
    // A context-budget change (model switch / rewind) is a fresh start: a
    // prior transient streak no longer applies under the new window.
    let r = runner();
    r.record_compaction_failure(SuppressionCause::Transient);
    r.record_compaction_failure(SuppressionCause::Transient);
    r.clear_sticky_compaction_suppression();
    r.record_compaction_failure(SuppressionCause::Transient);
    assert_eq!(
        r.compaction_suppression(),
        CompactionSuppression::Turn,
        "streak reset on context-budget change"
    );
}

#[tokio::test]
async fn test_auto_failure_suppression() {
    use houyicoder_async::PFut;
    use houyicoder_context::{ContextBackend, ContextError, EventId, SessionLogEntry};

    /// A backend whose write_checkpoint always fails, so the auto compact's
    /// commit errors + the suppress is set. append succeeds so the
    /// TurnStarted marker + replay work.
    struct FailingBackend;
    impl ContextBackend for FailingBackend {
        fn append(&self, _e: SessionLogEntry) -> PFut<'_, Result<EventId, ContextError>> {
            Box::pin(async { Ok(EventId::new()) })
        }
        fn read_range(
            &self,
            _s: SessionId,
            _from: Option<EventId>,
            _to: Option<EventId>,
        ) -> PFut<'_, Result<Vec<SessionLogEntry>, ContextError>> {
            Box::pin(async { Ok(vec![]) })
        }
        fn replay(&self, _s: SessionId) -> PFut<'_, Result<Vec<SessionLogEntry>, ContextError>> {
            Box::pin(async { Ok(vec![]) })
        }
        fn write_checkpoint(
            &self,
            _m: houyicoder_context::CheckpointManifest,
        ) -> PFut<'_, Result<houyicoder_context::CheckpointId, ContextError>> {
            Box::pin(async { Err(ContextError::Io) })
        }
        fn read_checkpoint(
            &self,
            _id: houyicoder_context::CheckpointId,
        ) -> PFut<'_, Result<houyicoder_context::CheckpointManifest, ContextError>> {
            Box::pin(async { Err(ContextError::NotFound) })
        }
        fn list_checkpoints(
            &self,
            _s: SessionId,
        ) -> PFut<'_, Result<Vec<houyicoder_context::CheckpointId>, ContextError>> {
            Box::pin(async { Ok(vec![]) })
        }
    }

    let store = std::sync::Arc::new(SessionStore::new(Box::new(FailingBackend)));
    let r = Runner::new(
        store,
        Arc::new(crate::provider::test_support::FakeProvider::text("ok")),
        ToolRegistry::new(),
        RunnerConfig {
            max_turns: 5,
            ..RunnerConfig::default()
        },
    );
    // compress is the auto path; the commit fails (transient I/O →
    // Turn). A manual /compact would bypass the suppress.
    let _outcome = r.compress(SessionId::new()).await;
    assert_eq!(
        r.compaction_suppression(),
        CompactionSuppression::Turn,
        "auto compact failure sets a Turn-level suppress"
    );
}

/// A successful compact clears the stale last_turn_delta so
/// conservative_input_tokens does not floor to the pre-compact provider
/// observation on the post-compact context. Pins the fix: after a mid-turn
/// compaction, the context shrinks but last_turn_delta held the
/// pre-compact input tokens, so max(estimate, stale) re-tripped the gate.
#[tokio::test]
async fn test_compact_clears_stale_delta() {
    use houyicoder_context::{EventId, SessionEvent, SessionLogEntry};
    use houyicoder_protocol::llm::Usage;
    let r = runner();
    let session = SessionId::new();
    // Seed events so compress has something to fold.
    for i in 0..6 {
        r.store()
            .append(SessionLogEntry {
                id: EventId::new(),
                session,
                ts: 0,
                prev_hash: None,
                event: if i == 0 {
                    SessionEvent::UserInput {
                        text: "do the work".into(),
                    }
                } else {
                    SessionEvent::AssistantMessage {
                        text: format!("response {i}"),
                        thinking: None,
                    }
                },
            })
            .await
            .unwrap();
    }
    // Simulate a prior provider turn that reported input_tokens (sets
    // last_turn_delta — the stale floor conservative_input_tokens uses).
    {
        let usage = Usage {
            input_tokens: 180_000,
            output_tokens: 500,
            total_tokens: 180_500,
            non_cached_input_tokens: 180_000,
            cache_read_input_tokens: 0,
            cache_write_input_tokens: 0,
            reasoning_tokens: 0,
        };
        let mut ol = r.observability.lock().unwrap();
        ol.record_usage("test", &usage, 180_000, 120, 100, 200_000, 32_768);
    }
    // Precondition: last_turn_delta is set.
    assert!(
        r.observability.lock().unwrap().last_turn_delta().is_some(),
        "precondition: last_turn_delta set after record_usage"
    );
    // Compact succeeds (folds the older events) — clears the stale
    // last_turn_delta so the gate reads the post-compact estimate.
    let _outcome = r.compress(session).await;
    assert!(
        r.observability.lock().unwrap().last_turn_delta().is_none(),
        "compact must clear stale last_turn_delta so the gate does not floor to the pre-compact value"
    );
}
