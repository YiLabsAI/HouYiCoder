//! Choosing which shell runs a command.
//!
//! The dialect and program value types and the trust rules that judge a
//! candidate live in the platform crate, the single source for cross-platform
//! shell types. This module re-exports them and delegates the host selection to
//! the platform discovery, so the sandbox carries no platform conditionals.

pub use houyicoder_platform::host::discover_shell;
pub use houyicoder_platform::host::shell::{ShellPreference, ShellProgram, ShellSelection};
use std::path::{Path, PathBuf};

/// Choose the shell this session runs commands with, from the settings and the
/// host's own search. The untrusted directories name where a checkout, a working
/// directory, or a temp root could supply a program.
pub fn select_shell(
    preference: ShellPreference,
    configured: Option<&Path>,
    untrusted: &[PathBuf],
) -> ShellSelection {
    discover_shell(preference, configured, untrusted)
}
