//! Atomic inference state and its per-request snapshot.

pub mod effort;
pub(crate) mod model_display;
pub mod model_window;

use houyicoder_protocol::frontend::model::{ModelChoice, SpeedMode};
use houyicoder_protocol::llm::EffortLevel;

/// The runtime inference state, held under one lock on the Runner. The model
/// id, effort and speed tier move together at a request boundary; a switch
/// swaps the whole config so a request never reads a half-applied mix. The
/// selection intent is stored, not derived from id equality — but a resume
/// rebuilds it from the descriptor model as an explicit pick.
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
mod tests;
