//! Startup-warning formatting: the config loads each produce warnings, and
//! this turns them into the lines the runner queues for the host to drain as
//! opening transcript system lines. It is the last hop before the user sees
//! them, so a source that reaches here and is not merged goes silent. Split
//! from composition.rs so that file stays under the size gate.

use houyicoder_config::ConfigWarning;

/// Flatten the startup warning sources into the lines the runner queues.
/// Network warnings arrive as bare strings with no field of their own, so
/// they take a prefix to name their origin; the rest already carry one.
pub(super) fn collect_startup_warnings(
    network: &[String],
    toggles: &[ConfigWarning],
    effort: &[ConfigWarning],
    provider: &[ConfigWarning],
) -> Vec<String> {
    let mut out: Vec<String> = network
        .iter()
        .map(|w| format!("sandbox.network: {w}"))
        .collect();
    out.extend(toggles.iter().map(format_warning));
    out.extend(effort.iter().map(format_warning));
    out.extend(provider.iter().map(format_warning));
    out
}

fn format_warning(w: &ConfigWarning) -> String {
    format!("{}: {}", w.field, w.reason)
}

#[cfg(test)]
#[path = "startup_warnings_tests.rs"]
mod tests;
