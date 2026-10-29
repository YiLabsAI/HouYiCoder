//! The trust rules that judge a shell candidate, shared by every host so the
//! same rules are unit-tested wherever the crate builds.

use super::{ShellPreference, ShellProgram, ShellSelection};
use std::path::{Path, PathBuf};

/// Whether the system search ran. The rules need to know only when it did
/// not, so a failed report can say the search could not run instead of
/// claiming no Git Bash is installed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SearchOutcome {
    /// The search ran, whatever it named.
    Ran,
    /// The search could not run on this host.
    Unavailable,
}

/// Where a shell program may be found, in trust order.
pub(super) struct Candidates<'a> {
    /// The absolute path from settings.
    pub(super) configured: Option<&'a Path>,
    /// The install locations a Git Bash installer uses.
    pub(super) standard: &'a [PathBuf],
    /// What the system's own search named, in the order it listed them.
    pub(super) searched: &'a [PathBuf],
    /// How the search that produced the list above ended.
    pub(super) search: SearchOutcome,
    /// Directories a shell program must not live in: a checkout, a working
    /// directory, or a temp root could supply the program that then reads
    /// every command.
    pub(super) untrusted: &'a [PathBuf],
}

/// Choose the program that runs a command, from the candidates in trust order.
///
/// The file checks arrive as arguments so the decision is the same on every
/// host and a test can drive each refusal without touching the disk.
pub(super) fn select(
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
/// directory commands run in, and the temp directory.
///
/// Every root is kept in the form that named it and in the form the file
/// system reports, so a match does not depend on a root being resolvable: a
/// root that disappears between resolution and use still encloses what it
/// enclosed.
pub(super) fn untrusted_roots(
    named: &[PathBuf],
    working: Option<&Path>,
    temp: &Path,
) -> Vec<PathBuf> {
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

/// Why a candidate was refused, kept so a failed search can name the reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Refusal {
    /// Not an absolute Windows path.
    NotAbsolute,
    /// A file name other than bash.
    NotBash,
    /// Inside a directory the working tree can write.
    Untrusted,
    /// No file there, or the real target cannot be read.
    Missing,
}

impl Refusal {
    fn describe(self) -> &'static str {
        match self {
            Self::NotAbsolute => "it is not an absolute path",
            Self::NotBash => "it does not name bash",
            Self::Untrusted => "it is inside a directory the working tree can write",
            Self::Missing => "no file exists there",
        }
    }
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
    if under_any(candidate, untrusted) {
        return Err(Refusal::Untrusted);
    }
    if !exists(candidate) {
        return Err(Refusal::Missing);
    }
    // A path can end at a file that lives elsewhere: a link, a junction, a
    // short name. The question is where the bytes that run actually live, so
    // acceptance rests on the resolved path.
    let real = real_path(candidate).ok_or(Refusal::Missing)?;
    if under_any(&real, untrusted) {
        return Err(Refusal::Untrusted);
    }
    Ok(ShellProgram::GitBash(real))
}

/// What the user is told when a program was found.
///
/// A settings path that was refused is the one thing that must not pass in
/// silence: the user stated which program runs, and a different one does. The
/// default setting is reported too, because it can pick a shell that changes
/// what a command means and nothing else says so.
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
    message.push_str(
        ", so commands run with the Windows command interpreter. Point \
         sandbox.shell.git_bash_path at the bash to run.",
    );
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
/// to case because a Windows file system does not distinguish it.
fn names_bash(path: &Path) -> bool {
    let text = path.to_string_lossy();
    text.rsplit(['/', '\\'])
        .next()
        .is_some_and(|name| name.eq_ignore_ascii_case("bash.exe"))
}

/// True when the path is one of the directories or below it.
///
/// The comparison folds the separator and the case, because a Windows file
/// system matches both ways. It is written over the text rather than through
/// path components because the paths being compared belong to the Windows file
/// system even when this host resolves them, and it stops at a separator so a
/// sibling whose name merely starts with the root's name is not inside it.
pub(super) fn under_any(path: &Path, roots: &[PathBuf]) -> bool {
    let candidate = normalized(path);
    roots.iter().any(|root| {
        let root = normalized(root);
        candidate == root || candidate.starts_with(&format!("{root}/"))
    })
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
