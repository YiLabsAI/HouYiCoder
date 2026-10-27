//! Recap settings (recap.enabled, recap.model), read from settings.json.
//!
//! A recap spends a model request while the user is away, so the section is
//! off unless the file switches it on. The model must be named: a catalog
//! entry able to run in Fast mode says how the user wants that model to
//! respond, not what it costs, so it is not a recap model, and the session's
//! main model is never a fallback either.

use std::fs::read_to_string;
use std::path::Path;

use serde_json::Value;

use crate::ConfigWarning;
use crate::json_type_name;

/// Whether the host may summarize an away session, and which catalog entry
/// the request may use.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecapConfig {
    /// Whether the host may generate a recap. False keeps every trigger
    /// path inert, which is the default for a file that never names it.
    pub enabled: bool,
    /// The model id the recap request names. None leaves the recap off: no
    /// catalog fact stands in for a model the user did not choose.
    pub model: Option<String>,
}

/// Pure loader against an explicit path; testable without env mutation.
pub fn load_recap_from(path: &Path) -> (RecapConfig, Vec<ConfigWarning>) {
    let text = match read_to_string(path) {
        Ok(text) => text,
        Err(_) => return (RecapConfig::default(), Vec::new()),
    };
    let value: Value = match serde_json::from_str(&text) {
        Ok(value) => value,
        Err(_) => {
            return (
                RecapConfig::default(),
                vec![ConfigWarning {
                    field: "<file>".into(),
                    reason: "settings.json is not valid JSON — the recap stays off".into(),
                }],
            );
        }
    };
    let section = match value.get("recap") {
        None | Some(Value::Null) => return (RecapConfig::default(), Vec::new()),
        Some(section) if section.is_object() => section,
        Some(other) => {
            return (
                RecapConfig::default(),
                vec![ConfigWarning {
                    field: "recap".into(),
                    reason: format!(
                        "expected an object, got {} — the recap stays off",
                        json_type_name(other)
                    ),
                }],
            );
        }
    };
    let mut warnings = Vec::new();
    let enabled = extract_enabled(section, &mut warnings);
    let model = extract_model(section, &mut warnings);
    if enabled && model.is_none() && !warnings.iter().any(|w| w.field == "recap.model") {
        // Enabling the feature and naming no model leaves it inert. Say so
        // once at startup: the user asked for recaps and would otherwise
        // wait for a summary that never comes. A malformed value already
        // warned about this field, so it is not reported twice.
        warnings.push(ConfigWarning {
            field: "recap.model".into(),
            reason: "the recap is enabled but names no model — it stays off until one is set"
                .into(),
        });
    }
    (RecapConfig { enabled, model }, warnings)
}

/// Read the switch. Off unless the file says a JSON true.
fn extract_enabled(section: &Value, warnings: &mut Vec<ConfigWarning>) -> bool {
    match section.get("enabled") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(enabled)) => *enabled,
        Some(other) => {
            warnings.push(ConfigWarning {
                field: "recap.enabled".into(),
                reason: format!(
                    "expected a boolean, got {} — the recap stays off",
                    json_type_name(other)
                ),
            });
            false
        }
    }
}

/// Read the model id. A malformed value is dropped rather than guessed: the
/// recap then stays off instead of spending on a model the user did not name.
/// The id is trimmed, so a stray space cannot turn a named catalog entry into
/// an unknown model at the request.
fn extract_model(section: &Value, warnings: &mut Vec<ConfigWarning>) -> Option<String> {
    match section.get("model") {
        None | Some(Value::Null) => None,
        Some(Value::String(model)) if !model.trim().is_empty() => Some(model.trim().to_string()),
        Some(Value::String(_)) => {
            warnings.push(ConfigWarning {
                field: "recap.model".into(),
                reason: "the value is empty — the recap stays off until a model is set".into(),
            });
            None
        }
        Some(other) => {
            warnings.push(ConfigWarning {
                field: "recap.model".into(),
                reason: format!(
                    "expected a string, got {} — the recap stays off until a model is set",
                    json_type_name(other)
                ),
            });
            None
        }
    }
}

#[cfg(test)]
mod tests;
