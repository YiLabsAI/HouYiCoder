//! Choosing which shell runs a command on a Windows host.
//!
//! The shell decides what a command line means, so the choice is made once,
//! before any command runs, and reported through the session: the tool layer
//! translates a command for the shell that will read it. Candidates are tried
//! in trust order, and every one is refused when its real target sits inside
//! the working tree, the working directory, or the temp root. That covers the
//! directories a checkout can supply; a per-user install and a program found
//! on the path list are accepted on location alone, which is a recorded
//! residual risk because the Windows fence limits resources, not writes.

use houyicoder_api::shell::Shell;
#[cfg(target_os = "windows")]
use std::env::var_os;
use std::path::{Path, PathBuf};

#[cfg(target_os = "windows")]
mod discovery;
#[cfg(any(target_os = "windows", test))]
mod rules;

/// The shell a host was asked to run commands with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ShellPreference {
    /// Git Bash when one can be found; otherwise the command interpreter,
    /// reported.
    #[default]
    Auto,
    /// Git Bash asked for by name. A host without one still runs, but the
    /// fallback is reported as a broken promise rather than as a default.
    GitBash,
    /// The command interpreter. Never replaced by a search result, so a
    /// settings value cannot silently change what a command means.
    Cmd,
}

/// The program that runs a command line, and the form a command is passed to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellProgram {
    /// The Windows command interpreter, which reads a command line after its
    /// command flag.
    Cmd,
    /// Git Bash, at the absolute path the search settled on.
    GitBash(PathBuf),
}

impl ShellProgram {
    /// The shell this program presents to the tool layer, so a command is
    /// translated for the program that will read it.
    #[must_use]
    pub fn shell(&self) -> Shell {
        match self {
            Self::Cmd => Shell::Cmd,
            Self::GitBash(_) => Shell::GitBash,
        }
    }

    /// The program to start.
    ///
    /// The command interpreter is named by the path the system keeps it at: a
    /// bare name is answered from the directory the command runs in before the
    /// system directory, so a file the working tree planted would start.
    #[must_use]
    pub fn executable(&self) -> PathBuf {
        match self {
            Self::Cmd => command_interpreter(),
            Self::GitBash(path) => path.clone(),
        }
    }

    /// The arguments that pass one command line to this program.
    #[must_use]
    pub fn arguments(&self, command: &str) -> Vec<String> {
        match self {
            Self::Cmd => vec!["/C".to_owned(), command.to_owned()],
            Self::GitBash(_) => vec!["-c".to_owned(), command.to_owned()],
        }
    }
}

/// The system's own command interpreter, by absolute path where the host
/// supplies one.
///
/// The system directory is read from the environment because the interpreter
/// lives there on every supported host, and the shell variable is the host's
/// own record of where it is. Only a host that sets neither leaves the bare
/// name as the answer, which resolves through the system search rather than
/// through an absolute path.
fn command_interpreter() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        if let Some(root) = var_os("SystemRoot") {
            return PathBuf::from(root).join("System32").join("cmd.exe");
        }
        if let Some(shell) = var_os("COMSPEC") {
            return PathBuf::from(shell);
        }
    }
    PathBuf::from("cmd")
}

/// The shell a host will run commands with, and what to tell the user about
/// the choice.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ShellSelection {
    /// The program that runs a command. None when the host makes no choice,
    /// because a POSIX host runs its own shell.
    pub program: Option<ShellProgram>,
    /// A line for the user when the choice needs explaining: a requested
    /// shell that did not run, a settings path that was refused, or the
    /// default search picking a shell other than the interpreter. Phrased
    /// without a field prefix, because the caller that displays it owns the
    /// prefix.
    pub notice: Option<String>,
}

/// Choose the shell this session runs commands with, from the settings and the
/// host's own search.
///
/// A POSIX host has no choice to make, so it reports no program and answers a
/// Windows-only setting with a line saying so. A Windows host always reports a
/// program.
pub fn select_shell(
    preference: ShellPreference,
    configured: Option<&Path>,
    untrusted: &[PathBuf],
) -> ShellSelection {
    #[cfg(target_os = "windows")]
    {
        discovery::discover(preference, configured, untrusted)
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = untrusted;
        let notice = match (preference, configured) {
            (ShellPreference::Auto, None) => None,
            (ShellPreference::Auto, Some(path)) => Some(format!(
                "the bash path in settings, {}, only selects a shell on Windows; \
                 this host keeps its own shell",
                path.display()
            )),
            (ShellPreference::GitBash | ShellPreference::Cmd, _) => Some(
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
}

#[cfg(test)]
mod tests;
