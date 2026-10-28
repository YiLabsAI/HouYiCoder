//! The settings a session runs on: the model, effort and speed pick the user
//! holds, the frozen per-request view of it, and the builders that install it.
//! A pick lands as one config so a request never reads a half-applied mix.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use houyicoder_protocol::frontend::model::{ModelChoice, SpeedMode};
use houyicoder_protocol::llm::EffortLevel;

use super::effort::{ModelCatalogResolver, resolve_applied_effort};
use super::{InferenceConfig, RequestInferenceConfig};
use crate::agent::Runner;

impl Runner {
    /// The active model id (the /model pane select). Reads the atomic
    /// inference state; apply_inference swaps it so the next completion serves
    /// the new id without rebuilding the provider.
    pub fn active_model(&self) -> String {
        self.inference
            .read()
            .map(|c| c.model.clone())
            .unwrap_or_default()
    }

    /// The stored selection intent (Default vs Explicit), not derived from id
    /// equality. A pick that resolved to the default id but was explicit stays
    /// Explicit — the receipt and the snapshot report the real intent.
    pub fn active_choice(&self) -> ModelChoice {
        self.inference
            .read()
            .map(|c| c.choice.clone())
            .unwrap_or_default()
    }

    /// The active effort pick, or None to follow the resolution chain.
    pub fn active_effort(&self) -> Option<EffortLevel> {
        self.inference.read().map(|c| c.effort).ok().flatten()
    }

    /// The active speed tier. Reported in the receipt so the pane describes the
    /// tier the session holds rather than the draft's ask.
    pub fn active_speed(&self) -> SpeedMode {
        self.inference
            .read()
            .map(|c| c.speed)
            .unwrap_or(SpeedMode::Standard)
    }

    /// Resolve the effort level the next completion request should carry,
    /// following the chain: active pick → catalog → per-model default → None.
    /// Short-circuits to None for a model the dialect probe does not recognize.
    pub fn resolve_applied_effort(&self) -> Option<EffortLevel> {
        let (model, effort) = self
            .inference
            .read()
            .map(|c| (c.model.clone(), c.effort))
            .ok()
            .unwrap_or_default();
        resolve_applied_effort(&model, effort, self.catalog_resolver.as_deref())
    }

    /// Frozen per-request view of the inference config with resolved
    /// capabilities. Also the request boundary where a model switch's cache
    /// side effects fire.
    pub fn snapshot_inference(&self) -> RequestInferenceConfig {
        let cfg = self.inference.read().map(|c| c.clone()).unwrap_or_default();
        let previous = self.last_request_model.lock().ok().and_then(|m| m.clone());
        // The first request has no prior id to switch from, so it only seeds
        // the marker; every later request compares against the id the
        // previous request ran on, so an active request's attribution
        // survives a pick made during the run.
        if let Some(previous) = previous.as_deref()
            && previous != cfg.model
        {
            self.clear_sticky_compaction_suppression();
            self.cached_prefix.invalidate();
            if let Ok(mut ol) = self.observability.lock() {
                ol.clear_last_turn_delta();
            }
            self.cache_model_switch_flag.store(true, Ordering::Relaxed);
        }
        if let Ok(mut m) = self.last_request_model.lock() {
            *m = Some(cfg.model.clone());
        }
        let capabilities = self.resolve_request_capabilities(&cfg.model);
        RequestInferenceConfig {
            model: cfg.model,
            effort: cfg.effort,
            speed: cfg.speed,
            context_window: capabilities.context_window,
            max_output_tokens: capabilities.max_output_tokens,
        }
    }

    /// Apply a model pick atomically: choice, model, effort and speed land
    /// together under one write. Change detection is not done here — the
    /// next request boundary (snapshot_inference) compares against the id
    /// the previous request ran on, so racing applies cannot slip past it.
    pub fn apply_inference(&self, next: InferenceConfig) {
        if let Ok(mut current) = self.inference.write() {
            *current = next;
        }
    }

    /// Set the active effort alone (the path that starts a multi-agent child; the
    /// child runs on its assembled model at Standard speed, the same cost
    /// discipline that holds children at the lowest effort tier). Prefer
    /// apply_inference for a pick that carries model and speed together.
    pub fn set_effort(&self, effort: Option<EffortLevel>) {
        if let Ok(mut c) = self.inference.write() {
            c.effort = effort;
        }
    }

    /// Install the catalog resolver (the composition root injects an impl
    /// backed by the loaded ModelSection). Builder-style so the runner
    /// assembles in one statement.
    pub fn with_catalog_resolver(mut self, resolver: Arc<dyn ModelCatalogResolver>) -> Self {
        self.catalog_resolver = Some(resolver);
        self
    }

    /// Set the initial selection intent (the composition root passes Default
    /// when it resolved the sentinel).
    pub fn with_model_choice(self, choice: ModelChoice) -> Self {
        if let Ok(mut c) = self.inference.write() {
            c.choice = choice;
        }
        self
    }

    /// Restore the speed tier the user persisted, so a restart keeps it.
    /// A startup model with no fast tier falls back to Standard here;
    /// install the catalog resolver first, as the fallback consults it.
    pub fn with_speed_mode(self, speed: SpeedMode) -> Self {
        let model = self
            .inference
            .read()
            .map(|c| c.model.clone())
            .unwrap_or_default();
        let effective =
            if speed == SpeedMode::Fast && !self.display_capabilities(&model).fast.is_available() {
                SpeedMode::Standard
            } else {
                speed
            };
        if let Ok(mut c) = self.inference.write() {
            c.speed = effective;
        }
        self
    }

    /// Switch the active model id (the --model flag override and the path
    /// that starts a child), keeping the current effort and speed. A Fast
    /// tier the target model lacks falls back to Standard here, the same
    /// clamp the pick path applies.
    pub fn set_model(&self, model: String) {
        let mut next = self
            .inference
            .read()
            .map(|c| c.clone())
            .unwrap_or_else(|_| InferenceConfig::for_model(model.clone(), true));
        next.choice = ModelChoice::Explicit { id: model.clone() };
        next.model = model;
        if next.speed == SpeedMode::Fast
            && !self.display_capabilities(&next.model).fast.is_available()
        {
            next.speed = SpeedMode::Standard;
        }
        self.apply_inference(next);
    }
}
