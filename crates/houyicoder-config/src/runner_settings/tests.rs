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
