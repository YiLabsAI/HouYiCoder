//! Sandbox shell selection configuration.
//!
//! Which program runs a command line on a Windows host. The default is to look
//! for Git Bash and fall back to the command interpreter; a host that has
//! neither reports the fallback rather than changing what a command means in
//! silence. Values here are settings only: the search, the trust checks, and
//! the chosen program live in the sandbox layer, which lowers this record onto
//! the session that runs commands.

use std::collections::BTreeMap;
use std::fs::read_to_string;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

/// The shell a host was asked to run commands with.
///
/// A value this version does not implement is held verbatim rather than
/// failing the parse, so the rest of the section survives and the caller can
/// name the value it ignored.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum ShellSetting {
    /// Search for Git Bash and fall back to the command interpreter, reporting
    /// the fallback. The default.
    #[default]
    #[serde(rename = "auto")]
    Auto,
    /// Git Bash asked for by name. A host without one still runs, and the
    /// report says the request could not be met.
    #[serde(rename = "git_bash")]
    GitBash,
    /// The command interpreter, never replaced by a search result.
    #[serde(rename = "cmd")]
    Cmd,
    /// A value this version does not implement, kept verbatim.
    #[serde(untagged)]
    Unsupported(String),
}

/// The shell section of the sandbox settings.
///
/// It lives under the sandbox section and holds two keys: mode selects the
/// shell a host runs commands with, and git_bash_path names the bash to run
/// when the search would not find it. The section follows the network
/// section's shape, so a user who learned one already knows the other.
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct SandboxShellConfig {
    /// Which shell a host should run commands with.
    pub mode: ShellSetting,
    /// The bash to run, for an install the search would not find. The path is
    /// checked before it is used: it must be an absolute path to bash whose
    /// real target sits outside the working tree, so a repository cannot
    /// supply the program that runs its commands.
    pub git_bash_path: Option<PathBuf>,
    /// Every key in the section this version does not act on, captured rather
    /// than discarded so a misspelled key is reported instead of ignored.
    #[serde(flatten)]
    pub unrecognized: BTreeMap<String, serde_json::Value>,
    /// Why the file could not be read as settings, when that happened: the
    /// values above are then the defaults rather than the user's answer, and
    /// an explicit mode in a file with a syntax error would otherwise be
    /// experienced as the default.
    #[serde(skip)]
    unreadable: Option<String>,
}

impl SandboxShellConfig {
    /// A section as a settings file would supply it: a mode and, for an
    /// install the search would not find, the bash to run.
    #[must_use]
    pub fn new(mode: ShellSetting, git_bash_path: Option<PathBuf>) -> Self {
        SandboxShellConfig {
            mode,
            git_bash_path,
            ..SandboxShellConfig::default()
        }
    }

    /// Describe every entry that was read but not acted on, phrased for the
    /// user. Returned rather than logged so the caller owns the destination.
    #[must_use]
    pub fn deferred_warnings(&self) -> Vec<String> {
        let mut out: Vec<String> = self.unreadable.iter().cloned().collect();
        if let ShellSetting::Unsupported(value) = &self.mode {
            out.push(format!(
                "mode {value:?} is not implemented, so the Windows command \
                 interpreter runs commands. Supported values are \"auto\", \
                 \"git_bash\" and \"cmd\""
            ));
        }
        if self.mode == ShellSetting::Cmd && self.git_bash_path.is_some() {
            out.push(
                "git_bash_path is ignored because mode is \"cmd\"; the command \
                 interpreter runs commands"
                    .to_owned(),
            );
        }
        out.extend(
            self.unrecognized
                .keys()
                .map(|k| format!("unknown key {k} is ignored")),
        );
        out
    }
}

/// Load the shell selection from the settings file. An absent file yields the
/// default, which searches and falls back with a report; a file that cannot be
/// read or parsed yields the default plus the reason, so a setting the user
/// wrote is not silently replaced by the default.
pub fn load_sandbox_shell() -> SandboxShellConfig {
    load_sandbox_shell_from(&crate::settings_path())
}

/// Pure loader against an explicit path; testable without env mutation.
pub fn load_sandbox_shell_from(path: &Path) -> SandboxShellConfig {
    let text = match read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == ErrorKind::NotFound => return SandboxShellConfig::default(),
        Err(e) => return unreadable(format!("settings could not be read: {e}")),
    };
    match serde_json::from_str::<SettingsShellView>(&text) {
        Ok(view) => view.sandbox.shell,
        Err(e) => unreadable(format!(
            "settings could not be read as a settings file, so the shell defaults apply: {e}"
        )),
    }
}

/// The default section, carrying the reason the file could not be used.
fn unreadable(reason: String) -> SandboxShellConfig {
    SandboxShellConfig {
        unreadable: Some(reason),
        ..SandboxShellConfig::default()
    }
}

/// Deserialization view over the settings file, reading only the path this
/// module owns. Unrelated settings keys are ignored, so this loader does not
/// have to track the rest of the settings schema.
#[derive(Debug, Default, serde::Deserialize)]
struct SettingsShellView {
    #[serde(default)]
    sandbox: ShellSectionView,
}

#[derive(Debug, Default, serde::Deserialize)]
struct ShellSectionView {
    #[serde(default)]
    shell: SandboxShellConfig,
}

#[cfg(test)]
mod tests;
