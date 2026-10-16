//! Builds the server-owned part of a status response: the config display
//! fields and the per-model usage view the server attaches to the
//! StatusSnapshot after map_status_snapshot builds the runner-owned fields.
//! Pure readers of config and the observability log, with no Server state.

use houyicoder_core::observability::ModelUsage;
use houyicoder_protocol::frontend::status::ModelUsageView;

/// Whether the user settings file exists: "User" when it does, else
/// "(defaults)". It names that one layer only: the project and local settings
/// files are merged into the values a session runs on, but this label does not
/// yet report them, so a repository that pins an endpoint still reads "User"
/// here.
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
