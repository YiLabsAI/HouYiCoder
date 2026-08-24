//! Turning a configured api key source into a key. The config layer names the
//! source; obtaining it runs a command, which belongs here where the spawn
//! chokepoint is reachable.
//!
//! The argv audit stays off for this spawn: a user may write the key inline in
//! the helper command, and the audit line would copy it into a log. A redacted
//! line records the resolution instead.

use houyicoder_api::launcher::{
    FenceConfig, ProcessLauncher, SpawnPolicy, SpawnRequest, StdProcessLauncher,
};
use houyicoder_config::ApiKeySource;

/// How long a key helper may run before it is killed. Wide enough for one that
/// reaches a password manager over the network, finite because startup blocks
/// on it.
const HELPER_TIMEOUT_MS: u64 = 10_000;

/// Resolve the api key: the configured source first, then the environment.
/// A None launcher uses the default one, so only a test has to supply it.
pub(crate) fn resolve_api_key(
    source: Option<&ApiKeySource>,
    launcher: Option<&dyn ProcessLauncher>,
) -> Option<String> {
    let from_source = source.and_then(|s| match s {
        ApiKeySource::Helper(command) => run_helper(command, launcher),
    });
    from_source.or_else(houyicoder_config::api_key_from_env)
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
