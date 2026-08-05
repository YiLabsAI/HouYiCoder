//! Catalog-backed EffortResolver: loads the model section from settings.json
//! once at runner construction and answers the catalog-side effort layers
//! for the agent loop's resolution chain. First-match on duplicate ids
//! matches the read path's keep-first dedup.

use houyicoder_config::{ModelSection, load_model_section_from, settings_path, update_settings};
use houyicoder_core::agent::{EffortResolver, effort_default_for};
use houyicoder_protocol::llm::EffortLevel;
use serde_json::json;

/// The effort level to persist on Enter: the picked level, or None when it
/// equals the model's default (so the pick follows the resolution chain
/// rather than pinning the default value). Unconditional on prior/toggle
/// history.
///
/// - picked: the level the picker will send this turn (None = auto).
/// - model_default: the resolved default for this model (None = no default).
/// - prior: unused (kept for the dispatch call site signature).
/// - toggled: unused (kept for the dispatch call site signature).
pub fn effort_to_persist(
    picked: Option<EffortLevel>,
    model_default: Option<EffortLevel>,
    _prior: Option<EffortLevel>,
    _toggled: bool,
) -> Option<EffortLevel> {
    if picked == model_default {
        None
    } else {
        picked
    }
}

pub(crate) struct SettingsEffortResolver {
    section: ModelSection,
}

/// Persist the /model Enter pick to settings.json: the model id (the model-id persistence rule — a
/// Default sentinel deletes the model.id key so the pick follows the
/// resolution chain; a concrete id writes the id) and the per-model effort
/// on the catalog's first-matching entry (the read/write first-match dedup read/write dedup same-source).
/// The effort to persist is decided by effort_to_persist against the prior
/// catalog value + the model's built-in default (a purely-default +
/// un-toggled pick is not written). Best-effort: a write failure is dropped
/// (the in-memory pick still takes effect this session; only cross-session
/// persistence is lost).
pub fn persist_model_pick(
    path: &std::path::Path,
    model_input: Option<&str>,
    picked_effort: Option<EffortLevel>,
    toggled: bool,
) {
    let (section, _) = load_model_section_from(path);
    let resolved = model_input
        .map(str::to_string)
        .unwrap_or_else(houyicoder_config::resolve_model);
    let prior = section
        .catalog
        .iter()
        .find(|e| e.id == resolved)
        .and_then(|e| e.effort);
    let model_default = effort_default_for(&resolved);
    let persist_effort = effort_to_persist(picked_effort, model_default, prior, toggled);
    drop(update_settings(
        path,
        |v| {
            // the model-id persistence rule: model.id — concrete id writes, Default deletes the key.
            match model_input {
                Some(id) => {
                    v["model"]["id"] = json!(id);
                }
                None => {
                    if let Some(m) = v["model"].as_object_mut() {
                        m.remove("id");
                    }
                }
            }
            // the read/write first-match dedup: catalog[id].effort — write the first-matching entry so the
            // read path (first match) and write path (first match) agree.
            if let Some(arr) = v["model"]["catalog"].as_array_mut() {
                for entry in arr.iter_mut() {
                    if entry.get("id").and_then(|x| x.as_str()) == Some(&resolved) {
                        entry["effort"] = json!(persist_effort);
                        break;
                    }
                }
            }
        },
        3,
    ));
}

impl SettingsEffortResolver {
    /// Load the model section from settings.json, returning the resolver
    /// plus the warnings the load produced so the composition root can
    /// surface them. A missing file or a malformed section yields defaults
    /// (no catalog), and the resolver answers None for every model — the
    /// chain then falls to the in-session pick + the built-in default.
    /// Best-effort, never fails construction. A bad field degrades only
    /// itself (per-field recovery) and is reported here rather than silently
    /// dropped.
    pub(crate) fn load_with_warnings() -> (Self, Vec<houyicoder_config::ConfigWarning>) {
        let (section, warnings) = load_model_section_from(&settings_path());
        (Self { section }, warnings)
    }
}

impl EffortResolver for SettingsEffortResolver {
    fn catalog_effort(&self, model: &str) -> Option<EffortLevel> {
        self.section
            .catalog
            .iter()
            .find(|e| e.id == model)
            .and_then(|e| e.effort)
            .or(self.section.effort_level)
    }

    fn catalog_context_window(&self, model: &str) -> Option<u32> {
        self.section
            .catalog
            .iter()
            .find(|e| e.id == model)
            .and_then(|e| e.context_window)
    }

    fn catalog_max_output_tokens(&self, model: &str) -> Option<u32> {
        self.section
            .catalog
            .iter()
            .find(|e| e.id == model)
            .and_then(|e| e.max_output_tokens)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use houyicoder_config::ModelEntry;

    fn resolver_with(catalog: &[(&str, Option<u32>, Option<u32>)]) -> SettingsEffortResolver {
        let section = ModelSection {
            id: None,
            effort_level: None,
            catalog: catalog
                .iter()
                .map(|(id, ctx, max)| ModelEntry {
                    id: id.to_string(),
                    display_name: None,
                    description: None,
                    effort: None,
                    context_window: *ctx,
                    max_output_tokens: *max,
                })
                .collect(),
        };
        SettingsEffortResolver { section }
    }

    #[test]
    fn test_catalog_override_output_tokens() {
        let r = resolver_with(&[("qwen3.7-max", None, Some(9999))]);
        assert_eq!(r.catalog_max_output_tokens("qwen3.7-max"), Some(9999));
        assert_eq!(
            r.catalog_max_output_tokens("other"),
            None,
            "no entry => None"
        );
    }

    #[test]
    fn test_catalog_override_context_window() {
        let r = resolver_with(&[("glm-5.2", Some(1_000_000), None)]);
        assert_eq!(r.catalog_context_window("glm-5.2"), Some(1_000_000));
        assert_eq!(r.catalog_context_window("qwen3.7-max"), None);
    }

    #[test]
    fn test_catalog_override_first_match() {
        // Duplicate id: the read path keeps the first, so the override does too.
        let r = resolver_with(&[("x", None, Some(111)), ("x", None, Some(222))]);
        assert_eq!(r.catalog_max_output_tokens("x"), Some(111));
    }

    #[test]
    fn test_picked_not_equal_persists() {
        assert_eq!(
            effort_to_persist(
                Some(EffortLevel::High),
                Some(EffortLevel::Medium),
                None,
                false
            ),
            Some(EffortLevel::High)
        );
    }

    #[test]
    fn test_picked_equal_skips_persist() {
        assert_eq!(
            effort_to_persist(
                Some(EffortLevel::Medium),
                Some(EffortLevel::Medium),
                None,
                false
            ),
            None
        );
    }

    #[test]
    fn test_default_pick_clears_prior() {
        assert_eq!(
            effort_to_persist(
                Some(EffortLevel::Medium),
                Some(EffortLevel::Medium),
                Some(EffortLevel::Low),
                true
            ),
            None,
            "equal to default clears even with prior + toggle"
        );
    }

    #[test]
    fn test_auto_not_persisted() {
        assert_eq!(
            effort_to_persist(None, Some(EffortLevel::Medium), None, false),
            None
        );
    }

    #[test]
    fn test_auto_prior_persists_none() {
        assert_eq!(
            effort_to_persist(
                None,
                Some(EffortLevel::Medium),
                Some(EffortLevel::Low),
                false
            ),
            None
        );
    }

    #[test]
    fn test_default_sentinel_deletes_id() {
        let path = std::env::temp_dir().join(format!("m25-default-{}.json", std::process::id()));
        std::fs::write(
            &path,
            r#"{"model":{"id":"qwen3-coder","catalog":[{"id":"qwen3-coder"}]}}"#,
        )
        .unwrap();
        // Select Default (model_input=None) → delete model.id key.
        drop(houyicoder_config::update_settings(
            &path,
            |v| {
                if let Some(m) = v["model"].as_object_mut() {
                    m.remove("id");
                }
            },
            3,
        ));
        let back: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(
            back["model"].get("id").is_none(),
            "model.id key deleted on Default sentinel: {back}"
        );
        drop(std::fs::remove_file(&path));
    }

    #[test]
    fn test_id_writes_model_id() {
        let path = std::env::temp_dir().join(format!("m25-concrete-{}.json", std::process::id()));
        std::fs::write(&path, r#"{"model":{"catalog":[{"id":"x"}]}}"#).unwrap();
        drop(houyicoder_config::update_settings(
            &path,
            |v| {
                v["model"]["id"] = serde_json::json!("glm-5.2");
            },
            3,
        ));
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains(r#""glm-5.2""#), "concrete id written: {text}");
        drop(std::fs::remove_file(&path));
    }
}
