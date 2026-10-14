//! Tests for this module.

use super::*;

fn entry(id: &str) -> ModelEntry {
    ModelEntry {
        id: id.into(),
        display_name: None,
        description: None,
        effort: None,
        effort_levels: None,
        context_window: None,
        max_output_tokens: None,
        fast: None,
    }
}

fn section(id: Option<&str>, catalog: &[ModelEntry]) -> ModelSection {
    ModelSection {
        id: id.map(str::to_string),
        effort_level: None,
        speed_mode: None,
        catalog: catalog.to_vec(),
    }
}

#[test]
fn test_speed_mode_roundtrips() {
    // The global speed preference round-trips through settings.json, and
    // a bad value degrades to None with a warning rather than resetting
    // the section.
    let path = temp_dir().join(format!("speed-mode-{}.json", id()));
    write(
        &path,
        r#"{"model":{"speed_mode":"fast","catalog":[{"id":"a"}]}}"#,
    )
    .unwrap();
    let (s, w) = load_model_section_from(&path);
    assert_eq!(s.speed_mode, Some(SpeedMode::Fast));
    assert_eq!(s.catalog.len(), 1, "sibling catalog survives");
    assert!(w.is_empty(), "a valid speed_mode does not warn: {w:?}");
    drop(remove_file(&path));

    write(&path, r#"{"model":{"speed_mode":"warp"}}"#).unwrap();
    let (s, w) = load_model_section_from(&path);
    assert_eq!(s.speed_mode, None, "bad value falls back to the default");
    assert!(
        w.iter().any(|x| x.field == "model.speed_mode"),
        "warning names the bad field: {w:?}"
    );
    assert!(!s.catalog.is_empty(), "catalog survives a bad sibling");
    drop(remove_file(&path));
}

#[test]
fn test_entry_fast_override() {
    // A per-model fast flag parses on its own entry; a bad value nulls
    // only that field.
    let path = temp_dir().join(format!("entry-fast-{}.json", id()));
    write(
        &path,
        r#"{"model":{"catalog":[{"id":"a","fast":true},{"id":"b","fast":"yes"}]}}"#,
    )
    .unwrap();
    let (s, w) = load_model_section_from(&path);
    assert_eq!(s.catalog.len(), 2, "both entries kept");
    assert_eq!(s.catalog[0].fast, Some(true));
    assert_eq!(s.catalog[1].fast, None, "bad boolean nulled, entry kept");
    assert!(
        w.iter().any(|x| x.field == "model.catalog[1].fast"),
        "warning names the bad field: {w:?}"
    );
    drop(remove_file(&path));
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
    let path = temp_dir().join(format!("catalog-order-{}.json", id()));
    write(
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
    drop(remove_file(&path));
}

#[test]
fn test_bad_effort_keeps_catalog() {
    // A typo'd effort_level must degrade only that field; the catalog
    // stays usable and effort falls to the next layer. The whole-section
    // reset (catalog lost) is the failure mode this guards against.
    let path = temp_dir().join(format!("m19-badeff-{}.json", id()));
    write(
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
    drop(remove_file(&path));
}

#[test]
fn test_bad_catalog_entry_salvaged() {
    // A bad effort on one catalog entry nulls that entry's effort; the
    // entry itself and the rest of the catalog survive. Same failure
    // class as the top-level case, one layer down.
    let path = temp_dir().join(format!("m19-entryeff-{}.json", id()));
    write(
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
    drop(remove_file(&path));
}

#[test]
fn test_bad_context_window_salvaged() {
    // A non-numeric context_window nulls only that field; the entry and
    // its sibling fields survive.
    let path = temp_dir().join(format!("m19-badctx-{}.json", id()));
    write(
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
    drop(remove_file(&path));
}

#[test]
fn test_catalog_not_array_warns() {
    // A catalog that is not an array cannot be salvaged entry by entry;
    // the field falls back to the shipped default catalog (not empty) with
    // a warning naming the malformed field.
    let path = temp_dir().join(format!("m19-notarray-{}.json", id()));
    write(&path, r#"{"model":{"catalog":"not-an-array"}}"#).unwrap();
    let (s, w) = load_model_section_from(&path);
    assert!(
        !s.catalog.is_empty(),
        "non-array catalog falls back to default"
    );
    assert!(
        w.iter().any(|x| x.field == "model.catalog"),
        "warning names the catalog field: {w:?}"
    );
    drop(remove_file(&path));
}

#[test]
fn test_non_object_catalog_dropped() {
    // A non-object array element is dropped with a warning; sibling
    // entries survive.
    let path = temp_dir().join(format!("m19-nonobj-{}.json", id()));
    write(&path, r#"{"model":{"catalog":[123,{"id":"a"}]}}"#).unwrap();
    let (s, w) = load_model_section_from(&path);
    assert_eq!(s.catalog.len(), 1, "non-object dropped, object kept");
    assert_eq!(s.catalog[0].id, "a");
    assert!(
        w.iter().any(|x| x.field == "model.catalog[0]"),
        "warning names the dropped entry: {w:?}"
    );
    drop(remove_file(&path));
}

#[test]
fn test_non_string_id_dropped() {
    // A non-string id reports honestly as a type error, not as blank.
    let path = temp_dir().join(format!("m19-badid-{}.json", id()));
    write(&path, r#"{"model":{"catalog":[{"id":123}]}}"#).unwrap();
    let (s, w) = load_model_section_from(&path);
    assert!(s.catalog.is_empty(), "non-string id entry dropped");
    assert!(
        w.iter().any(|x| x.field == "model.catalog[0].id"),
        "warning names the id field: {w:?}"
    );
    drop(remove_file(&path));
}

#[test]
fn test_model_not_object_warns() {
    // A model section that is not an object (a common shape mistake:
    // writing the id as a string) yields defaults plus a warning, not a
    // silent no-op.
    let path = temp_dir().join(format!("m19-notobj-{}.json", id()));
    write(&path, r#"{"model":"glm-5.2"}"#).unwrap();
    let (s, w) = load_model_section_from(&path);
    assert!(s.id.is_none(), "non-object model yields defaults");
    assert!(!s.catalog.is_empty(), "falls back to default catalog");
    assert_eq!(w.len(), 1, "one warning, not silent");
    assert_eq!(w[0].field, "model");
    drop(remove_file(&path));
}

#[test]
fn test_empty_model_object_default() {
    // An empty model object has every field absent. The catalog falls back
    // to the shipped default; no warnings (a valid, unconfigured state).
    let path = temp_dir().join(format!("m19-empty-{}.json", id()));
    write(&path, r#"{"model":{}}"#).unwrap();
    let (s, w) = load_model_section_from(&path);
    assert!(s.id.is_none());
    assert!(
        !s.catalog.is_empty(),
        "empty model falls back to default catalog"
    );
    assert!(w.is_empty(), "absent fields are not warnings");
    drop(remove_file(&path));
}

#[test]
fn test_null_catalog_falls_back() {
    let path = temp_dir().join(format!("catalog-null-{}.json", id()));
    write(&path, r#"{"model":{"catalog":null}}"#).unwrap();
    let (s, w) = load_model_section_from(&path);
    assert!(!s.catalog.is_empty(), "null catalog falls back to default");
    assert_eq!(s.catalog.len(), 4, "DEFAULT_CATALOG has 4 entries");
    assert!(
        s.catalog.iter().any(|e| e.id == "glm-5.2"),
        "glm-5.2 in default"
    );
    assert!(
        s.catalog.iter().any(|e| e.id == "qwen3.7-max"),
        "qwen3.7-max in default"
    );
    assert!(w.is_empty(), "no warnings on default catalog fallback");
    drop(remove_file(&path));
}

#[test]
fn test_missing_catalog_falls_back() {
    let path = temp_dir().join(format!("catalog-missing-{}.json", id()));
    write(&path, r#"{"model":{"id":"glm-5.2"}}"#).unwrap();
    let (s, _) = load_model_section_from(&path);
    assert!(!s.catalog.is_empty(), "missing catalog field falls back");
    assert_eq!(s.catalog.len(), 4);
    drop(remove_file(&path));
}

#[test]
fn test_empty_catalog_falls_back() {
    let path = temp_dir().join(format!("catalog-empty-{}.json", id()));
    write(&path, r#"{"model":{"catalog":[]}}"#).unwrap();
    let (s, _) = load_model_section_from(&path);
    assert!(!s.catalog.is_empty(), "empty array falls back to default");
    assert_eq!(s.catalog.len(), 4);
    drop(remove_file(&path));
}

#[test]
fn test_user_catalog_no_fallback() {
    let path = temp_dir().join(format!("catalog-user-{}.json", id()));
    write(&path, r#"{"model":{"catalog":[{"id":"my-custom-model"}]}}"#).unwrap();
    let (s, _) = load_model_section_from(&path);
    assert_eq!(s.catalog.len(), 1, "user catalog replaces default entirely");
    assert_eq!(s.catalog[0].id, "my-custom-model");
    drop(remove_file(&path));
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
