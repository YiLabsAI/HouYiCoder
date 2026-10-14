//! Builds the server-owned part of a status response: the env/config display
//! fields and the per-model usage view the server attaches to the
//! StatusSnapshot after map_status_snapshot builds the runner-owned fields.
//! Pure readers of env and the observability log, with no Server state.

use houyicoder_config::{ENV_DASHSCOPE_API_KEY, ENV_HOUYICODER_API_KEY, ENV_OPENAI_API_KEY};
use houyicoder_core::observability::ModelUsage;
use houyicoder_protocol::frontend::status::ModelUsageView;
use std::env::var;

/// The env var name that provides the auth token (DASHSCOPE / OPENAI /
/// HOUYICODER), or None. Returns the source name only, never the value.
pub(super) fn auth_token_source() -> Option<String> {
    if var(ENV_DASHSCOPE_API_KEY)
        .map(|v| !v.is_empty())
        .unwrap_or(false)
    {
        Some(ENV_DASHSCOPE_API_KEY.to_string())
    } else if var(ENV_OPENAI_API_KEY)
        .map(|v| !v.is_empty())
        .unwrap_or(false)
    {
        Some(ENV_OPENAI_API_KEY.to_string())
    } else if var(ENV_HOUYICODER_API_KEY)
        .map(|v| !v.is_empty())
        .unwrap_or(false)
    {
        Some(ENV_HOUYICODER_API_KEY.to_string())
    } else {
        None
    }
}

/// Which settings sources contribute: "User" when the user settings file
/// exists, else "(defaults)". Project-local settings are not wired yet.
pub(super) fn setting_sources_label() -> String {
    if houyicoder_config::settings_path().exists() {
        "User".to_string()
    } else {
        "(defaults)".to_string()
    }
}

/// Trim the engine per-model usage to the view the Usage tab renders:
/// token counts only, no USD, no capability fields. The order is whatever
/// Runner::by_model_usage returns (already sorted heaviest-first).
pub(super) fn build_model_usage_view(entries: Vec<(String, ModelUsage)>) -> Vec<ModelUsageView> {
    entries
        .into_iter()
        .map(|(model, u)| ModelUsageView {
            model,
            input_tokens: u.input_tokens,
            output_tokens: u.output_tokens,
            cache_read_tokens: u.cache_read_tokens,
            cache_write_tokens: u.cache_write_tokens,
            reasoning_tokens: u.reasoning_tokens,
        })
        .collect()
}

#[cfg(test)]
mod tests;
