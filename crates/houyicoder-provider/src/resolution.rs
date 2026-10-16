//! Turning a project's configuration into the provider a session runs on: the
//! adapter itself, the endpoint it sends to, and where its key came from.
//!
//! The endpoint and the key origin travel with the provider instead of being
//! recomputed by whoever reports them. The merged settings are what the
//! provider was built from, and a later resolution that reads only the
//! environment names a different host whenever the settings file pins one.

use std::env::var;
use std::path::Path;
use std::sync::Arc;

use houyicoder_api::provider::ModelProvider;
use houyicoder_config::settings_merge::load_provider_settings;
use houyicoder_config::{
    ConfigWarning, DEFAULT_MODEL, ENV_HOUYICODER_TEST_STUB_RESPONSE_SCRIPT, api_key_env_with_name,
    resolve_base_url,
};
use houyicoder_protocol::llm::OutputItem;

use crate::FakeProvider;
use crate::OpenAiCompatibleProvider;
use crate::api_key;

/// What the provider's configuration resolved to, for a surface that reports
/// it: the endpoint the provider sends to, and a label naming where its key
/// came from.
///
/// Both fields answer one question — which endpoint, with which credential —
/// so they are carried together rather than as two fields on every owner.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProviderFacts {
    /// The endpoint the provider was built against.
    pub base_url: String,
    /// A label naming the key's origin, never the key itself. None when no key
    /// resolved at all.
    pub key_origin: Option<String>,
}

impl ProviderFacts {
    /// The facts for an owner that was never handed a resolution: the endpoint
    /// the environment names, plus an origin label that says the state out
    /// loud.
    ///
    /// The label is what keeps this honest. An environment-derived endpoint
    /// reported as if it were the resolution is exactly the defect this type
    /// exists to prevent, so a forgotten attach has to look unresolved rather
    /// than plausible.
    pub fn unresolved() -> Self {
        Self {
            base_url: resolve_base_url(),
            key_origin: Some("unresolved".into()),
        }
    }
}

/// A project's provider, together with what building it produced.
///
/// The warnings travel with the provider so a caller cannot reuse the provider
/// and drop the notice naming where the repo sends traffic.
#[derive(Clone)]
pub struct ProviderResolution {
    pub provider: Arc<dyn ModelProvider>,
    /// What the configuration resolved to, for a surface that reports it.
    pub facts: ProviderFacts,
    pub warnings: Vec<ConfigWarning>,
}

impl ProviderResolution {
    /// A stub provider that returns canned text without making external
    /// requests, used by callers that assemble a runner under test.
    pub fn stub() -> Self {
        Self {
            provider: Arc::new(FakeProvider::text("stub")),
            facts: ProviderFacts::default(),
            warnings: Vec::new(),
        }
    }
}

/// Resolve the provider for a workspace: the settings layers that apply to it,
/// the key those settings name, and the adapter that spends the key.
///
/// The workspace is a parameter rather than discovered here: finding it is the
/// composition root's decision, and a resolver that also walked the tree would
/// own two questions instead of one.
///
/// Resolved once at process start and reused for every session including a
/// swap. One resolution is correct for all of them only because the workspace
/// is fixed for the life of a process; a build that lets sessions carry
/// different workspaces must revisit this, because the endpoint and the
/// refused-helper warning both come from the workspace's own settings files.
pub fn resolve(workspace: Option<&Path>) -> ProviderResolution {
    let (settings, mut warnings) = load_provider_settings(workspace);
    // Test affordance: a scripted response sequence lets PTY UI tests drive
    // tool calls (glob / edit / todo_write) through the real binary so the
    // interaction layer — permission cards, tool-result rendering, transcript
    // fold — is exercised end-to-end. The env holds a JSON array of per-call
    // output-item lists; the first response typically carries a ToolCall, the
    // rest plain text so the run ends cleanly (a stateless stub re-emits the
    // same ToolCall every call and loops to max_turns). Falls through to the
    // normal stub path on any parse failure. Honest test knob, not a feature
    // — the stub providers exist for dev/test.
    if let Ok(raw) = var(ENV_HOUYICODER_TEST_STUB_RESPONSE_SCRIPT)
        && let Ok(per_call) = serde_json::from_str::<Vec<Vec<OutputItem>>>(&raw)
        && !per_call.is_empty()
    {
        // The scripted provider never resolves a key, so no origin is reported
        // and the settings warnings stay out: this branch answers with canned
        // output, and a notice about the repo's traffic would describe a
        // request that is not being made.
        return ProviderResolution {
            provider: Arc::new(FakeProvider::from_outputs(per_call)),
            facts: ProviderFacts {
                base_url: settings.base_url,
                key_origin: None,
            },
            warnings: Vec::new(),
        };
    }
    // The config layer names where the key comes from; obtaining it is a spawn,
    // so it happens here through the chokepoint.
    let resolved_key =
        api_key::resolve_api_key(settings.key_source.as_ref(), api_key_env_with_name(), None);
    warnings.extend(resolved_key.warnings);
    let facts = ProviderFacts {
        base_url: settings.base_url.clone(),
        key_origin: resolved_key.origin,
    };
    let provider: Arc<dyn ModelProvider> = match resolved_key.key {
        Some(key) => Arc::new(OpenAiCompatibleProvider::new(settings.base_url, key)),
        None => {
            let reply = format!(
                "stub mode: no api key resolved, model {DEFAULT_MODEL} not called. \
                 set keychain (macOS) or apiKeyHelper in settings.json \
                 (~/.houyicoder/), or export DASHSCOPE_API_KEY in the shell."
            );
            // The warnings ride along on the stub path too: a skipped
            // project helper is most relevant exactly when no key resolved.
            Arc::new(FakeProvider::text(&reply))
        }
    };
    ProviderResolution {
        provider,
        facts,
        warnings,
    }
}

#[cfg(test)]
mod tests;
