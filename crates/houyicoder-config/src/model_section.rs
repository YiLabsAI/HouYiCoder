//! The model section of settings.json: the active model id, a global effort
//! fallback, and the catalog the /model panel lists. Loaded per-field-recoverable
//! via ConfigWarning so one malformed entry does not reset the section.

use houyicoder_protocol::llm::EffortLevel;

use crate::ConfigWarning;
use crate::json_type_name;

/// The shipped default catalog shown in the /model pane when settings.json has
/// no catalog. A provider-agnostic client cannot auto-discover which models the
/// endpoint serves, so a default list for the common DashScope endpoint ships
/// built-in. The user overrides by adding model.catalog to settings.json. Each
/// entry is (id, display_name) — the id is the real model id sent to the
/// provider; the display_name is the tier-like label shown in the pane.
pub const DEFAULT_CATALOG: &[(&str, &str)] = &[
    ("qwen3.7-max", "Max"),
    ("glm-5.2", "Fable"),
    ("glm-5.1", "Pro"),
];

/// One catalog entry: a model id plus the per-model parameters the catalog
/// overrides. effort is the persisted per-model pick (None = follow the
/// resolution chain). context_window / max_output_tokens override the
/// family-default table; both are optional because the family default is the
/// common case.
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
    #[serde(default)]
    pub context_window: Option<u32>,
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
}

/// The model section of settings.json: the active id (None = Default
/// sentinel, resolved to the constant), a global effort fallback for
/// catalog entries without one, and the catalog the /model panel lists.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ModelSection {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub effort_level: Option<EffortLevel>,
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
pub fn load_model_section_from(path: &std::path::Path) -> (ModelSection, Vec<ConfigWarning>) {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(_) => return (default_section_with_catalog(), Vec::new()),
    };
    let value: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(_) => {
            return (
                default_section_with_catalog(),
                vec![ConfigWarning {
                    field: "<file>".into(),
                    reason: "settings.json is not valid JSON; using defaults".into(),
                }],
            );
        }
    };
    let model_value = match value.get("model") {
        None | Some(serde_json::Value::Null) => {
            return (default_section_with_catalog(), Vec::new());
        }
        Some(v) if !v.is_object() => {
            return (
                default_section_with_catalog(),
                vec![ConfigWarning {
                    field: "model".into(),
                    reason: format!(
                        "expected an object, got {}; using defaults",
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
    let mut section = ModelSection {
        id,
        effort_level,
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
    warnings.extend(validate_catalog(
        &mut section,
        &crate::served_models::cached_ids(),
    ));
    (section, warnings)
}

/// Pull a field out of a parsed value, deserializing it as T. Missing or
/// null yields None (the field's default); a type error yields None plus a
/// warning naming the field, so the caller keeps loading the sibling fields.
/// Per-field recovery is the point: one bad value must not reset the section.
fn extract_field<T: serde::de::DeserializeOwned>(
    parent: &serde_json::Value,
    key: &str,
    label: &str,
    warnings: &mut Vec<ConfigWarning>,
) -> Option<T> {
    match parent.get(key) {
        None | Some(serde_json::Value::Null) => None,
        Some(v) => match serde_json::from_value::<T>(v.clone()) {
            Ok(t) => Some(t),
            Err(e) => {
                warnings.push(ConfigWarning {
                    field: label.to_string(),
                    reason: format!("{} malformed ({}); using the default", label, e),
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
fn extract_catalog(
    parent: &serde_json::Value,
    warnings: &mut Vec<ConfigWarning>,
) -> Vec<ModelEntry> {
    match parent.get("catalog") {
        None | Some(serde_json::Value::Null) => Vec::new(),
        Some(serde_json::Value::Array(arr)) => arr
            .iter()
            .enumerate()
            .filter_map(|(i, entry)| parse_catalog_entry(entry, i, warnings))
            .collect(),
        Some(other) => {
            warnings.push(ConfigWarning {
                field: "model.catalog".into(),
                reason: format!(
                    "expected an array, got {}; using the default",
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
    value: &serde_json::Value,
    index: usize,
    warnings: &mut Vec<ConfigWarning>,
) -> Option<ModelEntry> {
    if !value.is_object() {
        warnings.push(ConfigWarning {
            field: format!("model.catalog[{index}]"),
            reason: format!(
                "expected an object, got {}; entry dropped",
                json_type_name(value)
            ),
        });
        return None;
    }
    let id = match value.get("id") {
        None | Some(serde_json::Value::Null) => String::new(),
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(other) => {
            warnings.push(ConfigWarning {
                field: format!("model.catalog[{index}].id"),
                reason: format!(
                    "expected a string, got {}; entry dropped",
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
    Some(ModelEntry {
        id,
        display_name,
        description,
        effort,
        context_window,
        max_output_tokens,
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
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
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
                    "duplicate catalog id {} dropped; keeping the first occurrence",
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
                    "active model id {} is not in the catalog; the pick may be a typo",
                    trimmed
                ),
            });
        }
    }

    // Served-models existence check: when a provider served-id cache exists
    // (written by the startup /v1/models fetch), warn on catalog entries the
    // provider does not actually serve — a stale id or a typo that the
    // substring name-match (supports_effort) cannot catch. Fault-tolerant:
    // no cache (fetch failed, stub mode, never ran) => skip entirely, because
    // an empty cache means "cannot know", not "nothing is served". Never
    // blocks loading or drops the entry; the next fetch may add it back.
    if !served.is_empty() {
        for entry in &section.catalog {
            if !crate::served_models::exists_in(served, &entry.id) {
                warnings.push(ConfigWarning {
                    field: "model.catalog".into(),
                    reason: format!(
                        "{} is not in the provider's served-model list; it may be a typo or the provider no longer offers it",
                        entry.id
                    ),
                });
            }
        }
    }
    warnings
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str) -> ModelEntry {
        ModelEntry {
            id: id.into(),
            display_name: None,
            description: None,
            effort: None,
            context_window: None,
            max_output_tokens: None,
        }
    }

    fn section(id: Option<&str>, catalog: &[ModelEntry]) -> ModelSection {
        ModelSection {
            id: id.map(str::to_string),
            effort_level: None,
            catalog: catalog.to_vec(),
        }
    }

    #[test]
    fn test_catalog_drops_duplicate_id() {
        let mut s = section(None, &[entry("qwen3.7-max"), entry("qwen3.7-max")]);
        let w = validate_catalog(&mut s, &[]);
        assert_eq!(s.catalog.len(), 1, "first occurrence kept, rest dropped");
        assert_eq!(s.catalog[0].id, "qwen3.7-max");
        assert_eq!(w.len(), 1, "one warning per dropped duplicate");
        assert!(w[0].reason.contains("qwen3.7-max"));
    }

    #[test]
    fn test_catalog_drops_blank_id() {
        let mut s = section(None, &[entry(""), entry("  "), entry("qwen3.7-max")]);
        let w = validate_catalog(&mut s, &[]);
        assert_eq!(s.catalog.len(), 1, "blank entries dropped, real one kept");
        assert_eq!(s.catalog[0].id, "qwen3.7-max");
        assert_eq!(w.len(), 2, "one warning per blank entry");
        assert!(w.iter().all(|x| x.field == "model.catalog"));
    }

    #[test]
    fn test_active_model_missing_warns() {
        let mut s = section(Some("qwen3.8-max"), &[entry("qwen3.7-max")]);
        let w = validate_catalog(&mut s, &[]);
        assert_eq!(
            w.len(),
            1,
            "active id not in catalog yields exactly one warning"
        );
        assert_eq!(w[0].field, "model.id");
        assert!(w[0].reason.contains("qwen3.8-max"));
        assert_eq!(s.catalog.len(), 1);
    }

    #[test]
    fn test_active_id_present() {
        let mut s = section(Some("qwen3.7-max"), &[entry("qwen3.7-max")]);
        let w = validate_catalog(&mut s, &[]);
        assert!(w.is_empty(), "active id present => no warning");
    }

    #[test]
    fn test_active_id_absent() {
        // None = Default sentinel; nothing to match against, no warning.
        let mut s = section(None, &[entry("qwen3.7-max")]);
        let w = validate_catalog(&mut s, &[]);
        assert!(w.is_empty());
    }

    #[test]
    fn test_duplicate_and_blank_stack() {
        // Two same-id + one blank: first kept, two dropped, two warnings.
        let mut s = section(
            None,
            &[entry("qwen3.7-max"), entry("qwen3.7-max"), entry("")],
        );
        let w = validate_catalog(&mut s, &[]);
        assert_eq!(s.catalog.len(), 1);
        assert_eq!(w.len(), 2);
    }

    #[test]
    fn test_catalog_preserves_order() {
        // A settings.json with several catalog entries round-trips through
        // disk preserving insertion order (the catalog is a Vec, not a map),
        // so the /model panel lists entries in the author's intended order.
        let path = std::env::temp_dir().join(format!("catalog-order-{}.json", std::process::id()));
        std::fs::write(
            &path,
            r#"{"model":{"id":"a","catalog":[{"id":"a"},{"id":"b"},{"id":"c"}]}}"#,
        )
        .unwrap();
        let (s, w) = load_model_section_from(&path);
        assert!(w.is_empty(), "no warnings on a valid catalog");
        assert_eq!(
            s.catalog.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(),
            ["a", "b", "c"],
            "insertion order preserved through a write->read cycle"
        );
        drop(std::fs::remove_file(&path));
    }

    #[test]
    fn test_bad_effort_keeps_catalog() {
        // A typo'd effort_level must degrade only that field; the catalog
        // stays usable and effort falls to the next layer. The whole-section
        // reset (catalog lost) is the failure mode this guards against.
        let path = std::env::temp_dir().join(format!("m19-badeff-{}.json", std::process::id()));
        std::fs::write(
            &path,
            r#"{"model":{"effort_level":"insane","catalog":[{"id":"qwen3.7-max"}]}}"#,
        )
        .unwrap();
        let (s, w) = load_model_section_from(&path);
        assert_eq!(
            s.effort_level, None,
            "bad effort_level falls back to None, not the whole section"
        );
        assert_eq!(s.catalog.len(), 1, "catalog survives a bad sibling field");
        assert_eq!(s.catalog[0].id, "qwen3.7-max");
        let bad = w.iter().find(|x| x.field == "model.effort_level");
        assert!(bad.is_some(), "a warning names the bad field: {w:?}");
        drop(std::fs::remove_file(&path));
    }

    #[test]
    fn test_bad_catalog_entry_salvaged() {
        // A bad effort on one catalog entry nulls that entry's effort; the
        // entry itself and the rest of the catalog survive. Same failure
        // class as the top-level case, one layer down.
        let path = std::env::temp_dir().join(format!("m19-entryeff-{}.json", std::process::id()));
        std::fs::write(
            &path,
            r#"{"model":{"catalog":[{"id":"a","effort":"insane"},{"id":"b","effort":"low"}]}}"#,
        )
        .unwrap();
        let (s, w) = load_model_section_from(&path);
        assert_eq!(s.catalog.len(), 2, "both entries kept");
        assert_eq!(s.catalog[0].id, "a");
        assert_eq!(
            s.catalog[0].effort, None,
            "bad effort nulled, entry not dropped"
        );
        assert_eq!(s.catalog[1].id, "b");
        assert_eq!(
            s.catalog[1].effort,
            Some(EffortLevel::Low),
            "good entry untouched"
        );
        assert!(
            w.iter().any(|x| x.field == "model.catalog[0].effort"),
            "warning names the entry field: {w:?}"
        );
        drop(std::fs::remove_file(&path));
    }

    #[test]
    fn test_bad_context_window_salvaged() {
        // A non-numeric context_window nulls only that field; the entry and
        // its sibling fields survive.
        let path = std::env::temp_dir().join(format!("m19-badctx-{}.json", std::process::id()));
        std::fs::write(
            &path,
            r#"{"model":{"catalog":[{"id":"a","context_window":"huge","effort":"low"}]}}"#,
        )
        .unwrap();
        let (s, w) = load_model_section_from(&path);
        assert_eq!(s.catalog.len(), 1);
        assert_eq!(s.catalog[0].context_window, None, "bad number nulled");
        assert_eq!(
            s.catalog[0].effort,
            Some(EffortLevel::Low),
            "sibling field untouched"
        );
        assert!(
            w.iter()
                .any(|x| x.field == "model.catalog[0].context_window"),
            "warning names the bad field: {w:?}"
        );
        drop(std::fs::remove_file(&path));
    }

    #[test]
    fn test_catalog_not_array_warns() {
        // A catalog that is not an array cannot be salvaged entry by entry;
        // the field falls back to the shipped default catalog (not empty) with
        // a warning naming the malformed field.
        let path = std::env::temp_dir().join(format!("m19-notarray-{}.json", std::process::id()));
        std::fs::write(&path, r#"{"model":{"catalog":"not-an-array"}}"#).unwrap();
        let (s, w) = load_model_section_from(&path);
        assert!(
            !s.catalog.is_empty(),
            "non-array catalog falls back to default"
        );
        assert!(
            w.iter().any(|x| x.field == "model.catalog"),
            "warning names the catalog field: {w:?}"
        );
        drop(std::fs::remove_file(&path));
    }

    #[test]
    fn test_non_object_catalog_dropped() {
        // A non-object array element is dropped with a warning; sibling
        // entries survive.
        let path = std::env::temp_dir().join(format!("m19-nonobj-{}.json", std::process::id()));
        std::fs::write(&path, r#"{"model":{"catalog":[123,{"id":"a"}]}}"#).unwrap();
        let (s, w) = load_model_section_from(&path);
        assert_eq!(s.catalog.len(), 1, "non-object dropped, object kept");
        assert_eq!(s.catalog[0].id, "a");
        assert!(
            w.iter().any(|x| x.field == "model.catalog[0]"),
            "warning names the dropped entry: {w:?}"
        );
        drop(std::fs::remove_file(&path));
    }

    #[test]
    fn test_non_string_id_dropped() {
        // A non-string id reports honestly as a type error, not as blank.
        let path = std::env::temp_dir().join(format!("m19-badid-{}.json", std::process::id()));
        std::fs::write(&path, r#"{"model":{"catalog":[{"id":123}]}}"#).unwrap();
        let (s, w) = load_model_section_from(&path);
        assert!(s.catalog.is_empty(), "non-string id entry dropped");
        assert!(
            w.iter().any(|x| x.field == "model.catalog[0].id"),
            "warning names the id field: {w:?}"
        );
        drop(std::fs::remove_file(&path));
    }

    #[test]
    fn test_model_not_object_warns() {
        // A model section that is not an object (a common shape mistake:
        // writing the id as a string) yields defaults plus a warning, not a
        // silent no-op.
        let path = std::env::temp_dir().join(format!("m19-notobj-{}.json", std::process::id()));
        std::fs::write(&path, r#"{"model":"glm-5.2"}"#).unwrap();
        let (s, w) = load_model_section_from(&path);
        assert!(s.id.is_none(), "non-object model yields defaults");
        assert!(!s.catalog.is_empty(), "falls back to default catalog");
        assert_eq!(w.len(), 1, "one warning, not silent");
        assert_eq!(w[0].field, "model");
        drop(std::fs::remove_file(&path));
    }

    #[test]
    fn test_empty_model_object_default() {
        // An empty model object has every field absent. The catalog falls back
        // to the shipped default; no warnings (a valid, unconfigured state).
        let path = std::env::temp_dir().join(format!("m19-empty-{}.json", std::process::id()));
        std::fs::write(&path, r#"{"model":{}}"#).unwrap();
        let (s, w) = load_model_section_from(&path);
        assert!(s.id.is_none());
        assert!(
            !s.catalog.is_empty(),
            "empty model falls back to default catalog"
        );
        assert!(w.is_empty(), "absent fields are not warnings");
        drop(std::fs::remove_file(&path));
    }

    #[test]
    fn test_null_catalog_falls_back() {
        let path = std::env::temp_dir().join(format!("catalog-null-{}.json", std::process::id()));
        std::fs::write(&path, r#"{"model":{"catalog":null}}"#).unwrap();
        let (s, w) = load_model_section_from(&path);
        assert!(!s.catalog.is_empty(), "null catalog falls back to default");
        assert_eq!(s.catalog.len(), 3, "DEFAULT_CATALOG has 3 entries");
        assert!(
            s.catalog.iter().any(|e| e.id == "glm-5.2"),
            "glm-5.2 in default"
        );
        assert!(
            s.catalog.iter().any(|e| e.id == "qwen3.7-max"),
            "qwen3.7-max in default"
        );
        assert!(w.is_empty(), "no warnings on default catalog fallback");
        drop(std::fs::remove_file(&path));
    }

    #[test]
    fn test_missing_catalog_falls_back() {
        let path =
            std::env::temp_dir().join(format!("catalog-missing-{}.json", std::process::id()));
        std::fs::write(&path, r#"{"model":{"id":"glm-5.2"}}"#).unwrap();
        let (s, _) = load_model_section_from(&path);
        assert!(!s.catalog.is_empty(), "missing catalog field falls back");
        assert_eq!(s.catalog.len(), 3);
        drop(std::fs::remove_file(&path));
    }

    #[test]
    fn test_empty_catalog_falls_back() {
        let path = std::env::temp_dir().join(format!("catalog-empty-{}.json", std::process::id()));
        std::fs::write(&path, r#"{"model":{"catalog":[]}}"#).unwrap();
        let (s, _) = load_model_section_from(&path);
        assert!(!s.catalog.is_empty(), "empty array falls back to default");
        assert_eq!(s.catalog.len(), 3);
        drop(std::fs::remove_file(&path));
    }

    #[test]
    fn test_user_catalog_no_fallback() {
        let path = std::env::temp_dir().join(format!("catalog-user-{}.json", std::process::id()));
        std::fs::write(&path, r#"{"model":{"catalog":[{"id":"my-custom-model"}]}}"#).unwrap();
        let (s, _) = load_model_section_from(&path);
        assert_eq!(s.catalog.len(), 1, "user catalog replaces default entirely");
        assert_eq!(s.catalog[0].id, "my-custom-model");
        drop(std::fs::remove_file(&path));
    }

    #[test]
    fn test_warns_on_unserved_entry() {
        // When the served list is non-empty, a catalog id the provider does
        // not serve gets a warning (a stale id or a typo the name-match cannot
        // catch). The entry is kept — the next fetch may add it back.
        let mut s = section(None, &[entry("qwen3.7-max"), entry("glm-5.2")]);
        let served = vec!["qwen3.7-max".to_string()];
        let w = validate_catalog(&mut s, &served);
        assert_eq!(s.catalog.len(), 2, "both entries kept");
        let stale = w
            .iter()
            .find(|x| x.reason.contains("glm-5.2") && x.field == "model.catalog");
        assert!(stale.is_some(), "unserved entry warns: {w:?}");
        assert!(
            w.iter().all(|x| !x.reason.contains("qwen3.7-max")),
            "served entry does not warn: {w:?}"
        );
    }

    #[test]
    fn test_no_warn_without_cache() {
        // No served list (fetch failed, stub mode, never ran) => skip the
        // existence check entirely. Cannot know, so do not warn.
        let mut s = section(None, &[entry("qwen3.7-max"), entry("glm-5.2")]);
        let w = validate_catalog(&mut s, &[]);
        assert!(
            w.iter().all(|x| !x.reason.contains("served-model list")),
            "no served cache => no existence warning: {w:?}"
        );
    }

    #[test]
    fn test_validate_catalog_substring_match() {
        // A served "qwen3" matches a catalog "qwen3.7-max" by substring, so
        // no warning (the family prefix is enough to confirm the model is
        // served by that provider family).
        let mut s = section(None, &[entry("qwen3.7-max")]);
        let served = vec!["qwen3".to_string()];
        let w = validate_catalog(&mut s, &served);
        assert!(
            w.iter().all(|x| !x.reason.contains("qwen3.7-max")),
            "substring match => no warning: {w:?}"
        );
    }
}
