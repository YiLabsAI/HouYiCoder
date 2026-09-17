//! The capability projection both the /model pane and the request path
//! consume: for any model id, the window, the output cap, the effort dialect
//! and the Fast tier. The host answers these so the guest never probes an id
//! for substrings — what a model supports is the host's answer.
//!
//! One resolver serves both consumers, so the window a pane row shows is the
//! window the next request is gated against; the two can never drift apart
//! again.

use houyicoder_protocol::frontend::model::{
    ContextWindow, ContextWindowSource, EffortCapability, FastModeAvailability,
    ModelDisplayCapabilities,
};

use crate::agent::{ModelCatalogResolver, Runner, model_window};

/// The resolved request facts a frozen inference snapshot carries: the
/// context window the pre-flight gate uses and the output cap the request
/// body sends, both from the same chain the pane displays.
pub struct ResolvedRequestCapabilities {
    pub context_window: u32,
    pub max_output_tokens: u32,
}

impl Runner {
    /// The installed catalog resolver, for callers applying picks outside
    /// the request path.
    pub fn catalog_resolver(&self) -> Option<&dyn ModelCatalogResolver> {
        self.catalog_resolver.as_deref()
    }

    /// Resolve the output-token cap the next request carries, same-source for
    /// the pre-flight reserve and the request body (no overflow when the two
    /// disagree). A catalog override (ModelEntry.max_output_tokens) wins over
    /// the construction-time config value; otherwise the config value stands
    /// (the family default resolved at the composition root).
    pub fn resolve_max_output_tokens(&self) -> u32 {
        self.resolve_max_output_tokens_for(&self.active_model())
    }

    /// The same resolution for an arbitrary model id, so the pane can show a
    /// row's cap without switching the session to it.
    pub fn resolve_max_output_tokens_for(&self, model: &str) -> u32 {
        let resolved = self
            .catalog_resolver
            .as_deref()
            .and_then(|r| r.catalog_max_output_tokens(model))
            .unwrap_or(self.config.max_output_tokens);
        // The provider's declared cap is its own real limit; take the min so
        // a provider reporting a smaller cap (tests, a constrained gateway)
        // is respected — the catalog default is a fallback for unknown
        // families, not a floor that overstates the provider's actual room.
        let provider_cap = self.provider.capabilities().max_output_tokens;
        resolved.min(provider_cap)
    }

    /// The capabilities a request on this model is gated by — the same values
    /// display_capabilities reports, resolved here once so the frozen
    /// inference snapshot and the pane row cannot disagree.
    pub fn resolve_request_capabilities(&self, model: &str) -> ResolvedRequestCapabilities {
        let (window, _source) = self.resolve_window_info_for(model);
        ResolvedRequestCapabilities {
            context_window: window,
            max_output_tokens: self.resolve_max_output_tokens_for(model),
        }
    }

    /// The display capabilities the pane renders for a model id. Same
    /// resolution chain as resolve_request_capabilities, one pass over the
    /// window chain.
    pub fn display_capabilities(&self, model: &str) -> ModelDisplayCapabilities {
        let (window, source) = self.resolve_window_info_for(model);
        // A Fallback source means the id is unknown to every layer (no
        // learned limit, no opt-in, no config, no family table, no provider
        // window). Show nothing rather than a made-up number: the conservative
        // 200K default still gates the request, but the pane must not present
        // a guess as the model's real window.
        ModelDisplayCapabilities {
            context_window: (source != ContextWindowSource::Fallback).then_some(ContextWindow {
                tokens: window,
                source,
            }),
            max_output_tokens: Some(self.resolve_max_output_tokens_for(model)),
            effort: match model_window::effort_dialect(model) {
                model_window::EffortDialect::NotSupported => EffortCapability::Unsupported,
                // The dialect's set, minus what the catalog's per-model
                // list excludes.
                _ => EffortCapability::Supported {
                    levels: super::effort::effort_levels_for(
                        model,
                        self.catalog_resolver.as_deref(),
                    ),
                },
            },
            fast: self.fast_availability(model),
        }
    }

    /// The window chain for a model id: learned > [1m] suffix > the user's
    /// catalog override > the shipped family table > the provider's declared
    /// window > the conservative default. One call site so the chain lives in
    /// one place for both consumers.
    fn resolve_window_info_for(&self, model: &str) -> (u32, ContextWindowSource) {
        model_window::resolve_window_info(
            model,
            self.provider.capabilities().context_window,
            self.catalog_resolver
                .as_deref()
                .and_then(|r| r.catalog_context_window(model)),
        )
    }

    /// Whether a model can run Fast, and why not when it cannot. The catalog
    /// is the only source: no name probe grants the tier, because a family
    /// name is no evidence that an endpoint serves a faster variant of it.
    fn fast_availability(&self, model: &str) -> FastModeAvailability {
        match self
            .catalog_resolver
            .as_deref()
            .and_then(|r| r.catalog_fast(model))
        {
            Some(true) => FastModeAvailability::Available,
            Some(false) => FastModeAvailability::Unavailable {
                reason: "disabled in the model catalog".into(),
            },
            None => FastModeAvailability::NotConfigured,
        }
    }
}
