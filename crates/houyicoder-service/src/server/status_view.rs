//! Builds the server-owned part of a status response: the config display
//! fields and the per-model usage view the server attaches to the
//! StatusSnapshot after map_status_snapshot builds the runner-owned fields.
//! Pure readers of config and the observability log, with no Server state.

use houyicoder_core::observability::ModelUsage;
use houyicoder_protocol::frontend::status::ModelUsageView;
use std::path::Path;

/// Whether the server's own settings file exists. The label reads User when
/// the file is present and the defaults otherwise. It reads the path the
/// server was assembled with, not the ambient config, so a server that owns
/// no settings backend reports the defaults rather than the host file. It
/// names that one layer only, since the project and local settings files are
/// merged into the values a session runs on but this label does not report
/// them, so a repository that pins an endpoint still reads User here.
pub(super) fn setting_sources_label(settings_path: Option<&Path>) -> String {
    if settings_path.is_some_and(Path::exists) {
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
            provenance: u.provenance,
        })
        .collect()
}

#[cfg(test)]
mod tests;
