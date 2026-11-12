//! Shell value types and the trust rules that judge a candidate program.
//!
//! The dialect a command is written for and the program that runs it are
//! distinct: a model writes for a dialect, and the host picks the program.
//! The rules judge a candidate path by its form and its real target, so the
//! same rule runs wherever the crate builds. A path the working tree could
//! supply is never accepted, because the program that reads every command is
//! too dangerous to let a checkout choose.

use std::path::{Path, PathBuf};

/// The edition of a PowerShell program, named so a host reports which one it
/// started. Core is the cross-platform build; Desktop is the Windows-only one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PwshEdition {
    /// PowerShell 7 or later, started as pwsh.
    Core,
    /// Windows PowerShell 5.1, started as powershell.
    Desktop,
}

/// The shell a backend starts for one command. A property of the backend, not
/// of the caller: a backend reports the shell it will actually start.
#[expect(
    clippy::enum_variant_names,
    reason = "the variant names a distinct shell, not a repetition of the enum name"
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shell {
    /// A POSIX shell, on every unix host.
    Posix,
    /// The bash that ships with Git for Windows.
    GitBash,
    /// The Windows command interpreter.
    Cmd,
    /// PowerShell, in one of its editions.
    PowerShell(PwshEdition),
}

/// The program that runs a command line, and the form a command is passed to.
///
/// The variants without a path name a program the host resolves: the POSIX
/// shell is standard, and the command interpreter lives where the host keeps
/// it. Git Bash and PowerShell carry the absolute path the search settled on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellProgram {
    /// A POSIX shell.
    Posix,
    /// The Windows command interpreter.
    Cmd,
    /// Git Bash at the absolute path the search settled on.
    GitBash(PathBuf),
    /// PowerShell at the resolved path, in one of its editions.
    PowerShell {
        /// The executable path.
        path: PathBuf,
        /// Which PowerShell this is.
        edition: PwshEdition,
    },
}

impl ShellProgram {
    /// The dialect this program presents, so a command is translated for the
    /// program that will read it.
    #[must_use]
    pub fn shell(&self) -> Shell {
        match self {
            Self::Posix => Shell::Posix,
            Self::Cmd => Shell::Cmd,
            Self::GitBash(_) => Shell::GitBash,
            Self::PowerShell { edition, .. } => Shell::PowerShell(*edition),
        }
    }

    /// The program to start. The command interpreter is resolved by the host,
    /// because it lives where the host keeps it; the POSIX shell is standard.
    #[must_use]
    pub fn executable(&self) -> PathBuf {
        match self {
            Self::Posix => PathBuf::from("sh"),
            Self::Cmd => super::command_interpreter(),
            Self::GitBash(path) | Self::PowerShell { path, .. } => path.clone(),
        }
    }

    /// The arguments that pass one command line to this program.
    #[must_use]
    pub fn arguments(&self, command: &str) -> Vec<String> {
        match self {
            Self::Posix | Self::GitBash(_) => vec!["-c".to_owned(), command.to_owned()],
            Self::Cmd => vec!["/C".to_owned(), command.to_owned()],
            Self::PowerShell { .. } => vec![
                "-NoProfile".to_owned(),
                "-NonInteractive".to_owned(),
                "-Command".to_owned(),
                command.to_owned(),
            ],
        }
    }
}

/// Which shell a host was asked to run commands with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ShellPreference {
    /// Git Bash when one can be found; otherwise the command interpreter.
    #[default]
    Auto,
    /// Git Bash asked for by name.
    GitBash,
    /// The command interpreter, never replaced by a search result.
    Cmd,
}

/// The interpreter a configured helper command runs through. Explicit so a
/// settings value cannot silently change what a command means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelperShell {
    /// A POSIX shell, the default a helper command was written for.
    Sh,
    /// Git Bash.
    GitBash,
    /// The Windows command interpreter.
    Cmd,
    /// PowerShell.
    PowerShell,
}

/// The program a host settled on, and the notice a developer reads when the
/// choice is not the default.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ShellSelection {
    /// The program that runs commands. None when the host makes no choice.
    pub program: Option<ShellProgram>,
    /// A notice when the chosen program needs explaining.
    pub notice: Option<String>,
}

/// Whether the system search ran. The rules need to know only when it did not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchOutcome {
    /// The search ran, whatever it named.
    Ran,
    /// The search could not run on this host.
    Unavailable,
}

/// Where a shell program may be found, in trust order.
pub struct Candidates<'a> {
    /// The absolute path from settings.
    pub configured: Option<&'a Path>,
    /// The install locations an installer uses.
    pub standard: &'a [PathBuf],
    /// What the system's own search named, in the order it listed them.
    pub searched: &'a [PathBuf],
    /// How the search that produced the list above ended.
    pub search: SearchOutcome,
    /// Directories a shell program must not live in.
    pub untrusted: &'a [PathBuf],
}

/// Why a candidate was refused, kept so a failed search can name the reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// Not an absolute Windows path.
    NotAbsolute,
    /// A file name other than bash.
    NotBash,
    /// Inside a directory the working tree can write.
    Untrusted,
    /// No file there, or the real target cannot be read.
    Missing,
    /// A launcher for the Linux subsystem, which is not the POSIX shell the
    /// command was written for.
    WslLauncher,
}

impl Refusal {
    /// A phrase naming the reason, for a notice a developer reads.
    pub fn describe(self) -> &'static str {
        match self {
            Self::NotAbsolute => "it is not an absolute path",
            Self::NotBash => "it does not name bash",
            Self::Untrusted => "it is inside a directory the working tree can write",
            Self::Missing => "no file exists there",
            Self::WslLauncher => "it is the Linux subsystem launcher, not Git Bash",
        }
    }
}

/// Choose the program that runs a command, from the candidates in trust order.
///
/// The file checks arrive as arguments so the decision is the same on every
/// host and a test can drive each refusal without touching the disk. A Cmd
/// preference or a failed search settles on the command interpreter.
pub fn select(
    preference: ShellPreference,
    candidates: &Candidates<'_>,
    exists: impl Fn(&Path) -> bool,
    real_path: impl Fn(&Path) -> Option<PathBuf>,
) -> ShellSelection {
    if preference == ShellPreference::Cmd {
        return ShellSelection {
            program: Some(ShellProgram::Cmd),
            notice: None,
        };
    }
    let mut refused: Vec<(PathBuf, Refusal)> = Vec::new();
    for candidate in trust_order(candidates) {
        match accept(&candidate, candidates.untrusted, &exists, &real_path) {
            Ok(program) => {
                let notice = accepted_notice(preference, candidates.configured, &refused, &program);
                return ShellSelection {
                    program: Some(program),
                    notice,
                };
            }
            Err(reason) => refused.push((candidate, reason)),
        }
    }
    ShellSelection {
        program: Some(ShellProgram::Cmd),
        notice: Some(fallback_notice(
            preference,
            candidates.configured,
            &refused,
            candidates.search,
        )),
    }
}

/// The directories a shell program must not live in: the caller's roots, the
/// directory commands run in, and the temp directory. Every root is kept in
/// the form that named it and in the form the file system reports.
pub fn untrusted_roots(named: &[PathBuf], working: Option<&Path>, temp: &Path) -> Vec<PathBuf> {
    let mut roots = named.to_vec();
    if let Some(working) = working {
        roots.push(working.to_path_buf());
    }
    roots.push(temp.to_path_buf());
    let mut forms = roots.clone();
    forms.extend(
        roots
            .iter()
            .filter_map(|root| dunce::canonicalize(root).ok()),
    );
    forms.sort();
    forms.dedup();
    forms
}

/// True when the path is one of the directories or below it. The comparison
/// folds the separator and the case, because a Windows file system matches
/// both ways.
pub fn under_any(path: &Path, roots: &[PathBuf]) -> bool {
    let candidate = normalized(path);
    roots.iter().any(|root| {
        let root = normalized(root);
        candidate == root || candidate.starts_with(&format!("{root}/"))
    })
}

/// The install locations an installer writes, derived from the environment a
/// user can set. Each entry of the returned list names a bash the search
/// should try, in the order the variables outrank the fixed paths.
///
/// A variable naming a bash directly is pushed as it stands. A variable naming
/// an install root contributes the two layouts an install uses, bin and the
/// deeper usr bin, because either one may hold the only bash a relocated
/// install left on its disk.
pub fn locations_from_env(direct: Option<&Path>, roots: Option<&Path>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(path) = direct {
        out.push(path.to_path_buf());
    }
    if let Some(root) = roots {
        out.push(root.join("bin").join("bash.exe"));
        out.push(root.join("usr").join("bin").join("bash.exe"));
    }
    out
}

/// The bash paths an install of Git for Windows holds, derived from a path to
/// its git program. The install root sits two or three levels above the file
/// that PATH names, so the search walks the parents of the git path and keeps
/// every candidate that could hold the install's bash.
///
/// The separator is folded before the walk, because a path from a Windows
/// search carries the backslash form and the parents must split on it wherever
/// this code runs. The git path itself is only a starting point: a path under
/// cmd or the deeper mingw64 bin names the same install once its parents are
/// walked.
pub fn bash_from_git(git: &Path, exists: impl Fn(&Path) -> bool) -> Vec<PathBuf> {
    let folded = PathBuf::from(git.to_string_lossy().replace('\\', "/"));
    let mut out = Vec::new();
    // Walk at most three parents: cmd, mingw64 bin, and usr bin each sit two
    // or three levels below the install root, and a deeper walk would leave
    // the install's own directories.
    let mut parent = folded.parent().map(Path::to_path_buf);
    for _ in 0..3 {
        let Some(step) = parent else { break };
        let root = step;
        for sub in ["bin", "usr/bin"] {
            let path = root.join(sub).join("bash.exe");
            if !out.contains(&path) && exists(&path) {
                out.push(path);
            }
        }
        parent = root.parent().map(Path::to_path_buf);
    }
    out
}

/// Every candidate in trust order: settings first, then the standard install
/// locations, then what the system search named.
fn trust_order(candidates: &Candidates<'_>) -> impl Iterator<Item = PathBuf> {
    candidates
        .configured
        .map(Path::to_path_buf)
        .into_iter()
        .chain(candidates.standard.iter().cloned())
        .chain(candidates.searched.iter().cloned())
}

/// Judge one candidate, and answer with the real target of an accepted one.
fn accept(
    candidate: &Path,
    untrusted: &[PathBuf],
    exists: &impl Fn(&Path) -> bool,
    real_path: &impl Fn(&Path) -> Option<PathBuf>,
) -> Result<ShellProgram, Refusal> {
    if !is_absolute_windows(candidate) {
        return Err(Refusal::NotAbsolute);
    }
    if !names_bash(candidate) {
        return Err(Refusal::NotBash);
    }
    if is_wsl_launcher(candidate) {
        return Err(Refusal::WslLauncher);
    }
    if under_any(candidate, untrusted) {
        return Err(Refusal::Untrusted);
    }
    if !exists(candidate) {
        return Err(Refusal::Missing);
    }
    let real = real_path(candidate).ok_or(Refusal::Missing)?;
    if under_any(&real, untrusted) {
        return Err(Refusal::Untrusted);
    }
    Ok(ShellProgram::GitBash(real))
}

/// What the user is told when a program was found.
fn accepted_notice(
    preference: ShellPreference,
    configured: Option<&Path>,
    refused: &[(PathBuf, Refusal)],
    program: &ShellProgram,
) -> Option<String> {
    if let Some((path, reason)) = refused_entry(refused, configured) {
        return Some(format!(
            "the path in settings, {}, was refused because {}, so {} runs commands instead",
            path.display(),
            reason.describe(),
            describe(program)
        ));
    }
    (preference == ShellPreference::Auto)
        .then(|| format!("commands run with {}", describe(program)))
}

fn fallback_notice(
    preference: ShellPreference,
    configured: Option<&Path>,
    refused: &[(PathBuf, Refusal)],
    search: SearchOutcome,
) -> String {
    let mut message = String::new();
    if let Some((path, reason)) = refused_entry(refused, configured) {
        message.push_str(&format!(
            "the path in settings, {}, was refused because {}, and ",
            path.display(),
            reason.describe()
        ));
    }
    message.push_str(match preference {
        ShellPreference::GitBash => "Git Bash was asked for by name but none was found",
        _ => "no Git Bash was found",
    });
    if search == SearchOutcome::Unavailable {
        message.push_str(
            "; the system search could not run here, so only the settings path \
             and the standard install locations were checked",
        );
    }
    message.push_str(", so commands run with the Windows command interpreter");
    message
}

/// The refused entry for the path the settings named, when it was refused.
fn refused_entry<'a>(
    refused: &'a [(PathBuf, Refusal)],
    configured: Option<&Path>,
) -> Option<(&'a Path, Refusal)> {
    let configured = configured?;
    refused
        .iter()
        .find(|(path, _)| path.as_path() == configured)
        .map(|(path, reason)| (path.as_path(), *reason))
}

fn describe(program: &ShellProgram) -> String {
    match program {
        ShellProgram::Cmd => "the Windows command interpreter".to_owned(),
        ShellProgram::GitBash(path) => format!("Git Bash at {}", path.display()),
        ShellProgram::Posix => "the POSIX shell".to_owned(),
        ShellProgram::PowerShell { edition, .. } => match edition {
            PwshEdition::Core => "PowerShell 7".to_owned(),
            PwshEdition::Desktop => "Windows PowerShell".to_owned(),
        },
    }
}

/// True when the path is rooted the way a Windows path is: a drive letter, or
/// a network share named by two leading separators.
fn is_absolute_windows(path: &Path) -> bool {
    let text = path.to_string_lossy();
    match text.as_bytes() {
        [b'\\', b'\\', ..] => true,
        [letter, b':', separator, ..] => {
            letter.is_ascii_alphabetic() && matches!(separator, b'\\' | b'/')
        }
        _ => false,
    }
}

/// True when the last segment of the path names bash, compared without regard
/// to case.
fn names_bash(path: &Path) -> bool {
    let text = path.to_string_lossy();
    text.rsplit(['/', '\\'])
        .next()
        .is_some_and(|name| name.eq_ignore_ascii_case("bash.exe"))
}

/// True when the path names the Linux subsystem launcher, which a search for
/// Git Bash must refuse: it is not the POSIX shell the command was written for.
fn is_wsl_launcher(path: &Path) -> bool {
    let text = path.to_string_lossy().to_ascii_lowercase();
    text.ends_with("\\system32\\bash.exe")
        || text.ends_with("\\sysnative\\bash.exe")
        || text.ends_with("\\microsoft\\windowsapps\\bash.exe")
}

fn normalized(path: &Path) -> String {
    let mut text = path
        .to_string_lossy()
        .replace('\\', "/")
        .to_ascii_lowercase();
    while text.len() > 1 && text.ends_with('/') {
        text.pop();
    }
    text
}

#[cfg(test)]
mod tests;
