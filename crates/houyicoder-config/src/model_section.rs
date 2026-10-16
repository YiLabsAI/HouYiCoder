//! The model section of settings.json: the active model id, a global effort
//! fallback, and the catalog the /model panel lists. Loaded per-field-recoverable
//! via ConfigWarning so one malformed entry does not reset the section.

use houyicoder_protocol::cache_policy::PromptCacheFormat;
use houyicoder_protocol::frontend::model::SpeedMode;
use houyicoder_protocol::llm::EffortLevel;

use crate::ConfigWarning;
use crate::json_type_name;
use crate::served_models::cached_ids;
use crate::served_models::exists_in;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::collections::HashSet;
#[cfg(test)]
use std::env::temp_dir;
use std::fs::read_to_string;
#[cfg(test)]
use std::fs::remove_file;
#[cfg(test)]
use std::fs::write;
use std::path::Path;
#[cfg(test)]
use std::process::id;

/// The shipped default catalog shown in the /model pane when settings.json has
/// no catalog. A provider-agnostic client cannot auto-discover which models the
/// endpoint serves, so a default list for the common DashScope endpoint ships
/// built-in. The user overrides by adding model.catalog to settings.json. Each
/// entry is (id, display_name) — the id is the real model id sent to the
/// provider; the display_name is the tier-like label shown in the pane.
pub const DEFAULT_CATALOG: &[(&str, &str)] = &[
    ("qwen3.7-max", "Max"),
    ("glm-5.2", "Fable"),
    ("qwen3.8-max", "Pro"),
    ("qwen3.6-flash", "Flash"),
];

/// One catalog entry: a model id plus the per-model parameters the catalog
/// overrides. effort is the persisted per-model pick (None = follow the
/// resolution chain). context_window / max_output_tokens override the
/// family-default table; both are optional because the family default is the
/// common case. fast declares whether this model can run in Fast mode; the
/// catalog is the only source (no name probe grants a tier), so None reads
/// as unavailable and false is how a user revokes a declared tier.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ModelEntry {
    pub id: String,
    /// Optional display label for the /model pane; falls back to the id when
    /// unset so a minimal catalog entry still renders a readable row.
    #[serde(default)]
    pub display_name: Option<String>,
    /// Optional one-line description for the /model pane row.
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub effort: Option<EffortLevel>,
    /// The effort levels the picker cycles for this model; None keeps the
    /// dialect's full set. A listed level the dialect does not serve is
    /// dropped at resolution, so the list cannot grant it.
    #[serde(default)]
    pub effort_levels: Option<Vec<EffortLevel>>,
    #[serde(default)]
    pub context_window: Option<u32>,
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    #[serde(default)]
    pub fast: Option<bool>,
    /// How the endpoint serving this model wants a cache position expressed.
    /// None declares nothing and leaves the request's default in place.
    #[serde(default)]
    pub cache_format: Option<PromptCacheFormat>,
}

/// The model section of settings.json: the active id (None = Default
/// sentinel, resolved to the constant), a global effort fallback for
/// catalog entries without one, the global speed preference, and the
/// catalog the /model panel lists.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ModelSection {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub effort_level: Option<EffortLevel>,
    #[serde(default)]
    pub speed_mode: Option<SpeedMode>,
    #[serde(default)]
    pub catalog: Vec<ModelEntry>,
}

/// A default ModelSection with the shipped DEFAULT_CATALOG populated, so the
/// /model pane is usable even when settings.json is missing, has no model
/// section, or has a null/empty catalog. Every early-return path that would
/// produce an empty catalog uses this instead of ModelSection::default().
fn default_section_with_catalog() -> ModelSection {
    ModelSection {
        catalog: DEFAULT_CATALOG
            .iter()
            .map(|(id, name)| ModelEntry {
                id: (*id).to_string(),
                display_name: Some((*name).to_string()),
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    }
}

/// Load the model section from an explicit path. A missing file or a missing
/// model section yields defaults with no warnings. Each field is recovered
/// independently so one malformed value (a typo'd effort level, a non-numeric
/// context window) degrades only that field; the rest of the section —
/// especially the catalog — stays usable. Per-field recovery, like the
/// toggles loader already uses: a single bad value must not silently reset
/// every other setting.
pub fn load_model_section_from(path: &Path) -> (ModelSection, Vec<ConfigWarning>) {
    let text = match read_to_string(path) {
        Ok(t) => t,
        Err(_) => return (default_section_with_catalog(), Vec::new()),
    };
    let value: Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(_) => {
            return (
                default_section_with_catalog(),
                vec![ConfigWarning {
                    field: "<file>".into(),
                    reason: "settings.json is not valid JSON — using defaults".into(),
                }],
            );
        }
    };
    let model_value = match value.get("model") {
        None | Some(Value::Null) => {
            return (default_section_with_catalog(), Vec::new());
        }
        Some(v) if !v.is_object() => {
            return (
                default_section_with_catalog(),
                vec![ConfigWarning {
                    field: "model".into(),
                    reason: format!(
                        "expected an object, got {} — using defaults",
                        json_type_name(v)
                    ),
                }],
            );
        }
        Some(v) => v,
    };
    let mut warnings = Vec::new();
    let id = extract_field::<String>(model_value, "id", "model.id", &mut warnings);
    let effort_level = extract_field::<EffortLevel>(
        model_value,
        "effort_level",
        "model.effort_level",
        &mut warnings,
    );
    let catalog = extract_catalog(model_value, &mut warnings);
    let speed_mode =
        extract_field::<SpeedMode>(model_value, "speed_mode", "model.speed_mode", &mut warnings);
    let mut section = ModelSection {
        id,
        effort_level,
        speed_mode,
        catalog,
    };
    // Fallback to the shipped default catalog when the user has not
    // configured one — the /model pane is usable out-of-the-box. A null
    // catalog, an empty array, or a missing catalog field all fall back.
    // A user-configured catalog (even one entry) replaces the default.
    if section.catalog.is_empty() {
        section.catalog = DEFAULT_CATALOG
            .iter()
            .map(|(id, name)| ModelEntry {
                id: (*id).to_string(),
                display_name: Some((*name).to_string()),
                ..Default::default()
            })
            .collect();
    }
    warnings.extend(validate_catalog(&mut section, &cached_ids()));
    (section, warnings)
}

/// Pull a field out of a parsed value, deserializing it as T. Missing or
/// null yields None (the field's default); a type error yields None plus a
/// warning naming the field, so the caller keeps loading the sibling fields.
/// Per-field recovery is the point: one bad value must not reset the section.
fn extract_field<T: DeserializeOwned>(
    parent: &Value,
    key: &str,
    label: &str,
    warnings: &mut Vec<ConfigWarning>,
) -> Option<T> {
    match parent.get(key) {
        None | Some(Value::Null) => None,
        Some(v) => match serde_json::from_value::<T>(v.clone()) {
            Ok(t) => Some(t),
            Err(e) => {
                warnings.push(ConfigWarning {
                    field: label.to_string(),
                    reason: format!("{} malformed ({}) — using the default", label, e),
                });
                None
            }
        },
    }
}

/// Load the catalog with per-entry, per-field recovery. A malformed catalog
/// (not an array) yields an empty catalog plus a warning. Each entry is
/// parsed field by field, so a bad effort on one entry nulls that entry's
/// effort without dropping the entry or the rest of the catalog.
fn extract_catalog(parent: &Value, warnings: &mut Vec<ConfigWarning>) -> Vec<ModelEntry> {
    match parent.get("catalog") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(arr)) => arr
            .iter()
            .enumerate()
            .filter_map(|(i, entry)| parse_catalog_entry(entry, i, warnings))
            .collect(),
        Some(other) => {
            warnings.push(ConfigWarning {
                field: "model.catalog".into(),
                reason: format!(
                    "expected an array, got {} — using the default",
                    json_type_name(other)
                ),
            });
            Vec::new()
        }
    }
}

/// Parse one catalog entry field by field. A non-object entry is dropped with
/// a warning. The id is read directly so a non-string id reports honestly
/// rather than reading as blank; a blank or missing id falls through to
/// validate_catalog, which drops it with its own warning.
fn parse_catalog_entry(
    value: &Value,
    index: usize,
    warnings: &mut Vec<ConfigWarning>,
) -> Option<ModelEntry> {
    if !value.is_object() {
        warnings.push(ConfigWarning {
            field: format!("model.catalog[{index}]"),
            reason: format!(
                "expected an object, got {} — entry dropped",
                json_type_name(value)
            ),
        });
        return None;
    }
    let id = match value.get("id") {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => {
            warnings.push(ConfigWarning {
                field: format!("model.catalog[{index}].id"),
                reason: format!(
                    "expected a string, got {} — entry dropped",
                    json_type_name(other)
                ),
            });
            String::new()
        }
    };
    let label = |field: &str| format!("model.catalog[{index}].{field}");
    let display_name =
        extract_field::<String>(value, "display_name", &label("display_name"), warnings);
    let description =
        extract_field::<String>(value, "description", &label("description"), warnings);
    let effort = extract_field::<EffortLevel>(value, "effort", &label("effort"), warnings);
    let context_window =
        extract_field::<u32>(value, "context_window", &label("context_window"), warnings);
    let max_output_tokens = extract_field::<u32>(
        value,
        "max_output_tokens",
        &label("max_output_tokens"),
        warnings,
    );
    let fast = extract_field::<bool>(value, "fast", &label("fast"), warnings);
    let cache_format =
        extract_field::<PromptCacheFormat>(value, "cache_format", &label("cache_format"), warnings);
    let effort_levels = extract_field::<Vec<EffortLevel>>(
        value,
        "effort_levels",
        &label("effort_levels"),
        warnings,
    );
    Some(ModelEntry {
        id,
        display_name,
        description,
        effort,
        effort_levels,
        context_window,
        max_output_tokens,
        fast,
        cache_format,
    })
}

/// Validate the catalog in place: drop blank-id entries and duplicate-id
/// entries (keeping the first of each id), then warn when the active model id
/// is not among the surviving catalog entries. All three are zero-cost local
/// checks that catch real typos without a network probe. The same ConfigWarning
/// channel as per-field settings recovery is reused so one malformed entry does
/// not silently become a no-op. Like the HookSpec precedent: a config typo
/// must not silently register a no-op entry.
fn validate_catalog(section: &mut ModelSection, served: &[String]) -> Vec<ConfigWarning> {
    let mut warnings = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut kept: Vec<ModelEntry> = Vec::with_capacity(section.catalog.len());
    for entry in section.catalog.drain(..) {
        if entry.id.trim().is_empty() {
            warnings.push(ConfigWarning {
                field: "model.catalog".into(),
                reason: "catalog entry with blank id dropped".into(),
            });
            continue;
        }
        if !seen.insert(entry.id.clone()) {
            warnings.push(ConfigWarning {
                field: "model.catalog".into(),
                reason: format!(
                    "duplicate catalog id {} dropped — keeping the first occurrence",
                    entry.id
                ),
            });
            continue;
        }
        kept.push(entry);
    }
    section.catalog = kept;

    // Active id check: only when the user picked a specific id. None or blank
    // means the Default sentinel (resolved to the constant elsewhere) so there
    // is no catalog entry to match against and nothing to warn about.
    if let Some(id) = section.id.as_ref() {
        let trimmed = id.trim();
        if !trimmed.is_empty() && !section.catalog.iter().any(|e| e.id.trim() == trimmed) {
            warnings.push(ConfigWarning {
                field: "model.id".into(),
                reason: format!(
                    "active model id {} is not in the catalog — the pick may be a typo",
                    trimmed
                ),
            });
        }
    }

    // Served-models existence check: when a provider served-id cache exists
    // (written by the startup /v1/models fetch), warn on catalog entries the
    // provider does not actually serve — a stale id or a typo that no name
    // pattern would catch. Fault-tolerant:
    // no cache (fetch failed, stub mode, never ran) => skip entirely, because
    // an empty cache means "cannot know", not "nothing is served". Never
    // blocks loading or drops the entry; the next fetch may add it back.
    if !served.is_empty() {
        for entry in &section.catalog {
            if !exists_in(served, &entry.id) {
                warnings.push(ConfigWarning {
                    field: "model.catalog".into(),
                    reason: format!(
                        "{} is not in the provider's served-model list — it may be a typo or the provider no longer offers it",
                        entry.id
                    ),
                });
            }
        }
    }
    warnings
}

#[cfg(test)]
mod tests;
