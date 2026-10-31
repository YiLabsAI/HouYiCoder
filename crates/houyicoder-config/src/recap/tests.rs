//! Tests for this module.

use super::*;
use crate::recap::DEFAULT_RECAP_IDLE_SECS;
use std::env::temp_dir;
use std::fs;
use std::path::PathBuf;
use std::process::id;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

fn temp_settings(content: &str) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let path = temp_dir().join(format!("houyi-recap-{seq}-{}.json", id()));
    fs::write(&path, content).expect("write");
    path
}

#[test]
fn test_missing_file_stays_off() {
    let path = temp_dir().join(format!("houyi-recap-absent-{}.json", id()));
    let (cfg, warnings) = load_recap_from(&path);
    assert_eq!(cfg, RecapConfig::default());
    assert!(warnings.is_empty(), "no file is not a warning");
}

#[test]
fn test_empty_section_stays_off() {
    let path = temp_settings(r#"{"recap": {}}"#);
    let (cfg, warnings) = load_recap_from(&path);
    assert_eq!(
        cfg,
        RecapConfig::default(),
        "a section that names neither field leaves both at their defaults"
    );
    assert!(warnings.is_empty(), "both fields are optional");
    fs::remove_file(&path).ok();
}

#[test]
fn test_load_reads_fields() {
    let path = temp_settings(r#"{"recap": {"enabled": true, "model": "small-1"}}"#);
    let (cfg, warnings) = load_recap_from(&path);
    assert_eq!(
        cfg,
        RecapConfig {
            enabled: true,
            model: Some("small-1".to_string()),
            idle_trigger_secs: DEFAULT_RECAP_IDLE_SECS,
        }
    );
    assert!(warnings.is_empty());
    fs::remove_file(&path).ok();
}

#[test]
fn test_model_id_is_trimmed() {
    let path = temp_settings(r#"{"recap": {"enabled": true, "model": "  small-1 "}}"#);
    let (cfg, warnings) = load_recap_from(&path);
    assert_eq!(
        cfg.model.as_deref(),
        Some("small-1"),
        "a stray space must not turn a named entry into an unknown model"
    );
    assert!(warnings.is_empty());
    fs::remove_file(&path).ok();
}

#[test]
fn test_absent_section_stays_off() {
    let path = temp_settings(r#"{"auto_memory": true}"#);
    let (cfg, warnings) = load_recap_from(&path);
    assert_eq!(cfg, RecapConfig::default());
    assert!(
        !cfg.enabled,
        "a file that never names the section stays off"
    );
    assert!(warnings.is_empty(), "an absent section is not a warning");
    fs::remove_file(&path).ok();
}

#[test]
fn test_load_reports_corrupt_file() {
    let path = temp_settings("not json");
    let (cfg, warnings) = load_recap_from(&path);
    assert_eq!(cfg, RecapConfig::default());
    assert_eq!(warnings.len(), 1, "one warning for the corrupt file");
    assert!(!cfg.enabled, "a file this build cannot read spends nothing");
    fs::remove_file(&path).ok();
}

#[test]
fn test_load_ignores_bad_enabled() {
    let path = temp_settings(r#"{"recap": {"enabled": "yes", "model": "small-1"}}"#);
    let (cfg, warnings) = load_recap_from(&path);
    assert!(!cfg.enabled, "a non-boolean switch stays off");
    assert_eq!(
        cfg.model.as_deref(),
        Some("small-1"),
        "the model beside a bad switch still reads"
    );
    assert_eq!(warnings.len(), 1);
    assert_eq!(warnings[0].field, "recap.enabled");
    fs::remove_file(&path).ok();
}

#[test]
fn test_load_ignores_bad_model() {
    let path = temp_settings(r#"{"recap": {"enabled": true, "model": 7}}"#);
    let (cfg, warnings) = load_recap_from(&path);
    assert!(cfg.enabled);
    assert_eq!(cfg.model, None, "a malformed model is dropped, not guessed");
    assert_eq!(warnings.len(), 1);
    assert_eq!(warnings[0].field, "recap.model");
    fs::remove_file(&path).ok();
}

#[test]
fn test_load_ignores_non_object() {
    let path = temp_settings(r#"{"recap": true}"#);
    let (cfg, warnings) = load_recap_from(&path);
    assert_eq!(cfg, RecapConfig::default());
    assert_eq!(warnings.len(), 1);
    assert_eq!(warnings[0].field, "recap");
    fs::remove_file(&path).ok();
}

#[test]
fn test_load_ignores_empty_model() {
    let path = temp_settings(r#"{"recap": {"enabled": true, "model": "  "}}"#);
    let (cfg, warnings) = load_recap_from(&path);
    assert!(cfg.enabled);
    assert_eq!(cfg.model, None, "whitespace names no model");
    assert_eq!(warnings.len(), 1);
    assert_eq!(warnings[0].field, "recap.model");
    fs::remove_file(&path).ok();
}

#[test]
fn test_enabled_without_model_warns() {
    let path = temp_settings(r#"{"recap": {"enabled": true}}"#);
    let (cfg, warnings) = load_recap_from(&path);
    assert!(cfg.enabled);
    assert_eq!(
        cfg.model, None,
        "no catalog fact stands in for a model the user did not name"
    );
    assert_eq!(warnings.len(), 1, "the inert feature is reported once");
    assert_eq!(warnings[0].field, "recap.model");
    fs::remove_file(&path).ok();
}

#[test]
fn test_disabled_section_is_silent() {
    let path = temp_settings(r#"{"recap": {}}"#);
    let (_cfg, warnings) = load_recap_from(&path);
    assert!(
        warnings.is_empty(),
        "a feature the file never turned on has nothing to warn about"
    );
    fs::remove_file(&path).ok();
}

/// A file that names an idle window uses it. The field is what the Config tab
/// steps through, so a value the loader drops would make the tab's change
/// disappear on the next read.
#[test]
fn test_idle_window_read() {
    let path =
        temp_settings(r#"{"recap": {"enabled": true, "model": "m", "idle_trigger_secs": 600}}"#);
    let (cfg, warnings) = load_recap_from(&path);
    assert_eq!(cfg.idle_trigger_secs, 600, "the configured window is read");
    assert!(warnings.is_empty(), "{warnings:?}");
    fs::remove_file(&path).ok();
}

/// A window of zero would recap a session the instant the user stopped
/// typing, so it warns and falls back rather than being honoured.
#[test]
fn test_zero_window_falls_back() {
    let path = temp_settings(r#"{"recap": {"idle_trigger_secs": 0}}"#);
    let (cfg, warnings) = load_recap_from(&path);
    assert_eq!(
        cfg.idle_trigger_secs, DEFAULT_RECAP_IDLE_SECS,
        "zero is not a window, it is a missing one"
    );
    assert!(
        warnings
            .iter()
            .any(|w| w.field == "recap.idle_trigger_secs"),
        "the file is told which field was ignored: {warnings:?}"
    );
    fs::remove_file(&path).ok();
}

/// Writing the pair puts both fields in the settings file and leaves the
/// recap's model choice alone: a toggle that rewrote the model would silently
/// retarget every later recap.
#[test]
fn test_save_writes_both_fields() {
    let path = temp_settings(r#"{"recap": {"model": "small-1"}}"#);
    save_recap_to(&path, true, 600).expect("save");
    let (cfg, warnings) = load_recap_from(&path);
    assert!(cfg.enabled, "the switch is written");
    assert_eq!(cfg.idle_trigger_secs, 600, "the window is written");
    assert_eq!(
        cfg.model.as_deref(),
        Some("small-1"),
        "the model choice survives the write"
    );
    assert!(warnings.is_empty(), "{warnings:?}");
    fs::remove_file(&path).ok();
}

/// A settings file with no recap section gains one rather than the write being
/// dropped: the first flip on a fresh install is the common case.
#[test]
fn test_save_creates_section() {
    let path = temp_settings(r#"{"model": {"id": "m"}}"#);
    save_recap_to(&path, true, 300).expect("save");
    let (cfg, _) = load_recap_from(&path);
    assert!(cfg.enabled, "a fresh file gains the section");
    assert_eq!(cfg.idle_trigger_secs, 300);
    fs::remove_file(&path).ok();
}
