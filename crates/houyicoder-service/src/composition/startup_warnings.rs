//! Startup-warning formatting: merges the three startup warning sources
//! (network-policy typos, settings toggle warnings, model-section warnings)
//! into one Vec<String> the runner queues for the host to drain + surface as
//! initial transcript system lines. Split from composition.rs so that file
//! stays under the size gate.

use houyicoder_config::ConfigWarning;

/// Merge network-policy lines (raw strings from the sandbox-policy load)
/// with the field/reason pairs from the toggle and model-section loads into
/// one queue. Order is network, then toggles, then model-section — matching
/// the order the loads run at startup. Each ConfigWarning becomes
/// "field: reason"; each network line is prefixed "sandbox.network: " so its
/// origin is visible in the transcript.
pub(super) fn collect_startup_warnings(
    network: &[String],
    toggles: &[ConfigWarning],
    effort: &[ConfigWarning],
) -> Vec<String> {
    let mut out: Vec<String> = network
        .iter()
        .map(|w| format!("sandbox.network: {w}"))
        .collect();
    out.extend(toggles.iter().map(format_warning));
    out.extend(effort.iter().map(format_warning));
    out
}

fn format_warning(w: &ConfigWarning) -> String {
    format!("{}: {}", w.field, w.reason)
}
