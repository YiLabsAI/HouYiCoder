//! Tests for this module.

use super::*;
use crate::agent::cache_liveness::CachedPrefixState;
use crate::agent::retention::{RetentionContext, RetentionDecision, RetentionPolicy};
use houyicoder_context::{EventId, SessionEvent, SessionId, SessionLogEntry};
use std::sync::Mutex;

use crate::agent::ToolRegistry;
use crate::agent::runner_config::RunnerConfig;
use crate::agent::{ModelCatalogResolver, Runner};
use crate::provider::test_support::FakeProvider;
use houyicoder_memory::InMemoryBackend;
use houyicoder_session::SessionStore;
use std::sync::Arc;
use std::sync::atomic::Ordering;

fn runner_with(model: &str) -> Runner {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    Runner::new(
        store,
        Arc::new(FakeProvider::text("x")),
        ToolRegistry::new(),
        RunnerConfig {
            model: model.into(),
            ..RunnerConfig::default()
        },
    )
}

/// The frozen snapshot carries the model the request serves, so a switch
/// landing mid-request cannot rewrite the attribution of the in-flight
/// call.
#[test]
fn test_snapshot_isolated_from_switch() {
    let runner = runner_with("model-a");
    let frozen = runner.snapshot_inference();
    assert_eq!(frozen.model, "model-a");
    runner.apply_inference(InferenceConfig::for_model("model-b".into(), true));
    assert_eq!(
        frozen.model, "model-a",
        "the frozen copy does not follow the live state"
    );
    assert_eq!(runner.active_model(), "model-b");
}

/// Cache side effects fire at the request boundary, not when the pick
/// lands.
#[test]
fn test_switch_effects_at_boundary() {
    let runner = runner_with("model-a");
    runner.snapshot_inference();
    assert!(!runner.cache_model_switch_flag.load(Ordering::Relaxed));
    runner.apply_inference(InferenceConfig::for_model("model-b".into(), true));
    assert!(
        !runner.cache_model_switch_flag.load(Ordering::Relaxed),
        "the pick alone does not flag a switch"
    );
    runner.snapshot_inference();
    assert!(
        runner.cache_model_switch_flag.load(Ordering::Relaxed),
        "the first request on the new id flags it"
    );
}

/// A catalog that declares whether the seeded model serves a Fast tier.
struct FastCatalog(bool);
impl ModelCatalogResolver for FastCatalog {
    fn catalog_effort(&self, _model: &str) -> Option<EffortLevel> {
        None
    }
    fn catalog_fast(&self, _model: &str) -> Option<bool> {
        Some(self.0)
    }
}

/// A persisted Fast tier survives the restart seed when the model serves
/// it, and clamps to Standard when it does not - the same boundary the
/// pick path clamps on.
#[test]
fn test_speed_seed_restores_persisted() {
    let runner = runner_with("qwen3.7-max")
        .with_catalog_resolver(Arc::new(FastCatalog(true)))
        .with_speed_mode(SpeedMode::Fast);
    assert_eq!(
        runner.active_speed(),
        SpeedMode::Fast,
        "a served tier carries over the restart"
    );
}

#[test]
fn test_speed_seed_clamps_unserved() {
    let runner = runner_with("qwen3.7-max")
        .with_catalog_resolver(Arc::new(FastCatalog(false)))
        .with_speed_mode(SpeedMode::Fast);
    assert_eq!(
        runner.active_speed(),
        SpeedMode::Standard,
        "a tier the model cannot serve clamps at the seed"
    );
}

/// Serves Fast for the listed models only, so a swap to an unlisted id
/// hits the clamp.
struct FastFor(Vec<&'static str>);
impl ModelCatalogResolver for FastFor {
    fn catalog_effort(&self, _model: &str) -> Option<EffortLevel> {
        None
    }
    fn catalog_fast(&self, model: &str) -> Option<bool> {
        Some(self.0.contains(&model))
    }
}

/// A model swap (--model override, child spawn) preserves the session's
/// tier, but a target model that cannot serve Fast clamps to Standard:
/// the override path must not ask a model for a tier it lacks.
#[test]
fn test_set_model_clamps_speed() {
    let runner = runner_with("qwen3.7-max")
        .with_catalog_resolver(Arc::new(FastFor(vec!["qwen3.7-max"])))
        .with_speed_mode(SpeedMode::Fast);
    runner.set_model("deepseek-chat".into());
    assert_eq!(runner.active_model(), "deepseek-chat");
    assert_eq!(
        runner.active_speed(),
        SpeedMode::Standard,
        "a swap onto a model without Fast drops the tier"
    );
    runner.set_model("qwen3.7-max".into());
    assert_eq!(
        runner.active_speed(),
        SpeedMode::Standard,
        "the clamp sticks: a later served model does not re-earn the tier"
    );
}

/// A model switch resets the cached-prefix decisions before the request is
/// assembled. The probe records the generation it sees while the assembly
/// consults a retention decision; moving the snapshot after assembly would
/// show the old generation here.
#[test]
fn test_switch_resets_before_assembly() {
    struct GenerationProbe {
        cached: Arc<CachedPrefixState>,
        seen: Mutex<Option<u64>>,
    }
    impl RetentionPolicy for GenerationProbe {
        fn decide(&self, ctx: &RetentionContext) -> RetentionDecision {
            if ctx.block_ref.is_some() {
                *self.seen.lock().expect("probe") = Some(self.cached.generation());
            }
            RetentionDecision::Evict
        }
    }

    let runner = runner_with("model-a");
    let probe = Arc::new(GenerationProbe {
        cached: Arc::clone(runner.cached_prefix()),
        seen: Mutex::new(None),
    });
    runner
        .context_builder
        .set_retention_policy(Arc::clone(&probe) as Arc<dyn RetentionPolicy>);
    runner.snapshot_inference();
    assert_eq!(runner.cached_prefix().generation(), 0);
    runner.apply_inference(InferenceConfig::for_model("model-b".into(), true));

    let entry = |ts: u64, event: SessionEvent| SessionLogEntry {
        id: EventId::new(),
        session: SessionId::new(),
        ts,
        prev_hash: None,
        event,
    };
    let events = vec![
        entry(0, SessionEvent::UserInput { text: "u".into() }),
        entry(
            1,
            SessionEvent::AssistantMessage {
                text: "a".into(),
                thinking: None,
            },
        ),
        entry(
            2,
            SessionEvent::ToolCall {
                call_id: "c1".into(),
                tool: "read".into(),
                input: serde_json::json!({"path": "a.rs"}),
            },
        ),
        entry(
            3,
            SessionEvent::ToolResult {
                call_id: "c1".into(),
                output: serde_json::json!({
                    "block_ref": "hash-1",
                    "preview": "p",
                    "hint": "compacted",
                }),
                duration_ms: 1,
            },
        ),
    ];
    let (_inference, _assembled) = runner.assemble_request(&events, None);
    assert_eq!(
        *probe.seen.lock().expect("probe"),
        Some(1),
        "assembly must see the generation the switch bumped, not the old one"
    );
}

/// Switching to a model from a family with a different output-token cap
/// resolves the new model's family cap rather than keeping the startup
/// model's value.
#[test]
fn test_switch_updates_output_cap() {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let runner = Runner::new(
        store,
        Arc::new(FakeProvider::text("x")),
        ToolRegistry::new(),
        RunnerConfig {
            model: "qwen3.6-flash".into(),
            max_output_tokens: 1_000,
            ..RunnerConfig::default()
        },
    );
    assert_eq!(runner.resolve_max_output_tokens(), 1_000);
    assert_eq!(
        runner.resolve_max_output_tokens_for("deepseek-v4.1-flash-1017"),
        8_000
    );
}
