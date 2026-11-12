//! Where the api key comes from, as data. The settings file names a source
//! rather than holding the key, so a model switch rewriting settings.json
//! never puts a secret through the atomic temp file.
//!
//! Naming the source is all this does. Obtaining it is a spawn, which needs
//! the chokepoint policy from a layer this leaf cannot reach, so the
//! composition root resolves it.

use houyicoder_platform::host::shell::HelperShell;

/// A configured source for the api key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiKeySource {
    /// An operating-system keychain item, addressed by service and account.
    Keychain { service: String, account: String },
    /// A shell command whose stdout is the key, and the interpreter it runs
    /// through. A None interpreter is the default POSIX shell a helper command
    /// was written for. An unknown name in settings yields no source at all,
    /// so the command is never handed to a different interpreter.
    Helper {
        command: String,
        shell: Option<HelperShell>,
    },
    /// A helper whose interpreter name was not recognized. Carries the text of
    /// the failed match so the warning can name it, and never runs: the command
    /// was written for one interpreter, and running it through another could
    /// turn a benign line into something else entirely.
    UnknownHelperShell { command: String, named: String },
}

impl ApiKeySource {
    /// A label naming where the key comes from, for the status view.
    ///
    /// A keychain item is named by its coordinates: that is what tells two
    /// accounts apart, and the coordinates are not the secret. A helper is
    /// named by its variant alone — its command is an arbitrary shell string
    /// that may hold the key inline, so echoing any part of it risks printing
    /// the very thing this label exists to avoid.
    pub fn display_label(&self) -> String {
        match self {
            Self::Keychain { service, account } => format!("keychain {service}/{account}"),
            Self::Helper { .. } | Self::UnknownHelperShell { .. } => "helper".to_string(),
        }
    }
}

/// Read the api key source out of a parsed settings value. Callers hand in the
/// user settings value only: both fields hand control of the key to whoever
/// wrote them.
///
/// The keychain wins over the helper when both are set: it is the source that
/// keeps the key out of any file, so a user who configured it meant it, and
/// picking one here means the loser is never even attempted.
///
/// A field present but unusable warns rather than reading as absent -- a
/// keychain entry missing its account is a mistake to report, not a
/// preference to honor. An unrecognized shell name is the same class of
/// mistake, but it also refuses the source: a command written for one
/// interpreter must not run through another, so the environment becomes the
/// fallback and the warning names the refusal.
pub(crate) fn key_source_from_value(
    v: &serde_json::Value,
    warnings: &mut Vec<crate::ConfigWarning>,
) -> Option<ApiKeySource> {
    if let Some(entry) = v.get("keychain") {
        match keychain_source(entry) {
            Ok(source) => return Some(source),
            Err(reason) => warnings.push(crate::ConfigWarning {
                field: "keychain".into(),
                reason,
            }),
        }
    }
    let helper = v.get("apiKeyHelper")?.as_str()?.trim();
    if helper.is_empty() {
        return None;
    }
    match v.get("apiKeyHelperShell").and_then(|s| s.as_str()) {
        None => Some(ApiKeySource::Helper {
            command: helper.to_string(),
            shell: None,
        }),
        Some(value) => match parse_helper_shell(value) {
            Some(shell) => Some(ApiKeySource::Helper {
                command: helper.to_string(),
                shell: Some(shell),
            }),
            None => {
                warnings.push(crate::ConfigWarning {
                    field: "apiKeyHelperShell".into(),
                    reason: format!(
                        "the value {value} is not a recognized shell, so the configured \
                         helper is not run; the environment key is used instead"
                    ),
                });
                Some(ApiKeySource::UnknownHelperShell {
                    command: helper.to_string(),
                    named: value.to_string(),
                })
            }
        },
    }
}

/// Parse a helper shell name. The recognized values name the interpreters a
/// helper command can ask for by name.
fn parse_helper_shell(value: &str) -> Option<HelperShell> {
    match value.trim() {
        "sh" => Some(HelperShell::Sh),
        "git_bash" => Some(HelperShell::GitBash),
        "cmd" => Some(HelperShell::Cmd),
        "powershell" => Some(HelperShell::PowerShell),
        _ => None,
    }
}

/// Build a keychain source from the settings entry, or say what is wrong with
/// it. Both fields are required: a lookup by service alone would return
/// whichever account the keychain happens to answer with, so an entry that
/// omits one is ambiguous rather than convenient.
fn keychain_source(entry: &serde_json::Value) -> Result<ApiKeySource, String> {
    let field = |name: &str| {
        entry
            .get(name)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
    };
    match (field("service"), field("account")) {
        (Some(service), Some(account)) => Ok(ApiKeySource::Keychain {
            service: service.to_string(),
            account: account.to_string(),
        }),
        (None, Some(_)) => Err("keychain needs a service; ignored".into()),
        (Some(_), None) => Err("keychain needs an account; ignored".into()),
        (None, None) => {
            Err("keychain needs a service and an account as non-empty strings; ignored".into())
        }
    }
}

#[cfg(test)]
mod tests;
