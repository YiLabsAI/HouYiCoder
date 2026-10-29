//! Startup-warning formatting: the config loads each produce warnings, and
//! this turns them into the lines the runner queues for the host to drain as
//! opening transcript system lines. It is the last hop before the user sees
//! them, so a source that reaches here and is not merged goes silent. Split
//! from composition.rs so that file stays under the size gate.

use crate::sandbox_policy::SandboxWarnings;
use houyicoder_config::ConfigWarning;

/// Flatten the startup warning sources into the lines the runner queues.
/// Sandbox warnings arrive as bare strings with no field of their own, so this
/// is the one place that prefixes them with the settings section they came
/// from; every other source carries its field already.
pub(super) fn collect_startup_warnings(
    sandbox: &SandboxWarnings,
    toggles: &[ConfigWarning],
    effort: &[ConfigWarning],
    provider: &[ConfigWarning],
    recap: &[ConfigWarning],
) -> Vec<String> {
    let mut out: Vec<String> = sandbox
        .network
        .iter()
        .map(|w| format!("sandbox.network: {w}"))
        .collect();
    out.extend(sandbox.shell.iter().map(|w| format!("sandbox.shell: {w}")));
    out.extend(toggles.iter().map(format_warning));
    out.extend(effort.iter().map(format_warning));
    out.extend(provider.iter().map(format_warning));
    out.extend(recap.iter().map(format_warning));
    out
}

fn format_warning(w: &ConfigWarning) -> String {
    format!("{}: {}", w.field, w.reason)
}

#[cfg(test)]
mod tests;
