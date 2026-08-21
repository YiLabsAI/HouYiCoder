//! Session retention settings (session_retention_days,
//! session_retention_count), read from settings.json. Mirrors the
//! MemoryToggles load shape: a missing file yields defaults with no
//! warnings, a corrupt file yields defaults plus one warning, a valid file
//! with one bad field yields that field's default plus a warning while the
//! other field reads normally - one typo does not reset the whole file.
//!
//! 0 on either field is a legal opt-out (no TTL prune / no count cap), not
//! a "disable persistence" switch - persistence and retention are separate
//! concerns, and one value must not carry both meanings; the destructive one
//! (delete everything) least of all.

use crate::{ConfigWarning, json_type_name, settings_path};

/// How long to keep sessions, and how many to keep at minimum. Both default
/// to bounded values so a session store is bounded out of the box; the TTL
/// is the main rule, the count is the hard floor that does not rely on the
/// usage rate staying low.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetentionConfig {
    /// Sessions older than this many days by last-active are pruned. 0 =
    /// no TTL prune (the count cap alone bounds the store).
    pub session_retention_days: u32,
    /// Remove the oldest sessions past this count. 0 = no count cap.
    pub session_retention_count: u32,
}

const DEFAULT_DAYS: u32 = 30;
const DEFAULT_COUNT: u32 = 1000;

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            session_retention_days: DEFAULT_DAYS,
            session_retention_count: DEFAULT_COUNT,
        }
    }
}

/// Load retention settings from the settings file. A missing or corrupt
/// file yields defaults - retention is advisory, never a hard gate that
/// bricks the session on a malformed file.
pub fn load_retention() -> (RetentionConfig, Vec<ConfigWarning>) {
    load_retention_from(&settings_path())
}

/// Pure loader against an explicit path; testable without env mutation.
pub fn load_retention_from(path: &std::path::Path) -> (RetentionConfig, Vec<ConfigWarning>) {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(_) => return (RetentionConfig::default(), Vec::new()),
    };
    let value: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(_) => {
            return (
                RetentionConfig::default(),
                vec![ConfigWarning {
                    field: "<file>".into(),
                    reason: "settings.json is not valid JSON; using defaults".into(),
                }],
            );
        }
    };
    let mut warnings = Vec::new();
    let session_retention_days = extract_u32_field(
        &value,
        "session_retention_days",
        DEFAULT_DAYS,
        &mut warnings,
    );
    let session_retention_count = extract_u32_field(
        &value,
        "session_retention_count",
        DEFAULT_COUNT,
        &mut warnings,
    );
    (
        RetentionConfig {
            session_retention_days,
            session_retention_count,
        },
        warnings,
    )
}

/// Read a u32 field from a settings JSON object. Missing or null yields the
/// default (no warning); a non-number yields the default plus a warning so
/// the typo is surfaced, not silently swallowed.
fn extract_u32_field(
    value: &serde_json::Value,
    field: &str,
    default: u32,
    warnings: &mut Vec<ConfigWarning>,
) -> u32 {
    match value.get(field) {
        None | Some(serde_json::Value::Null) => default,
        Some(serde_json::Value::Number(n)) => n
            .as_u64()
            .and_then(|v| u32::try_from(v).ok())
            .unwrap_or_else(|| {
                warnings.push(ConfigWarning {
                    field: field.to_string(),
                    reason: format!(
                        "expected a non-negative integer, got {n}; using the default ({default})"
                    ),
                });
                default
            }),
        Some(other) => {
            warnings.push(ConfigWarning {
                field: field.to_string(),
                reason: format!(
                    "expected a number, got {}; using the default ({default})",
                    json_type_name(other)
                ),
            });
            default
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn temp_settings(content: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("houyi-ret-{seq}-{}.json", std::process::id()));
        fs::write(&p, content).expect("write");
        p
    }

    #[test]
    fn test_load_reads_both_fields() {
        let p = temp_settings(r#"{"session_retention_days": 14, "session_retention_count": 200}"#);
        let (cfg, w) = load_retention_from(&p);
        assert_eq!(cfg.session_retention_days, 14);
        assert_eq!(cfg.session_retention_count, 200);
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
        let p =
            temp_settings(r#"{"session_retention_days": "two", "session_retention_count": 200}"#);
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
}
