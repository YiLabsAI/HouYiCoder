//! Tests for this module.

use super::*;

fn temp_settings(content: &str) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let p = temp_dir().join(format!("houyi-runner-{seq}-{}.json", id()));
    write(&p, content).expect("write");
    p
}

#[test]
fn test_settings_value_wins() {
    let p = temp_settings(r#"{"max_turns": 7}"#);
    let (s, w) = load_runner_settings_from(&p, None);
    remove_file(&p).ok();
    assert_eq!(s.max_turns, 7);
    assert!(w.is_empty());
}

#[test]
fn test_missing_uses_default() {
    let p = temp_settings(r#"{"auto_memory": true}"#);
    let (s, w) = load_runner_settings_from(&p, None);
    remove_file(&p).ok();
    assert_eq!(s.max_turns, DEFAULT_MAX_TURNS);
    assert!(w.is_empty(), "a missing field is not a warning");
}

#[test]
fn test_bad_type_warns() {
    let p = temp_settings(r#"{"max_turns": "two"}"#);
    let (s, w) = load_runner_settings_from(&p, None);
    remove_file(&p).ok();
    assert_eq!(s.max_turns, DEFAULT_MAX_TURNS);
    assert_eq!(w.len(), 1);
}

#[test]
fn test_zero_rejected() {
    let p = temp_settings(r#"{"max_turns": 0}"#);
    let (s, w) = load_runner_settings_from(&p, None);
    remove_file(&p).ok();
    assert_eq!(s.max_turns, DEFAULT_MAX_TURNS, "0 would stop every run");
    assert_eq!(w.len(), 1);
}

#[test]
fn test_env_parse_valid() {
    assert_eq!(parse_env_max_turns("3", &mut Vec::new()), Some(3));
}

#[test]
fn test_env_parse_zero() {
    let mut w = Vec::new();
    assert_eq!(parse_env_max_turns("0", &mut w), None);
    assert_eq!(w.len(), 1, "0 warns, does not silently fall back");
}

#[test]
fn test_env_parse_garbage() {
    let mut w = Vec::new();
    assert_eq!(parse_env_max_turns("abc", &mut w), None);
    assert_eq!(w.len(), 1);
}

/// The foreground budget is read from the same file, and a missing key keeps
/// the default rather than warning.
#[test]
fn test_budget_key_read() {
    let p = temp_settings(r#"{"bash_foreground_budget_ms": 2500}"#);
    let (s, w) = load_runner_settings_from(&p, None);
    remove_file(&p).ok();
    assert_eq!(s.bash_foreground_budget_ms, 2500);
    assert!(w.is_empty(), "a known value is not a warning");
}

/// Zero is the way to turn the background move off, so it is a value rather
/// than a malformed one.
#[test]
fn test_budget_zero_disables() {
    let p = temp_settings(r#"{"bash_foreground_budget_ms": 0}"#);
    let (s, w) = load_runner_settings_from(&p, None);
    remove_file(&p).ok();
    assert_eq!(s.bash_foreground_budget_ms, 0);
    assert!(
        w.is_empty(),
        "zero disables the background move, it is not an error"
    );
}

#[test]
fn test_budget_bad_type_warns() {
    // The parser is asked directly: the loader also reads the process env,
    // and a budget set there would decide the answer instead of the file.
    let mut w = Vec::new();
    let settings = serde_json::json!({"bash_foreground_budget_ms": "fast"});
    assert_eq!(parse_bash_budget(&settings, &mut w), None);
    assert_eq!(w.len(), 1);
    assert!(w[0].reason.contains("ignored"), "{}", w[0].reason);
}

#[test]
fn test_budget_env_parsed() {
    assert_eq!(parse_env_bash_budget("900", &mut Vec::new()), Some(900));
    let mut w = Vec::new();
    assert_eq!(parse_env_bash_budget("soon", &mut w), None);
    assert_eq!(w.len(), 1);
}

/// The delegation budget is read from the same file, so a workspace can let a
/// long delegation release the turn without changing any other default.
#[test]
fn test_delegation_budget_read() {
    let p = temp_settings(r#"{"delegation_foreground_budget_ms": 2500}"#);
    let (s, w) = load_runner_settings_from(&p, None);
    remove_file(&p).ok();
    assert_eq!(s.delegation_foreground_budget_ms, 2500);
    assert!(w.is_empty(), "a known value is not a warning");
}

/// Zero keeps the blocking behavior, and a missing key is a value rather than
/// a warning. The parser is asked directly because the loader also reads the
/// process env, and a budget set there would decide the answer.
#[test]
fn test_delegation_budget_off() {
    let mut w = Vec::new();
    assert_eq!(
        parse_delegation_budget(&serde_json::json!({}), &mut w),
        None,
        "a missing key resolves to the default budget"
    );
    assert!(w.is_empty(), "a missing key is not a warning");
    assert_eq!(
        parse_delegation_budget(
            &serde_json::json!({"delegation_foreground_budget_ms": 0}),
            &mut w
        ),
        Some(0),
        "zero is how the background move is turned off"
    );
    assert!(w.is_empty());
}

/// The settings default blocks, so a session that never configures a budget
/// keeps the behavior it had before the setting existed.
#[test]
fn test_delegation_budget_default() {
    let settings = RunnerSettings::default();
    assert_eq!(settings.delegation_foreground_budget_ms, 0);
}

/// A value the parser cannot read is ignored with a warning, and the default
/// applies instead: a malformed budget must not stop the runner from loading.
#[test]
fn test_delegation_budget_bad_value() {
    let mut w = Vec::new();
    let settings = serde_json::json!({"delegation_foreground_budget_ms": "fast"});
    assert_eq!(parse_delegation_budget(&settings, &mut w), None);
    assert_eq!(w.len(), 1);
    assert!(w[0].reason.contains("ignored"), "{}", w[0].reason);

    let mut negative = Vec::new();
    let settings = serde_json::json!({"delegation_foreground_budget_ms": -5});
    assert_eq!(parse_delegation_budget(&settings, &mut negative), None);
    assert_eq!(negative.len(), 1);
}

#[test]
fn test_delegation_budget_env_parsed() {
    assert_eq!(
        parse_env_delegation_budget("900", &mut Vec::new()),
        Some(900)
    );
    let mut w = Vec::new();
    assert_eq!(parse_env_delegation_budget("soon", &mut w), None);
    assert_eq!(w.len(), 1);
}
