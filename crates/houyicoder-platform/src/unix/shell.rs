//! Unix shell discovery: a POSIX host keeps its own shell, so the search names
//! no program and a Windows-only setting is reported as inactive.

use std::path::{Path, PathBuf};

use crate::host::shell::{ShellPreference, ShellSelection};

pub(crate) fn discover(
    preference: ShellPreference,
    configured: Option<&Path>,
    untrusted: &[PathBuf],
) -> ShellSelection {
    let _ = untrusted;
    let notice = match (preference, configured) {
        (ShellPreference::Auto, None) => None,
        (ShellPreference::Auto, Some(path)) => Some(format!(
            "the bash path in settings, {}, only selects a shell on Windows; \
             this host keeps its own shell",
            path.display()
        )),
        (ShellPreference::GitBash | ShellPreference::Cmd | ShellPreference::PowerShell, _) => Some(
            "the shell setting only selects a shell on Windows; this host keeps \
             its own shell"
                .to_owned(),
        ),
    };
    ShellSelection {
        program: None,
        notice,
    }
}
