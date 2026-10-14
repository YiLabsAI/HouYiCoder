//! Tests for this module.

use super::*;

#[test]
fn test_model_section_round_trip() {
    let path = temp_dir().join(format!("model-{}.json", id()));
    write(
        &path,
        r#"{"model":{"id":"qwen3-coder","effort_level":"high","catalog":[{"id":"qwen3-coder","effort":"low","context_window":131072}]}}"#,
    )
    .unwrap();
    let (s, w) = load_model_section_from(&path);
    assert_eq!(s.id.as_deref(), Some("qwen3-coder"));
    assert_eq!(s.effort_level, Some(EffortLevel::High));
    assert_eq!(s.catalog.len(), 1);
    assert_eq!(s.catalog[0].id, "qwen3-coder");
    assert_eq!(s.catalog[0].effort, Some(EffortLevel::Low));
    assert_eq!(s.catalog[0].context_window, Some(131072));
    assert!(w.is_empty(), "no warnings on a valid model section");
    drop(remove_file(&path));
}

#[test]
fn test_model_section_missing() {
    let path = temp_dir().join(format!("model-missing-{}.json", id()));
    write(&path, r#"{"auto_memory":true}"#).unwrap();
    let (s, w) = load_model_section_from(&path);
    assert!(s.id.is_none(), "no model section => default");
    assert!(!s.catalog.is_empty(), "falls back to default catalog");
    assert!(w.is_empty(), "a missing section is not a warning");
    drop(remove_file(&path));
}

#[test]
fn test_model_section_corrupt() {
    let path = temp_dir().join(format!("model-corrupt-{}.json", id()));
    write(&path, "not json {").unwrap();
    let (s, w) = load_model_section_from(&path);
    assert!(s.id.is_none(), "corrupt => default");
    assert_eq!(w.len(), 1, "corrupt file warns");
    drop(remove_file(&path));
}

#[test]
fn test_defaults_are_set() {
    assert_eq!(
        DEFAULT_BASE_URL,
        "https://dashscope.aliyuncs.com/compatible-mode/v1"
    );
    assert_eq!(DEFAULT_MODEL, "qwen3.7-max");
}

#[test]
fn test_first_non_empty_priority() {
    // First non-empty wins; order encodes the env var priority chain.
    assert_eq!(
        first_non_empty(&[Some("a".into()), Some("b".into())]).as_deref(),
        Some("a")
    );
    assert_eq!(
        first_non_empty(&[None, Some("b".into()), None]).as_deref(),
        Some("b")
    );
}

#[test]
fn test_first_non_empty_skips() {
    // A set-but-empty env var must not shadow a later real value.
    assert_eq!(
        first_non_empty(&[Some(String::new()), Some("b".into())]).as_deref(),
        Some("b")
    );
}

#[test]
fn test_first_non_empty_all() {
    assert_eq!(first_non_empty(&[None, None, None]), None);
    assert_eq!(
        first_non_empty(&[Some(String::new()), Some(String::new())]),
        None
    );
}

#[test]
fn test_resolve_base_url_default() {
    // When neither env var is set in the test process, the default applies.
    // (make check does not source .env, so these are unset here.)
    let base = resolve_base_url();
    assert!(
        base == DEFAULT_BASE_URL
            || base == var(ENV_DASHSCOPE_BASE_URL).unwrap_or_default()
            || base == var(ENV_OPENAI_BASE_URL).unwrap_or_default()
    );
}

#[test]
fn test_resolve_model_default() {
    // A settings file with no model.id yields DEFAULT_MODEL. Uses the
    // pure loader against a temp path so the test never touches the real
    // user settings.json, and no env var can shadow the pick.
    let path = temp_dir().join(format!("no-model-{}.json", id()));
    write(&path, r#"{"auto_memory":true}"#).unwrap();
    assert_eq!(resolve_model_from(&path), DEFAULT_MODEL);
    drop(remove_file(&path));
}

#[test]
fn test_resolve_model_from_id() {
    // A settings.json with model.id overrides DEFAULT_MODEL. The env
    // layer is gone, so the persisted id is the authority.
    let path = temp_dir().join(format!("with-model-{}.json", id()));
    write(
        &path,
        r#"{"model":{"id":"qwen3-coder","catalog":[{"id":"qwen3-coder"}]}}"#,
    )
    .unwrap();
    assert_eq!(resolve_model_from(&path), "qwen3-coder");
    drop(remove_file(&path));
}

#[test]
fn test_resolve_model_ignores_env() {
    // Setting HOUYICODER_TEST_MODEL in the process must not shadow the
    // settings pick: resolve_* reads no env. The var name still exists in
    // the wild; resolving ignores it (whether set or unset).
    let path = temp_dir().join(format!("no-env-{}.json", id()));
    write(
        &path,
        r#"{"model":{"id":"qwen3-coder","catalog":[{"id":"qwen3-coder"}]}}"#,
    )
    .unwrap();
    assert_eq!(resolve_model_from(&path), "qwen3-coder");
    drop(remove_file(&path));
}

#[test]
fn test_default_pick_ignores_settings() {
    // The Default pick resolves to the constant even when settings names
    // another id: that id is the selection the pick replaces, so reading
    // it would keep the session on the model the user deselected.
    assert_eq!(resolve_default_model(), DEFAULT_MODEL);
    let path = temp_dir().join(format!("default-pick-{}.json", id()));
    write(
        &path,
        r#"{"model":{"id":"glm-5.2","catalog":[{"id":"glm-5.2"}]}}"#,
    )
    .unwrap();
    assert_eq!(
        resolve_model_from(&path),
        "glm-5.2",
        "a fresh session still honours the persisted id"
    );
    assert_eq!(
        resolve_default_model(),
        DEFAULT_MODEL,
        "a Default pick does not read the persisted id"
    );
    drop(remove_file(&path));
}

#[test]
fn test_parse_mcp_servers_empty() {
    // An unset or empty value yields no external server.
    assert!(parse_mcp_servers(None).unwrap().is_empty());
    assert!(parse_mcp_servers(Some("")).unwrap().is_empty());
    assert!(parse_mcp_servers(Some("   ")).unwrap().is_empty());
}

#[test]
fn test_parse_mcp_servers_array() {
    let raw = r#"[
        {"program":"node","args":["server.js"]},
        {"program":"python","args":["-m","srv"]}
    ]"#;
    let list = parse_mcp_servers(Some(raw)).unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(list[0].program, "node");
    assert_eq!(list[0].args, vec!["server.js"]);
    assert_eq!(list[1].program, "python");
    assert_eq!(list[1].args, vec!["-m", "srv"]);
}

#[test]
fn test_parse_mcp_servers_invalid() {
    // A non-array value is an error so a config typo does not silently
    // register zero tools.
    assert!(parse_mcp_servers(Some("not json")).is_err());
    assert!(parse_mcp_servers(Some("[{}]")).is_err());
}

#[test]
fn test_parse_servers_drops_empty() {
    let raw = r#"[{"program":"","args":[]}]"#;
    let list = parse_mcp_servers(Some(raw)).unwrap();
    assert!(list.is_empty());
}

/// A missing settings file yields the default (both on).
#[test]
fn test_load_missing_is_default() {
    let path = temp_dir().join(format!("nope-{}.json", id()));
    let (t, _w) = load_toggles_from(&path);
    assert!(t.auto_memory && t.auto_dream, "missing file => both on");
}

/// A corrupt settings file yields the default, not a panic.
#[test]
fn test_load_corrupt_is_default() {
    let path = temp_dir().join(format!("bad-{}.json", id()));
    write(&path, "not json {").unwrap();
    let (t, _w) = load_toggles_from(&path);
    assert!(t.auto_memory && t.auto_dream, "corrupt file => both on");
    drop(remove_file(&path));
}

/// Save then load round-trips a non-default state.
#[test]
fn test_save_load_round_trips() {
    let path = temp_dir().join(format!("toggles-{}.json", id()));
    let toggles = MemoryToggles {
        auto_memory: false,
        auto_dream: true,
    };
    save_toggles_to(&path, &toggles).expect("save succeeds");
    let (loaded, _w) = load_toggles_from(&path);
    assert_eq!(loaded, toggles, "round-trip preserves the off/on state");
    drop(remove_file(&path));
}

/// An unwritable settings path returns the failure instead of dropping
/// it, so the caller can keep runtime state unchanged.
#[test]
fn test_save_failure_returns_err() {
    let blocker = temp_dir().join(format!("toggles-block-{}", id()));
    write(&blocker, "regular file, not a directory").unwrap();
    let path = blocker.join("settings.json");
    let err = save_toggles_to(&path, &MemoryToggles::default()).unwrap_err();
    assert!(
        matches!(err, SettingsWriteError::Io(_)),
        "parent is a file, so the write fails with Io: {err:?}"
    );
    drop(remove_file(&blocker));
}

/// config_home + settings_path resolve under the user HOME. Deterministic
/// (no file I/O, just path construction from HOME which the test env sets).
#[test]
fn test_config_home_resolves() {
    assert!(config_home().ends_with(".houyicoder"));
    let path = settings_path();
    assert!(path.ends_with("settings.json"));
    assert!(path.starts_with(config_home()));
}

/// A settings JSON missing a field uses the serde default, so a partial
/// file from an older build does not brick the load.
#[test]
fn test_partial_json_uses_defaults() {
    let path = temp_dir().join(format!("partial-{}.json", id()));
    write(&path, r#"{"auto_dream":false}"#).unwrap();
    let (t, _w) = load_toggles_from(&path);
    assert!(t.auto_memory, "missing auto_memory defaults on");
    assert!(!t.auto_dream, "auto_dream false from the file");
    drop(remove_file(&path));
}

/// A bad-typed field falls back to its default and warns; a sibling field
/// is still read normally. One typo does not reset the whole file.
#[test]
fn test_bad_field_defaults_warns() {
    let path = temp_dir().join(format!("badfield-{}.json", id()));
    write(&path, r#"{"auto_memory":"yes","auto_dream":false}"#).unwrap();
    let (t, warnings) = load_toggles_from(&path);
    assert!(t.auto_memory, "bad auto_memory falls back to default true");
    assert!(
        !t.auto_dream,
        "auto_dream read normally despite sibling bad field"
    );
    assert_eq!(warnings.len(), 1, "one warning for the bad field");
    assert!(
        warnings[0].field.contains("auto_memory"),
        "warning names the bad field"
    );
    drop(remove_file(&path));
}
