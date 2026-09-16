//! Atomic inference state and its per-request snapshot.

use houyicoder_protocol::frontend::model::{ModelChoice, SpeedMode};
use houyicoder_protocol::llm::EffortLevel;

/// The runtime inference state, held under one lock on the Runner. The model
/// id, effort and speed tier move together at a request boundary; a switch
/// swaps the whole config so a request never reads a half-applied mix. The
/// selection intent is stored, not derived from id equality — but a resume
/// rebuilds it from the sidecar model as an explicit pick.
#[derive(Clone, Debug, Default)]
pub struct InferenceConfig {
    pub choice: ModelChoice,
    pub model: String,
    pub effort: Option<EffortLevel>,
    pub speed: SpeedMode,
}

impl InferenceConfig {
    /// Build the initial config from the resolved model id the composition
    /// root assembled. Effort starts auto; speed starts Standard.
    pub fn for_model(model: String, explicit: bool) -> Self {
        let choice = if explicit {
            ModelChoice::Explicit { id: model.clone() }
        } else {
            ModelChoice::Default
        };
        Self {
            choice,
            model,
            effort: None,
            speed: SpeedMode::Standard,
        }
    }
}
/// A frozen view of the inference config plus the resolved request facts,
/// captured once at the start of a model request. Every reader in the request
/// lifetime consumes this snapshot rather than the live state or the
/// construction-time config, so a switch that lands mid-request cannot hand
/// any of them a mixed config or attribute the old request onto the new
/// model: the response's model field, the truncation verdict, usage records,
/// overflow learning and cache attribution all read this one copy.
#[derive(Clone, Debug, Default)]
pub struct RequestInferenceConfig {
    pub model: String,
    pub effort: Option<EffortLevel>,
    pub speed: SpeedMode,
    pub context_window: u32,
    pub max_output_tokens: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::runner_config::RunnerConfig;
    use crate::agent::{ModelCatalogResolver, Runner};
    use std::sync::Arc;
    use std::sync::atomic::Ordering;

    fn runner_with(model: &str) -> Runner {
        let store = Arc::new(houyicoder_session::SessionStore::new(Box::new(
            houyicoder_memory::InMemoryBackend::new(),
        )));
        Runner::with_shared_store(
            store,
            Arc::new(crate::provider::test_support::FakeProvider::text("x")),
            crate::agent::ToolRegistry::new(),
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
}
