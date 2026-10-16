//! Where the api key comes from, as data. The settings file names a source
//! rather than holding the key, so a model switch rewriting settings.json
//! never puts a secret through the atomic temp file.
//!
//! Naming the source is all this does. Obtaining it is a spawn, which needs
//! the chokepoint policy from a layer this leaf cannot reach, so the
//! composition root resolves it.

/// A configured source for the api key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiKeySource {
    /// An operating-system keychain item, addressed by service and account.
    Keychain { service: String, account: String },
    /// A shell command whose stdout is the key.
    Helper(String),
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
            Self::Helper(_) => "helper".to_string(),
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
/// preference to honor.
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
    Some(ApiKeySource::Helper(helper.to_string()))
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
