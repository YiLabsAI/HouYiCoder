//! Peer tests for the startup-warning merge.
//!
//! The merge is the last hop of every startup warning: a load produces a
//! ConfigWarning, this turns it into a line, and the runner queues it for the
//! transcript. A source that is produced but never merged is a silent
//! failure, and silence is exactly what a warning exists to prevent -- so
//! each source arm is pinned here rather than trusted to the call site.

use super::collect_startup_warnings;
use houyicoder_config::ConfigWarning;

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
    let out = collect_startup_warnings(&[], &[], &[], &provider);
    assert_eq!(
        out,
        vec![
            "apiKeyHelper: a project settings file asked to run a command for your API key; ignored"
                .to_string()
        ],
        "the provider arm must reach the queue as field: reason"
    );
}

/// Every source arm reaches the queue, in load order. Asserting the whole
/// vector rather than four separate contains checks is what catches a
/// dropped arm: a per-arm check still passes when another arm is deleted.
#[test]
fn test_arms_merge_in_order() {
    let out = collect_startup_warnings(
        &["mode typo".to_string()],
        &[warning("auto_memory", "expected a bool")],
        &[warning("effort", "unknown level")],
        &[warning("apiKeyHelper", "ignored")],
    );
    assert_eq!(
        out,
        vec![
            "sandbox.network: mode typo".to_string(),
            "auto_memory: expected a bool".to_string(),
            "effort: unknown level".to_string(),
            "apiKeyHelper: ignored".to_string(),
        ],
        "network, then toggles, then model-section, then provider"
    );
}

/// No warnings anywhere yields no lines: a clean startup must not open the
/// transcript with an empty system line.
#[test]
fn test_clean_startup_silent() {
    assert!(collect_startup_warnings(&[], &[], &[], &[]).is_empty());
}
