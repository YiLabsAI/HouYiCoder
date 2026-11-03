//! Runner-level settings read from the merged settings file + env: the
//! max_turns cap. Mirrors the provider loader shape (three-layer merge of
//! user < project < local, then an env fallback, then the default) so a repo
//! can pin the cap and a malformed file never bricks the runner. Future
//! runner-level knobs land here.

use crate::settings_merge::{merge_json, read_settings_value};
use crate::{ConfigWarning, settings_path};
use serde_json::Value;
#[cfg(test)]
use std::env::temp_dir;
use std::env::var;
#[cfg(test)]
use std::fs::remove_file;
#[cfg(test)]
use std::fs::write;
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;
#[cfg(test)]
use std::process::id;
#[cfg(test)]
use std::sync::atomic::{AtomicU64, Ordering};

/// The per-user-turn tool-loop ceiling a session gets when nothing overrides
/// it. Large enough that a long multi-module task runs to its own end rather
/// than stopping to report half of it, and still finite: a loop that never
/// terminates must fail visibly, and reaching the limit returns the turn-limit
/// outcome the terminal reports instead of hanging.
const DEFAULT_MAX_TURNS: u32 = 1000;

/// How long a shell command may hold the foreground before the host takes it
/// over. Long enough that an ordinary build finishes in place, short enough
/// that a slow one gives the prompt back; the command keeps running either
/// way.
const DEFAULT_BASH_FOREGROUND_BUDGET_MS: u64 = 15_000;

/// A foreground delegation blocks the parent's turn by default. A caller that
/// wants a long delegation to release the turn sets this; zero keeps the
/// blocking behavior every existing session has.
const DEFAULT_DELEGATION_FOREGROUND_BUDGET_MS: u64 = 0;

/// Runner-level settings resolved from the merged settings file and env.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerSettings {
    /// The per-user-turn tool-loop ceiling. The settings file wins, then the
    /// env, then this default. 0 is illegal (it would stop every runner on
    /// its first model call) so it falls back.
    pub max_turns: u32,
    /// How long a bash call may hold the foreground, in milliseconds. The
    /// settings file wins, then the env, then the default. 0 disables the
    /// background move, so every command finishes in the foreground.
    pub bash_foreground_budget_ms: u64,
    /// How long a foreground delegation may hold the parent's turn, in
    /// milliseconds. The settings file wins, then the env, then the default.
    /// 0 keeps every delegation blocking, which is the default.
    pub delegation_foreground_budget_ms: u64,
}

impl Default for RunnerSettings {
    fn default() -> Self {
        Self {
            max_turns: DEFAULT_MAX_TURNS,
            bash_foreground_budget_ms: DEFAULT_BASH_FOREGROUND_BUDGET_MS,
            delegation_foreground_budget_ms: DEFAULT_DELEGATION_FOREGROUND_BUDGET_MS,
        }
    }
}

/// Load runner settings, layering project and local settings over the user
/// file when a workspace is given (user < project < local), then env, then
/// the default. A bad type warns and falls back rather than bricking.
pub fn load_runner_settings(workspace: Option<&Path>) -> (RunnerSettings, Vec<ConfigWarning>) {
    load_runner_settings_from(&settings_path(), workspace)
}

/// Path-explicit variant; testable without home mutation. The user path is
/// the base; project and local settings layer over it when a workspace is
/// given.
pub fn load_runner_settings_from(
    user: &Path,
    workspace: Option<&Path>,
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
        var(crate::ENV_HOUYICODER_MAX_TURNS)
            .ok()
            .and_then(|raw| parse_env_max_turns(&raw, &mut warnings))
    });
    let budget = parse_bash_budget(&settings, &mut warnings).or_else(|| {
        var(crate::ENV_HOUYICODER_BASH_FOREGROUND_BUDGET_MS)
            .ok()
            .and_then(|raw| parse_env_bash_budget(&raw, &mut warnings))
    });
    let delegation_budget = parse_delegation_budget(&settings, &mut warnings).or_else(|| {
        var(crate::ENV_HOUYICODER_DELEGATION_FOREGROUND_BUDGET_MS)
            .ok()
            .and_then(|raw| parse_env_delegation_budget(&raw, &mut warnings))
    });
    (
        RunnerSettings {
            max_turns: value.unwrap_or(DEFAULT_MAX_TURNS),
            bash_foreground_budget_ms: budget.unwrap_or(DEFAULT_BASH_FOREGROUND_BUDGET_MS),
            delegation_foreground_budget_ms: delegation_budget
                .unwrap_or(DEFAULT_DELEGATION_FOREGROUND_BUDGET_MS),
        },
        warnings,
    )
}

/// The foreground budget accepts zero: it is the way to turn the background move off,
/// and a caller that wants every command to finish in place asks for it.
fn parse_bash_budget(value: &Value, warnings: &mut Vec<ConfigWarning>) -> Option<u64> {
    match value.get("bash_foreground_budget_ms") {
        None | Some(Value::Null) => None,
        Some(Value::Number(n)) => n.as_u64().or_else(|| {
            warnings.push(ConfigWarning {
                field: "bash_foreground_budget_ms".into(),
                reason: format!(
                    "expected a non-negative integer, got {n} — ignored, so the env var or the default ({DEFAULT_BASH_FOREGROUND_BUDGET_MS}) applies"
                ),
            });
            None
        }),
        Some(other) => {
            warnings.push(ConfigWarning {
                field: "bash_foreground_budget_ms".into(),
                reason: format!(
                    "expected a number, got {} — ignored, so the env var or the default ({DEFAULT_BASH_FOREGROUND_BUDGET_MS}) applies",
                    crate::json_type_name(other)
                ),
            });
            None
        }
    }
}

fn parse_env_bash_budget(raw: &str, warnings: &mut Vec<ConfigWarning>) -> Option<u64> {
    match raw.parse() {
        Ok(value) => Some(value),
        Err(_) => {
            warnings.push(ConfigWarning {
                field: crate::ENV_HOUYICODER_BASH_FOREGROUND_BUDGET_MS.into(),
                reason: format!(
                    "expected a non-negative integer, got {raw:?} — using the default ({DEFAULT_BASH_FOREGROUND_BUDGET_MS})"
                ),
            });
            None
        }
    }
}

/// The delegation budget accepts zero: it keeps every foreground delegation
/// blocking the turn, which is the behavior without the setting.
fn parse_delegation_budget(value: &Value, warnings: &mut Vec<ConfigWarning>) -> Option<u64> {
    match value.get("delegation_foreground_budget_ms") {
        None | Some(Value::Null) => None,
        Some(Value::Number(n)) => n.as_u64().or_else(|| {
            warnings.push(ConfigWarning {
                field: "delegation_foreground_budget_ms".into(),
                reason: format!(
                    "expected a non-negative integer, got {n} — ignored, so the env var or the default ({DEFAULT_DELEGATION_FOREGROUND_BUDGET_MS}) applies"
                ),
            });
            None
        }),
        Some(other) => {
            warnings.push(ConfigWarning {
                field: "delegation_foreground_budget_ms".into(),
                reason: format!(
                    "expected a number, got {} — ignored, so the env var or the default ({DEFAULT_DELEGATION_FOREGROUND_BUDGET_MS}) applies",
                    crate::json_type_name(other)
                ),
            });
            None
        }
    }
}

fn parse_env_delegation_budget(raw: &str, warnings: &mut Vec<ConfigWarning>) -> Option<u64> {
    match raw.parse() {
        Ok(value) => Some(value),
        Err(_) => {
            warnings.push(ConfigWarning {
                field: crate::ENV_HOUYICODER_DELEGATION_FOREGROUND_BUDGET_MS.into(),
                reason: format!(
                    "expected a non-negative integer, got {raw:?} — using the default ({DEFAULT_DELEGATION_FOREGROUND_BUDGET_MS})"
                ),
            });
            None
        }
    }
}

fn parse_max_turns(value: &Value, warnings: &mut Vec<ConfigWarning>) -> Option<u32> {
    match value.get("max_turns") {
        None | Some(Value::Null) => None,
        Some(Value::Number(n)) => n
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
mod tests;
