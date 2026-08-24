//! Turning a configured api key source into a key. The config layer names the
//! source; obtaining it runs a command, which belongs here where the spawn
//! chokepoint is reachable.
//!
//! The two sources differ on the argv audit. A helper command may hold the key
//! inline, so auditing argv would copy the secret into a log; a redacted line
//! records that resolution instead. A keychain lookup carries only the item's
//! service and account in argv, and reading a user's keychain is precisely the
//! event an operator should be able to see, so that spawn is audited.

use houyicoder_api::launcher::{
    FenceConfig, ProcessLauncher, SpawnPolicy, SpawnRequest, StdProcessLauncher,
};
use houyicoder_config::{ApiKeySource, ConfigWarning};

/// How long a key helper may run before it is killed. Wide enough for one that
/// reaches a password manager over the network, finite because startup blocks
/// on it.
const HELPER_TIMEOUT_MS: u64 = 10_000;

/// How long a keychain lookup may run. Longer than the helper because the
/// keychain can put an authorization dialog in front of a human, and still
/// finite because a session with no one watching would otherwise wait forever.
const KEYCHAIN_TIMEOUT_MS: u64 = 20_000;

/// A dialog waiting on a human needs more room than a script, so the ordering
/// of the two budgets is an invariant rather than a coincidence.
const _: () = assert!(KEYCHAIN_TIMEOUT_MS > HELPER_TIMEOUT_MS);

/// Absolute path, never a bare program name: resolving this through PATH would
/// let anything earlier on it answer for the keychain.
const SECURITY_TOOL: &str = "/usr/bin/security";

/// Resolve the api key: the configured source first, then the environment.
/// A None launcher uses the default one, so only a test has to supply it.
///
/// Returns the warnings resolution produced alongside the key. A source that
/// was configured and did not yield one has to say so: without it the user
/// sees stub mode with no hint that their keychain entry is the reason.
pub(crate) fn resolve_api_key(
    source: Option<&ApiKeySource>,
    launcher: Option<&dyn ProcessLauncher>,
) -> (Option<String>, Vec<ConfigWarning>) {
    let mut warnings = Vec::new();
    let from_source = source.and_then(|s| match s {
        ApiKeySource::Helper(command) => run_helper(command, launcher),
        ApiKeySource::Keychain { service, account } => {
            read_keychain(service, account, launcher, &mut warnings)
        }
    });
    let key = from_source.or_else(houyicoder_config::api_key_from_env);
    (key, warnings)
}

/// Read a keychain item's password. macOS only: the lookup shells out to the
/// system keychain tool, and no other platform has an equivalent this addresses
/// the same way. Elsewhere the entry is reported and skipped rather than
/// ignored, so a shared settings file does not look broken on the platform that
/// cannot honor it.
fn read_keychain(
    service: &str,
    account: &str,
    launcher: Option<&dyn ProcessLauncher>,
    warnings: &mut Vec<ConfigWarning>,
) -> Option<String> {
    if !cfg!(target_os = "macos") {
        warnings.push(ConfigWarning {
            field: "keychain".into(),
            reason: "keychain lookup is macOS only; falling back to the \
                     environment on this platform"
                .into(),
        });
        return None;
    }
    let default_launcher = StdProcessLauncher::new();
    let launcher: &dyn ProcessLauncher = launcher.unwrap_or(&default_launcher);
    let req = SpawnRequest::new(SECURITY_TOOL)
        .with_args(["find-generic-password", "-s", service, "-a", account, "-w"])
        .piped_output();
    let policy = SpawnPolicy::default().audited().with_fence(FenceConfig {
        wall_timeout_ms: KEYCHAIN_TIMEOUT_MS,
        ..FenceConfig::default()
    });
    let failed = |warnings: &mut Vec<ConfigWarning>, detail: String| {
        warnings.push(ConfigWarning {
            field: "keychain".into(),
            reason: format!(
                "no key from keychain service {service} account {account} \
                 ({detail}); falling back to the environment"
            ),
        });
        None
    };
    let child = match launcher.spawn(req, policy) {
        Ok(child) => child,
        Err(e) => return failed(warnings, e.to_string()),
    };
    let exit = match futures::executor::block_on(child.wait()) {
        Ok(exit) => exit,
        Err(e) => return failed(warnings, e.to_string()),
    };
    if exit.exit_code != Some(0) {
        // The tool reports a missing item and a denied authorization the same
        // way, with a non-zero exit, so the notice names neither.
        return failed(warnings, "item missing or access denied".into());
    }
    let key = exit.stdout.unwrap_or_default().trim().to_string();
    if key.is_empty() {
        return failed(warnings, "the item holds no password".into());
    }
    Some(key)
}

/// Run a helper command and take its stdout as the key. A spawn failure, a
/// non-zero exit, a timeout kill, and blank output all mean no key.
fn run_helper(command: &str, launcher: Option<&dyn ProcessLauncher>) -> Option<String> {
    let default_launcher = StdProcessLauncher::new();
    let launcher: &dyn ProcessLauncher = launcher.unwrap_or(&default_launcher);
    // A shell, not a split argv: the field exists so a user can write a
    // pipeline. Running an arbitrary string is acceptable only because the
    // merge refuses a repository-supplied value.
    let req = SpawnRequest::new("sh")
        .with_args(["-c", command])
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
            return None;
        }
    };
    // The capture path resolves its wait future before handing the child back,
    // so this is already settled; the fence above is what bounded it.
    let exit = match futures::executor::block_on(child.wait()) {
        Ok(exit) => exit,
        Err(e) => {
            tracing::warn!("api key helper did not finish: {e}");
            return None;
        }
    };
    if exit.exit_code != Some(0) {
        tracing::warn!(
            "api key helper exited with {:?}; falling back to the environment",
            exit.exit_code
        );
        return None;
    }
    let key = exit.stdout.unwrap_or_default().trim().to_string();
    if key.is_empty() {
        tracing::warn!("api key helper produced no output; falling back to the environment");
        return None;
    }
    Some(key)
}

#[cfg(test)]
#[path = "api_key_tests.rs"]
mod api_key_tests;
