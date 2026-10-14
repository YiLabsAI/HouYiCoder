//! Tests for this module.

use super::*;
use houyicoder_config::ModelEntry;

fn resolver_with(catalog: &[(&str, Option<u32>, Option<u32>)]) -> SettingsCatalogResolver {
    let section = ModelSection {
        id: None,
        effort_level: None,
        speed_mode: None,
        catalog: catalog
            .iter()
            .map(|(id, ctx, max)| ModelEntry {
                id: id.to_string(),
                display_name: None,
                description: None,
                effort: None,
                effort_levels: None,
                context_window: *ctx,
                max_output_tokens: *max,
                fast: None,
            })
            .collect(),
    };
    SettingsCatalogResolver::from_section(section)
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
        effort_to_persist(Some(EffortLevel::High), Some(EffortLevel::Medium)),
        Some(EffortLevel::High)
    );
}

#[test]
fn test_picked_equal_skips_persist() {
    assert_eq!(
        effort_to_persist(Some(EffortLevel::Medium), Some(EffortLevel::Medium)),
        None
    );
}

#[test]
fn test_return_to_default_clears() {
    // A deliberate return to the fallback default clears the explicit
    // value: the pick is not worth pinning, and the chain must lead a
    // later default change instead of re-reading the old pick.
    assert_eq!(
        effort_to_persist(Some(EffortLevel::Medium), Some(EffortLevel::Medium)),
        None,
        "pick equal to the fallback default yields nothing to persist"
    );
}

#[test]
fn test_auto_no_default_clears() {
    // Auto with no fallback default is itself the default: nothing to pin.
    assert_eq!(effort_to_persist(None, None), None);
}

/// A Fast pick writes model.speed_mode, and a later Standard pick
/// rewrites it: the stored tier must never ask a model for Fast it
/// cannot serve (the apply path clamps before persisting, so the value
/// arriving here is already the served tier).
#[test]
fn test_speed_mode_persisted() {
    let path = std::env::temp_dir().join(format!("m-speed-{}.json", std::process::id()));
    std::fs::write(
        &path,
        r#"{"model":{"id":"glm-5.2","catalog":[{"id":"glm-5.2","fast":true}]}}"#,
    )
    .unwrap();
    persist_model_pick(&path, Some("glm-5.2"), None, false, SpeedMode::Fast).unwrap();
    let back: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(
        back["model"]["speed_mode"], "fast",
        "a Fast pick writes the tier: {back}"
    );
    // The tier moved, so the write fires again — Standard is not pinned
    // by a no-op write but by an actual change.
    persist_model_pick(&path, Some("glm-5.2"), None, false, SpeedMode::Standard).unwrap();
    let back: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(
        back["model"]["speed_mode"], "standard",
        "a moved tier rewrites the stored value: {back}"
    );
    drop(std::fs::remove_file(&path));
}

/// A pick whose target has no catalog entry on disk (the shipped
/// fallback catalog) materializes a minimal entry, so the effort is not
/// silently dropped while the receipt says saved.
#[test]
fn test_effort_materializes_entry() {
    let path = std::env::temp_dir().join(format!("m-effort-new-{}.json", std::process::id()));
    std::fs::write(&path, "{}").unwrap();
    persist_model_pick(
        &path,
        Some("qwen3.7-max"),
        Some(EffortLevel::High),
        true,
        SpeedMode::Standard,
    )
    .unwrap();
    let back: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(
        back["model"]["catalog"][0]["id"], "qwen3.7-max",
        "the pick's target gets an entry: {back}"
    );
    assert_eq!(
        back["model"]["catalog"][0]["effort"], "high",
        "the effort reaches disk: {back}"
    );
    assert!(
        back["model"].get("speed_mode").is_none(),
        "a Standard pick on an unset section pins no tier: {back}"
    );
    drop(std::fs::remove_file(&path));
}

/// The stored tier only carries meaning when it is Fast: clearing a
/// stored Fast writes Standard, but a Standard pick on an unset section
/// writes nothing.
#[test]
fn test_speed_write_guards_pin() {
    let path = std::env::temp_dir().join(format!("m-speed-pin-{}.json", std::process::id()));
    std::fs::write(&path, r#"{"model":{}}"#).unwrap();
    persist_model_pick(&path, Some("glm-5.2"), None, false, SpeedMode::Standard).unwrap();
    let back: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert!(
        back["model"].get("speed_mode").is_none(),
        "Standard on unset writes nothing: {back}"
    );
    // Clearing a stored Fast is a real change and writes.
    persist_model_pick(&path, Some("glm-5.2"), None, false, SpeedMode::Fast).unwrap();
    persist_model_pick(&path, Some("glm-5.2"), None, false, SpeedMode::Standard).unwrap();
    let back: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(
        back["model"]["speed_mode"], "standard",
        "clearing Fast writes"
    );
    drop(std::fs::remove_file(&path));
}

#[test]
fn test_clears_on_global_default() {
    let path = std::env::temp_dir().join(format!("m-return-{}.json", std::process::id()));
    std::fs::write(
        &path,
        r#"{"model":{"effort_level":"medium","catalog":[{"id":"qwen3.7-max","effort":"low"}]}}"#,
    )
    .unwrap();
    // User adjusts back to the global default Medium: the pick equals the
    // fallback, so the per-model effort is cleared, not pinned as Medium.
    persist_model_pick(
        &path,
        Some("qwen3.7-max"),
        Some(EffortLevel::Medium),
        true,
        SpeedMode::Standard,
    )
    .unwrap();
    let back: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert!(
        back["model"]["catalog"][0].get("effort").is_none(),
        "returning to the global default clears the per-model key: {back}"
    );
    // With the key gone, the resolver leads from the global fallback, so a
    // later global change propagates without re-touching the entry.
    std::fs::write(
        &path,
        r#"{"model":{"effort_level":"high","catalog":[{"id":"qwen3.7-max"}]}}"#,
    )
    .unwrap();
    let (section, _) = houyicoder_config::load_model_section_from(&path);
    let resolver = SettingsCatalogResolver::from_section(section);
    assert_eq!(
        resolver.catalog_effort("qwen3.7-max"),
        Some(EffortLevel::High),
        "the chain follows the new global default after the clear"
    );
    drop(std::fs::remove_file(&path));
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
