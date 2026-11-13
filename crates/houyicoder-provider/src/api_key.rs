//! Turning a configured api key source into a key. The config layer names the
//! source; obtaining it runs a command, which belongs here where the spawn
//! chokepoint is reachable. A keychain item is read by the platform module,
//! which owns the store and the encoding each host writes.

use futures::executor::block_on;
use houyicoder_api::launcher::{
    FenceConfig, ProcessLauncher, SpawnPolicy, SpawnRequest, StdProcessLauncher,
};
use houyicoder_config::{ApiKeySource, ConfigWarning};
use houyicoder_platform::host::helper_program;
use houyicoder_platform::host::secrets::{SecretRead, read_secret};
use houyicoder_platform::host::shell::{HelperShell, ShellProgram};

/// How long a key helper may run before it is killed. Wide enough for one that
/// reaches a password manager over the network, finite because startup blocks
/// on it.
const HELPER_TIMEOUT_MS: u64 = 10_000;

/// A resolved key together with where it came from.
pub(crate) struct ResolvedKey {
    /// The key, or None when neither the configured source nor the environment
    /// supplied one.
    pub key: Option<String>,
    /// A label naming the origin, for the status view, or None when nothing
    /// supplied a key. A configured source that failed reports the environment
    /// instead, because that is where the caller's key actually came from.
    pub origin: Option<String>,
    pub warnings: Vec<ConfigWarning>,
}

/// Resolve the api key: the configured source first, then the environment.
/// A None launcher uses the default one, so only a test has to supply it.
///
/// The environment key is a parameter rather than a read: the workspace denies
/// unsafe, so a test cannot set the variable, and injecting it is the only way
/// to exercise both branches of the fallback.
///
/// Returns the warnings resolution produced alongside the key. A source that
/// was configured and did not yield one has to say so: without it the user
/// sees stub mode with no hint that their keychain entry is the reason.
///
/// The origin is reported from this one resolution rather than recomputed by
/// the caller: a second pass over the same inputs could disagree with what was
/// actually returned, which is the whole reason the status line shows it.
pub(crate) fn resolve_api_key(
    source: Option<&ApiKeySource>,
    env_key: Option<(&'static str, String)>,
    launcher: Option<&dyn ProcessLauncher>,
) -> ResolvedKey {
    let mut warnings = Vec::new();
    let from_source = source.and_then(|s| match s {
        ApiKeySource::Helper { command, shell } => {
            run_helper(command, *shell, launcher, &mut warnings)
        }
        ApiKeySource::UnknownHelperShell { named, .. } => {
            // The command was written for an interpreter the settings did not
            // name recognizably, so it never runs: a different interpreter
            // could give the same text a different meaning. The warning says
            // the helper was refused, and the environment answers instead.
            tracing::warn!(
                "api key helper shell {named} is not recognized; not running the helper"
            );
            warnings.push(ConfigWarning {
                field: "apiKeyHelperShell".into(),
                reason: format!(
                    "the named shell {named} is not recognized, so the helper is not run; \
                     the environment key is used instead"
                ),
            });
            None
        }
        ApiKeySource::Keychain { service, account } => keychain_key(
            service,
            account,
            read_secret(service, account),
            &mut warnings,
        ),
    });
    let (key, env_name) = match from_source {
        Some(key) => (Some(key), None),
        None => match env_key {
            Some((name, key)) => (Some(key), Some(name)),
            None => (None, None),
        },
    };
    let origin = key_origin(key.is_some(), source, env_name);
    ResolvedKey {
        key,
        origin,
        warnings,
    }
}

/// The origin label for a resolved key. Pure over the three facts, so every
/// branch is testable without touching the process environment.
///
/// key_resolved is what makes the label truthful: a configured source that
/// yielded nothing leaves the key absent, and naming that source would report a
/// credential the caller does not have — the case a keychain on a platform that
/// cannot read one lands in.
///
/// env_name is Some only where the environment supplied the key, which outranks
/// a configured source: a source that yielded nothing left the environment to
/// answer, and the environment is what the caller got.
fn key_origin(
    key_resolved: bool,
    source: Option<&ApiKeySource>,
    env_name: Option<&str>,
) -> Option<String> {
    if !key_resolved {
        return None;
    }
    match env_name {
        Some(name) => Some(format!("env {name}")),
        None => source.map(ApiKeySource::display_label),
    }
}

/// The key a store read yields, or the notice recorded in its place.
///
/// The read is a parameter so every outcome is exercised without a system
/// store. A read that yields nothing has to say why: without the notice the
/// user sees stub mode with no hint that their keychain entry is the reason.
fn keychain_key(
    service: &str,
    account: &str,
    read: SecretRead,
    warnings: &mut Vec<ConfigWarning>,
) -> Option<String> {
    let reason = match read {
        SecretRead::Value(key) => return Some(key),
        SecretRead::NotFound => "the item is missing or access was denied",
        SecretRead::InvalidValue => "the item holds a value that is not a usable key",
        SecretRead::UnsupportedPlatform => "this platform has no keychain",
        SecretRead::Unavailable => "the keychain could not be read",
    };
    warnings.push(ConfigWarning {
        field: "keychain".into(),
        reason: format!(
            "no key from keychain service {service} account {account} \
             ({reason}); falling back to the environment"
        ),
    });
    None
}

/// Run a helper command and take its stdout as the key. A spawn failure, a
/// non-zero exit, a timeout kill, and blank output all mean no key.
fn run_helper(
    command: &str,
    shell: Option<HelperShell>,
    launcher: Option<&dyn ProcessLauncher>,
    warnings: &mut Vec<ConfigWarning>,
) -> Option<String> {
    // The command may hold the key inline, so the notice names the field and
    // the failure class only — never the command text.
    let failed = |warnings: &mut Vec<ConfigWarning>, detail: &str| {
        warnings.push(ConfigWarning {
            field: "apiKeyHelper".into(),
            reason: format!(
                "no key from the configured helper ({detail}); falling back to the environment"
            ),
        });
        None
    };
    // A None shell is the default a helper command was written for: the POSIX
    // shell. An explicit shell names the interpreter the command asks for, and
    // one the host cannot run is reported rather than silently replaced.
    let program = match shell {
        None => ShellProgram::Posix,
        Some(helper) => match helper_program(helper) {
            Some(program) => program,
            None => return failed(warnings, "the declared shell is not available on this host"),
        },
    };
    let default_launcher = StdProcessLauncher::new();
    let launcher: &dyn ProcessLauncher = launcher.unwrap_or(&default_launcher);
    let req = SpawnRequest::new(program.executable().to_string_lossy().into_owned())
        .with_args(program.arguments(command))
        .piped_output();
    let policy = SpawnPolicy::default().with_fence(FenceConfig {
        wall_timeout_ms: HELPER_TIMEOUT_MS,
        ..FenceConfig::default()
    });
    tracing::debug!("resolving the api key through the configured helper");
    let child = match launcher.spawn(req, policy) {
        Ok(child) => child,
        Err(e) => {
            tracing::warn!("api key helper could not start: {e}");
            return failed(warnings, "the command could not start");
        }
    };
    // The capture path resolves its wait future before handing the child back,
    // so this is already settled; the fence above is what bounded it.
    let exit = match block_on(child.wait()) {
        Ok(exit) => exit,
        Err(e) => {
            tracing::warn!("api key helper did not finish: {e}");
            return failed(warnings, "the command did not finish");
        }
    };
    if exit.exit_code != Some(0) {
        tracing::warn!(
            "api key helper exited with {:?}; falling back to the environment",
            exit.exit_code
        );
        return failed(warnings, "the command exited non-zero");
    }
    let key = exit.stdout.unwrap_or_default().trim().to_string();
    if key.is_empty() {
        tracing::warn!("api key helper produced no output; falling back to the environment");
        return failed(warnings, "the command produced no output");
    }
    Some(key)
}

#[cfg(test)]
mod tests;
