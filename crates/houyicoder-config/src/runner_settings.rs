//! Runner-level settings read from the merged settings file + env: the
//! max_turns cap. Mirrors the provider loader shape (three-layer merge of
//! user < project < local, then an env fallback, then the default) so a repo
//! can pin the cap and a malformed file never bricks the runner. Future
//! runner-level knobs land here.

use crate::settings_merge::{merge_json, read_settings_value};
use crate::{ConfigWarning, settings_path};

const DEFAULT_MAX_TURNS: u32 = 200;

/// Runner-level settings resolved from the merged settings file and env.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerSettings {
    /// The per-user-turn tool-loop ceiling. The settings file wins, then the
    /// env, then this default. 0 is illegal (it would stop every runner on
    /// its first model call) so it falls back.
    pub max_turns: u32,
}

impl Default for RunnerSettings {
    fn default() -> Self {
        Self {
            max_turns: DEFAULT_MAX_TURNS,
        }
    }
}

/// Load runner settings, layering project and local settings over the user
/// file when a workspace is given (user < project < local), then env, then
/// the default. A bad type warns and falls back rather than bricking.
pub fn load_runner_settings(
    workspace: Option<&std::path::Path>,
) -> (RunnerSettings, Vec<ConfigWarning>) {
    load_runner_settings_from(&settings_path(), workspace)
}

/// Path-explicit variant; testable without home mutation. The user path is
/// the base; project and local settings layer over it when a workspace is
/// given.
pub fn load_runner_settings_from(
    user: &std::path::Path,
    workspace: Option<&std::path::Path>,
) -> (RunnerSettings, Vec<ConfigWarning>) {
    let mut settings = read_settings_value(user);
    if let Some(ws) = workspace {
        let project = ws.join(".houyicoder").join("settings.json");
        let local = ws.join(".houyicoder").join("settings.local.json");
        settings = merge_json(settings, read_settings_value(&project));
        settings = merge_json(settings, read_settings_value(&local));
    }
    let mut warnings = Vec::new();
    let from_settings = parse_max_turns(&settings, &mut warnings);
    let value = from_settings.or_else(|| {
        std::env::var(crate::ENV_HOUYICODER_MAX_TURNS)
            .ok()
            .and_then(|raw| parse_env_max_turns(&raw, &mut warnings))
    });
    (
        RunnerSettings {
            max_turns: value.unwrap_or(DEFAULT_MAX_TURNS),
        },
        warnings,
    )
}

fn parse_max_turns(value: &serde_json::Value, warnings: &mut Vec<ConfigWarning>) -> Option<u32> {
    match value.get("max_turns") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::Number(n)) => n
            .as_u64()
            .and_then(|v| u32::try_from(v).ok())
            .filter(|v| *v > 0)
            .or_else(|| {
                warnings.push(ConfigWarning {
                    field: "max_turns".into(),
                    reason: format!(
                        "expected a positive integer, got {n} — using the default ({DEFAULT_MAX_TURNS})"
                    ),
                });
                None
            }),
        Some(other) => {
            warnings.push(ConfigWarning {
                field: "max_turns".into(),
                reason: format!(
                    "expected a number, got {} — using the default ({DEFAULT_MAX_TURNS})",
                    crate::json_type_name(other)
                ),
            });
            None
        }
    }
}

fn parse_env_max_turns(raw: &str, warnings: &mut Vec<ConfigWarning>) -> Option<u32> {
    match raw.parse().ok().filter(|v: &u32| *v > 0) {
        Some(v) => Some(v),
        None => {
            warnings.push(ConfigWarning {
                field: crate::ENV_HOUYICODER_MAX_TURNS.into(),
                reason: format!(
                    "expected a positive integer, got {raw:?} — using the default ({DEFAULT_MAX_TURNS})"
                ),
            });
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_settings(content: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let p =
            std::env::temp_dir().join(format!("houyi-runner-{seq}-{}.json", std::process::id()));
        std::fs::write(&p, content).expect("write");
        p
    }

    #[test]
    fn test_settings_value_wins() {
        let p = temp_settings(r#"{"max_turns": 7}"#);
        let (s, w) = load_runner_settings_from(&p, None);
        std::fs::remove_file(&p).ok();
        assert_eq!(s.max_turns, 7);
        assert!(w.is_empty());
    }

    #[test]
    fn test_missing_uses_default() {
        let p = temp_settings(r#"{"auto_memory": true}"#);
        let (s, w) = load_runner_settings_from(&p, None);
        std::fs::remove_file(&p).ok();
        assert_eq!(s.max_turns, DEFAULT_MAX_TURNS);
        assert!(w.is_empty(), "a missing field is not a warning");
    }

    #[test]
    fn test_bad_type_warns() {
        let p = temp_settings(r#"{"max_turns": "two"}"#);
        let (s, w) = load_runner_settings_from(&p, None);
        std::fs::remove_file(&p).ok();
        assert_eq!(s.max_turns, DEFAULT_MAX_TURNS);
        assert_eq!(w.len(), 1);
    }

    #[test]
    fn test_zero_rejected() {
        let p = temp_settings(r#"{"max_turns": 0}"#);
        let (s, w) = load_runner_settings_from(&p, None);
        std::fs::remove_file(&p).ok();
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
}
