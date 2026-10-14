//! Tests for this module.

use super::*;
use std::fs;
use std::path::PathBuf;

fn temp_settings(content: &str) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let p = temp_dir().join(format!("houyi-ret-{seq}-{}.json", id()));
    fs::write(&p, content).expect("write");
    p
}

#[test]
fn test_load_reads_both_fields() {
    let p = temp_settings(
        r#"{"session_retention_days": 14, "session_retention_count": 200, "prune_confirm_threshold": 50}"#,
    );
    let (cfg, w) = load_retention_from(&p);
    assert_eq!(cfg.session_retention_days, 14);
    assert_eq!(cfg.session_retention_count, 200);
    assert_eq!(cfg.prune_confirm_threshold, 50);
    assert!(w.is_empty());
    fs::remove_file(&p).ok();
}

#[test]
fn test_load_defaults_on_missing() {
    let p = temp_settings(r#"{"auto_memory": true}"#);
    let (cfg, w) = load_retention_from(&p);
    assert_eq!(cfg, RetentionConfig::default());
    assert!(w.is_empty(), "missing fields are not warnings");
    fs::remove_file(&p).ok();
}

#[test]
fn test_load_defaults_on_corrupt() {
    let p = temp_settings("not json");
    let (cfg, w) = load_retention_from(&p);
    assert_eq!(cfg, RetentionConfig::default());
    assert_eq!(w.len(), 1, "one warning for the corrupt file");
    fs::remove_file(&p).ok();
}

#[test]
fn test_load_warns_bad_type() {
    // days is a string (bad), count is valid (200): days defaults, count
    // reads normally, one warning for days.
    let p = temp_settings(r#"{"session_retention_days": "two", "session_retention_count": 200}"#);
    let (cfg, w) = load_retention_from(&p);
    assert_eq!(cfg.session_retention_days, DEFAULT_DAYS);
    assert_eq!(cfg.session_retention_count, 200);
    assert_eq!(w.len(), 1);
    fs::remove_file(&p).ok();
}

#[test]
fn test_load_zero_optout() {
    let p = temp_settings(r#"{"session_retention_days": 0, "session_retention_count": 0}"#);
    let (cfg, w) = load_retention_from(&p);
    assert_eq!(cfg.session_retention_days, 0);
    assert_eq!(cfg.session_retention_count, 0);
    assert!(w.is_empty(), "0 is a legal opt-out, not a warning");
    fs::remove_file(&p).ok();
}

#[test]
fn test_load_warns_camelcase_alias() {
    let p = temp_settings(r#"{"sessionRetentionDays": 7}"#);
    let (cfg, w) = load_retention_from(&p);
    assert_eq!(
        cfg.session_retention_days, DEFAULT_DAYS,
        "the camelCase spelling must not be accepted"
    );
    assert_eq!(w.len(), 1);
    assert_eq!(w[0].field, "sessionRetentionDays");
    assert!(
        w[0].reason.contains("session_retention_days"),
        "the warning must name the canonical key: {}",
        w[0].reason
    );
    fs::remove_file(&p).ok();
}

#[test]
fn test_load_alias_beside_canonical() {
    // The snake_case key still reads normally next to the dead alias,
    // and the dead alias is still flagged so the author removes it.
    let p = temp_settings(r#"{"sessionRetentionDays": 7, "session_retention_days": 14}"#);
    let (cfg, w) = load_retention_from(&p);
    assert_eq!(cfg.session_retention_days, 14);
    assert_eq!(w.len(), 1);
    fs::remove_file(&p).ok();
}
