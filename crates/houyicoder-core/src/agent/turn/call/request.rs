//! Assembling the request one model call sends.

use houyicoder_protocol::llm::{CompletionRequest, ModelSettings};

use crate::agent::Runner;
use crate::agent::context::AssembledContext;
use crate::agent::model_window::normalize_model_for_api;

impl Runner {
    /// Build the request this turn sends: the instructions with the dynamic
    /// suffix, the assembled messages and their boundary, the tools, and the
    /// cache declarations. The endpoint's cache convention is resolved here
    /// because the provider holds no resolver, and a model that declares none
    /// keeps the request default.
    pub(super) fn build_request(
        &self,
        model: &str,
        assembled: AssembledContext,
        settings: ModelSettings,
    ) -> CompletionRequest {
        let mut request = CompletionRequest {
            model: normalize_model_for_api(model),
            instructions: assemble_instructions(&assembled.system, &self.config.instructions),
            // The configured instructions append after the dynamic suffix, so
            // the offset into the assembled system is still where the suffix
            // begins in the final string.
            dynamic_boundary: Some(assembled.dynamic_boundary),
            input: assembled.messages,
            tools: self.tools.tool_defs(),
            settings,
            cache_breakpoints: Vec::new(),
            cache_format: self
                .catalog_resolver
                .as_deref()
                .and_then(|r| r.catalog_cache_format(model))
                .unwrap_or_default(),
        };
        // The policy decides which cache positions exist; the provider lowers
        // each breakpoint to the form the endpoint declared.
        let policy = self.cache_policy.policy();
        self.cache_policy
            .apply(&mut request.cache_breakpoints, &policy);
        request
    }
}

/// Append configured instructions to the assembled system prompt. An empty
/// configured field returns the assembled system verbatim. Appending keeps
/// the static identity/framework prefix byte-stable for prompt-cache.
fn assemble_instructions(assembled_system: &str, configured: &str) -> String {
    if configured.is_empty() {
        assembled_system.to_string()
    } else {
        format!("{assembled_system}\n\n{configured}")
    }
}

#[cfg(test)]
mod tests;
