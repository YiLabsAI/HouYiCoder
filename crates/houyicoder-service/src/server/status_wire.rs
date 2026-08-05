//! Status-snapshot sidecar attachers: the env/config display fields + the
//! per-model usage projection the server attaches to the wire StatusSnapshot
//! after project_status builds the runner-owned fields. Split from dispatch
//! so that file stays under the size gate; these are pure readers of env +
//! the observability log, with no Server state.

use houyicoder_protocol::frontend::status::ModelUsageView;

/// The env var name that provides the auth token (DASHSCOPE / OPENAI /
/// HOUYICODER), or None. Returns the source name only, never the value.
pub(super) fn auth_token_source() -> Option<String> {
    use houyicoder_config::{ENV_DASHSCOPE_API_KEY, ENV_HOUYICODER_API_KEY, ENV_OPENAI_API_KEY};
    if std::env::var(ENV_DASHSCOPE_API_KEY)
        .map(|v| !v.is_empty())
        .unwrap_or(false)
    {
        Some(ENV_DASHSCOPE_API_KEY.to_string())
    } else if std::env::var(ENV_OPENAI_API_KEY)
        .map(|v| !v.is_empty())
        .unwrap_or(false)
    {
        Some(ENV_OPENAI_API_KEY.to_string())
    } else if std::env::var(ENV_HOUYICODER_API_KEY)
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

/// Trim the engine per-model usage to the wire view the Usage tab renders:
/// token counts only, no USD, no capability fields. The order is whatever
/// Runner::by_model_usage returns (already sorted heaviest-first).
pub(super) fn project_by_model(
    entries: Vec<(String, houyicoder_core::observability::ModelUsage)>,
) -> Vec<ModelUsageView> {
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
mod tests {
    use super::*;
    use houyicoder_core::observability::ModelUsage;

    #[test]
    fn test_project_model_trims_fields() {
        // USD, web search, context window, and max output tokens are dropped:
        // the Usage tab renders token counts only, and capability fields
        // belong to the /model pane, not the usage breakdown.
        let entries = vec![(
            "glm-5.2".to_string(),
            ModelUsage {
                input_tokens: 1000,
                output_tokens: 500,
                cache_read_tokens: 800,
                cache_write_tokens: 0,
                reasoning_tokens: 200,
                web_search_requests: 9,
                cost_usd: 1.5,
                context_window: 200_000,
                max_output_tokens: 8192,
            },
        )];
        let view = project_by_model(entries);
        assert_eq!(view.len(), 1);
        assert_eq!(view[0].model, "glm-5.2");
        assert_eq!(view[0].input_tokens, 1000);
        assert_eq!(view[0].output_tokens, 500);
        assert_eq!(view[0].cache_read_tokens, 800);
        assert_eq!(view[0].cache_write_tokens, 0);
        assert_eq!(view[0].reasoning_tokens, 200);
    }

    #[test]
    fn test_project_model_preserves_order() {
        // The caller (Runner::by_model_usage) sorts heaviest-first; the
        // projection must not reorder, only trim.
        let entries = vec![
            ("qwen3.7-max".to_string(), ModelUsage::default()),
            ("glm-5.2".to_string(), ModelUsage::default()),
        ];
        let view = project_by_model(entries);
        assert_eq!(
            view.iter().map(|m| m.model.as_str()).collect::<Vec<_>>(),
            ["qwen3.7-max", "glm-5.2"],
            "order preserved as given"
        );
    }
}
