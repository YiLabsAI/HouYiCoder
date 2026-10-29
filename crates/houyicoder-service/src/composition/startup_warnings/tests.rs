//! Peer tests for the startup-warning merge.
//!
//! The merge is the last hop of every startup warning: a load produces a
//! ConfigWarning, this turns it into a line, and the runner queues it for the
//! transcript. A source that is produced but never merged is a silent
//! failure, and silence is exactly what a warning exists to prevent -- so
//! each source arm is pinned here rather than trusted to the call site.

use super::collect_startup_warnings;
use crate::sandbox_policy::{SandboxWarnings, network_policy_from};
use houyicoder_config::{ConfigWarning, NetworkMode, SandboxNetworkConfig};

fn warning(field: &str, reason: &str) -> ConfigWarning {
    ConfigWarning {
        field: field.into(),
        reason: reason.into(),
    }
}

/// The provider arm reaches the queue. This is the one the ignored
/// apiKeyHelper rides: a repository settings file naming a shell command is
/// dropped, and the only thing telling the user their pinned helper will
/// never run is this line. Dropping the arm would turn an intentional
/// feature contraction into an unexplained one.
#[test]
fn test_provider_warning_reaches_queue() {
    let provider = vec![warning(
        "apiKeyHelper",
        "a project settings file asked to run a command for your API key; ignored",
    )];
    let out = collect_startup_warnings(&SandboxWarnings::default(), &[], &[], &provider, &[]);
    assert_eq!(
        out,
        vec![
            "apiKeyHelper: a project settings file asked to run a command for your API key; ignored"
                .to_string()
        ],
        "the provider arm must reach the queue as field: reason"
    );
}

/// A warning from the real settings loader reaches the queue with one section
/// prefix: the producer leaves its strings bare, so a second prefix here would
/// print the section name twice.
#[test]
fn test_network_warning_prefixed_once() {
    let network = SandboxNetworkConfig {
        mode: NetworkMode::Unsupported("proxy".into()),
        ..SandboxNetworkConfig::default()
    };
    let (_, warnings) = network_policy_from(&network);
    let sandbox = SandboxWarnings {
        network: warnings,
        shell: Vec::new(),
    };
    let out = collect_startup_warnings(&sandbox, &[], &[], &[], &[]);
    assert_eq!(out.len(), 1, "got {out:?}");
    assert!(
        out[0].starts_with("sandbox.network: mode"),
        "the section prefix is added here: {out:?}"
    );
    assert!(
        !out[0].contains("sandbox.network: sandbox.network"),
        "the producer must not carry the prefix too: {out:?}"
    );
}

/// Every source arm reaches the queue, in load order. Asserting the whole
/// vector rather than five separate contains checks is what catches a
/// dropped arm: a per-arm check still passes when another arm is deleted.
#[test]
fn test_arms_merge_in_order() {
    let sandbox = SandboxWarnings {
        network: vec!["mode typo".to_string()],
        shell: vec!["no shell found".to_string()],
    };
    let out = collect_startup_warnings(
        &sandbox,
        &[warning("auto_memory", "expected a bool")],
        &[warning("effort", "unknown level")],
        &[warning("apiKeyHelper", "ignored")],
        &[warning("recap.model", "unknown model")],
    );
    assert_eq!(
        out,
        vec![
            "sandbox.network: mode typo".to_string(),
            "sandbox.shell: no shell found".to_string(),
            "auto_memory: expected a bool".to_string(),
            "effort: unknown level".to_string(),
            "apiKeyHelper: ignored".to_string(),
            "recap.model: unknown model".to_string(),
        ],
        "network, shell, then toggles, model-section, provider, recap"
    );
}

/// No warnings anywhere yields no lines: a clean startup must not open the
/// transcript with an empty system line.
#[test]
fn test_clean_startup_silent() {
    assert!(collect_startup_warnings(&SandboxWarnings::default(), &[], &[], &[], &[]).is_empty());
}
