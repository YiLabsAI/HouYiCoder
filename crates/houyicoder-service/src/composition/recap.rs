//! Recap configuration resolution.
//!
//! The settings file names a switch and a window; the session names a model.
//! This module decides what the recap may run on, and reports every reason it
//! cannot, so the composition root and the live refresh apply one policy.
//!
//! An explicitly named model must be a catalog entry whose dialect can run
//! without thinking: the recap request carries no tools and no reasoning, so a
//! model that cannot turn thinking off would spend on something this feature
//! did not authorize. A section that names no model falls back to the session
//! model, then to the catalog, so enabling the switch on a fresh install
//! produces a summary instead of silently doing nothing.

use std::sync::Arc;

use houyicoder_api::provider::ModelProvider;
use houyicoder_api::session::SessionLog;
use houyicoder_config::ConfigWarning;
use houyicoder_config::ModelEntry;
use houyicoder_config::ModelSection;
use houyicoder_config::recap::RecapConfig;
use houyicoder_config::recap::load_recap_from;
use houyicoder_config::settings_path;
use houyicoder_context::SessionId;
use houyicoder_core::agent::model_window;
use houyicoder_session::SessionStore;

use crate::server::recap::RecapSession;
use crate::server::recap::RecapSettings;

/// Build the session's recap state from the settings file. The recap reads
/// and writes the session's own log through the same provider the session runs
/// on, so neither is a second source for the traffic it describes.
pub(super) fn build_recap(
    session: SessionId,
    section: &ModelSection,
    store: &Arc<SessionStore>,
    provider: &Arc<dyn ModelProvider>,
    session_model: &str,
) -> (Arc<RecapSession>, Vec<ConfigWarning>) {
    let (config, mut warnings) = load_recap_from(&settings_path());
    let (settings, model_warnings) = recap_settings(config, section, session_model);
    warnings.extend(model_warnings);
    let log: Arc<dyn SessionLog> = store.clone();
    let recap = Arc::new(RecapSession::new(
        session,
        settings,
        Arc::clone(provider),
        log,
    ));
    (recap, warnings)
}

/// Decide whether the configured recap may run, and say why when it may not.
///
/// A disabled section has nothing to refuse, so it resolves without a
/// warning. An enabled one resolves its model through the fallback chain and
/// keeps the feature off when no candidate can skip thinking, stating that
/// refusal once rather than leaving the user waiting for a summary.
pub(crate) fn recap_settings(
    config: RecapConfig,
    section: &ModelSection,
    session_model: &str,
) -> (RecapSettings, Vec<ConfigWarning>) {
    let settings = RecapSettings {
        enabled: config.enabled,
        model: None,
        idle_trigger_secs: config.idle_trigger_secs,
    };
    if !config.enabled {
        return (settings, Vec::new());
    }
    let mut warnings = Vec::new();
    let model = match config.model {
        Some(model) => {
            if section.catalog.iter().any(|entry| entry.id == model) {
                Some(model)
            } else {
                warnings.push(ConfigWarning {
                    field: "recap.model".into(),
                    reason: format!("{model} is not in the model catalog — the recap stays off"),
                });
                None
            }
        }
        None => fallback_recap_model(section, session_model),
    };
    let model = match model {
        Some(model) if model_window::effort_dialect(&model).can_disable_thinking() => Some(model),
        Some(model) => {
            warnings.push(ConfigWarning {
                field: "recap.model".into(),
                reason: format!("{model} cannot run without thinking — the recap stays off"),
            });
            None
        }
        None => {
            if warnings.is_empty() {
                warnings.push(ConfigWarning {
                    field: "recap.model".into(),
                    reason:
                        "no model in the catalog can run without thinking — the recap stays off"
                            .into(),
                });
            }
            None
        }
    };
    (RecapSettings { model, ..settings }, warnings)
}

/// The model a recap runs on when the settings file names none: the session's
/// own model when its dialect can run without thinking, otherwise the first
/// catalog entry that can, preferring a model declared able to run in Fast
/// mode. None when the catalog offers no such model.
fn fallback_recap_model(section: &ModelSection, session_model: &str) -> Option<String> {
    if model_window::effort_dialect(session_model).can_disable_thinking() {
        return Some(session_model.to_string());
    }
    let usable =
        |entry: &ModelEntry| model_window::effort_dialect(&entry.id).can_disable_thinking();
    section
        .catalog
        .iter()
        .find(|entry| entry.fast == Some(true) && usable(entry))
        .or_else(|| section.catalog.iter().find(|entry| usable(entry)))
        .map(|entry| entry.id.clone())
}

#[cfg(test)]
mod tests;
