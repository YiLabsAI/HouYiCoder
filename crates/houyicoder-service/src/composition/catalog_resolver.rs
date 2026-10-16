//! The settings-backed catalog resolver and the /model pick persistence:
//! loads the model section from settings.json once at runner construction
//! and answers the catalog-side layers for the agent loop's resolution
//! chains. First-match on duplicate ids matches the read path's keep-first
//! dedup.

use houyicoder_config::{
    ModelSection, SettingsWriteError, load_model_section_from, update_settings,
};
use houyicoder_core::agent::{ModelCatalogResolver, effort_default_for};
use houyicoder_protocol::cache_policy::PromptCacheFormat;
use houyicoder_protocol::frontend::model::SpeedMode;
use houyicoder_protocol::llm::EffortLevel;
use serde_json::json;
use std::path::Path;

/// The effort level to persist on Enter: the effective pick, or nothing
/// when the pick equals the model's fallback default — so a deliberate
/// return to the default clears the persisted value and the chain leads
/// again instead of pinning the default value.
///
/// - picked: the level the picker will send this turn (None = auto).
/// - model_default: the model's fallback default — the global effort_level,
///   then the built-in per-model default. Equal to this, the pick is not
///   worth pinning.
pub fn effort_to_persist(
    picked: Option<EffortLevel>,
    model_default: Option<EffortLevel>,
) -> Option<EffortLevel> {
    if picked == model_default {
        None
    } else {
        picked
    }
}

pub(crate) struct SettingsCatalogResolver {
    section: ModelSection,
}

/// Persist the /model Enter pick to settings.json: the model id (a Default
/// sentinel deletes the model.id key so the pick follows the resolution
/// chain; a concrete id writes the id), the per-model effort on the catalog's
/// first-matching entry (the read path wins first match, so the write path
/// targets the same entry — and a pick whose target has no entry yet, such
/// as the shipped fallback catalog's rows, materializes one so the effort is
/// not silently dropped), and the speed tier when the stored value would
/// resolve differently. The effort to persist is decided by
/// effort_to_persist. The error is returned rather than dropped: the caller
/// reports a partial result, so a session that switched but did not save
/// says so.
pub fn persist_model_pick(
    path: &Path,
    model_input: Option<&str>,
    picked_effort: Option<EffortLevel>,
    toggled: bool,
    speed: SpeedMode,
) -> Result<(), SettingsWriteError> {
    let (section, _) = load_model_section_from(path);
    let resolved = model_input
        .map(str::to_string)
        .unwrap_or_else(houyicoder_config::resolve_default_model);
    let prior = section
        .catalog
        .iter()
        .find(|e| e.id == resolved)
        .and_then(|e| e.effort);
    // The fallback default is the global effort_level, then the built-in
    // per-model default — the same chain the resolver leads with when no
    // per-model effort is pinned. A pick equal to this is not worth pinning;
    // clearing it lets a later default change lead the session.
    let model_default = section
        .effort_level
        .or_else(|| effort_default_for(&resolved));
    let target = effort_to_persist(picked_effort, model_default);
    // Touch the effort key only when the user adjusted effort this session
    // or the target differs from what is on disk — an untouched Enter with
    // the right value already pinned does not churn the file.
    let needs_effort_write = toggled || prior != target;
    // The stored tier only carries meaning when it is Fast (absent reads as
    // the Standard default): write when the pick is Fast, or when clearing a
    // previously stored Fast. A Standard pick on an unset section pins
    // nothing the user never chose.
    let write_speed = speed == SpeedMode::Fast || section.speed_mode == Some(SpeedMode::Fast);
    update_settings(
        path,
        |v| {
            match model_input {
                Some(id) => {
                    v["model"]["id"] = json!(id);
                }
                None => {
                    if let Some(m) = v.get_mut("model").and_then(|m| m.as_object_mut()) {
                        m.remove("id");
                    }
                }
            }
            if needs_effort_write {
                let model = &mut v["model"];
                if !model.is_object() {
                    *model = json!({});
                }
                if let Some(model_obj) = model.as_object_mut() {
                    // Ensure a catalog array exists; a pick whose target has
                    // no row yet (the shipped fallback catalog) materializes
                    // one so the effort is not silently dropped.
                    if !model_obj.get("catalog").is_some_and(|c| c.is_array()) {
                        model_obj.insert("catalog".to_string(), json!([]));
                    }
                    if let Some(arr) = model_obj.get_mut("catalog").and_then(|c| c.as_array_mut()) {
                        let entry = arr
                            .iter_mut()
                            .find(|e| e.get("id").and_then(|x| x.as_str()) == Some(&resolved));
                        match (entry, target) {
                            (Some(entry), Some(level)) => entry["effort"] = json!(level),
                            (Some(entry), None) => {
                                if let Some(o) = entry.as_object_mut() {
                                    o.remove("effort");
                                }
                            }
                            (None, Some(level)) => {
                                arr.push(json!({"id": resolved, "effort": level}));
                            }
                            (None, None) => {}
                        }
                    }
                }
            }
            if write_speed {
                v["model"]["speed_mode"] = json!(speed);
            }
        },
        3,
    )
}

impl SettingsCatalogResolver {
    /// A resolver over an already-loaded section, so a caller holding the
    /// model section (or a test holding a temp settings file) answers from
    /// the same rows the server read rather than re-reading the global path.
    pub(crate) fn from_section(section: ModelSection) -> Self {
        Self { section }
    }
}

impl ModelCatalogResolver for SettingsCatalogResolver {
    fn catalog_effort(&self, model: &str) -> Option<EffortLevel> {
        self.section
            .catalog
            .iter()
            .find(|e| e.id == model)
            .and_then(|e| e.effort)
            .or(self.section.effort_level)
    }

    fn catalog_effort_levels(&self, model: &str) -> Option<Vec<EffortLevel>> {
        self.section
            .catalog
            .iter()
            .find(|e| e.id == model)
            .and_then(|e| e.effort_levels.clone())
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

    fn catalog_fast(&self, model: &str) -> Option<bool> {
        self.section
            .catalog
            .iter()
            .find(|e| e.id == model)
            .and_then(|e| e.fast)
    }

    fn catalog_cache_format(&self, model: &str) -> Option<PromptCacheFormat> {
        self.section
            .catalog
            .iter()
            .find(|e| e.id == model)
            .and_then(|e| e.cache_format)
    }
}

#[cfg(test)]
mod tests;
