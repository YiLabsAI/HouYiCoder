//! The path a typed spelling names.
//!
//! A leading tilde is the shell's spelling for the home directory, while the
//! file system reads it as a literal name. Every consumer that compares a
//! typed path against a stored one has to expand it the same way, or a grant
//! recorded through one spelling is a grant the other never matches.

use std::env::var_os;
use std::path::{Path, PathBuf};

/// The home directory a typed tilde expands against, read from the variables
/// the platform sets.
///
/// Windows reads the profile path first: a POSIX home inherited from a shell
/// names a path its file system cannot resolve. Unix has only HOME, so a
/// profile variable left over from somewhere else is not a home. An empty
/// value counts as unset in both cases.
pub fn home_directory() -> Option<PathBuf> {
    preferred_home(
        var_os("HOME").map(PathBuf::from),
        var_os("USERPROFILE").map(PathBuf::from),
    )
}

/// The home directory among the variables a platform sets, in the order this
/// platform reads them.
fn preferred_home(home: Option<PathBuf>, profile: Option<PathBuf>) -> Option<PathBuf> {
    let ordered = if cfg!(windows) {
        profile.or(home)
    } else {
        home
    };
    ordered.filter(|path| !path.as_os_str().is_empty())
}

/// The characters that separate a tilde from the rest of the path on this
/// platform. A backslash separates on Windows and is an ordinary file-name
/// character on Unix, where a typed backslash names a file rather than a
/// directory level.
const TILDE_SEPARATORS: &[char] = if cfg!(windows) { &['/', '\\'] } else { &['/'] };

/// Expand a leading tilde against the home directory.
///
/// A tilde that names another user, a path with no leading tilde, and one with
/// no home to expand against are returned as typed: their meaning is not ours
/// to guess, and a refusal then names the path the user wrote.
pub fn expand_home(path: &Path, home: Option<&Path>) -> PathBuf {
    let Some(text) = path.to_str() else {
        return path.to_path_buf();
    };
    let Some(rest) = text.strip_prefix('~') else {
        return path.to_path_buf();
    };
    if !rest.is_empty() && !rest.starts_with(TILDE_SEPARATORS) {
        return path.to_path_buf();
    }
    let Some(home) = home else {
        return path.to_path_buf();
    };
    let rest = rest.trim_start_matches(TILDE_SEPARATORS);
    if rest.is_empty() {
        home.to_path_buf()
    } else {
        home.join(rest)
    }
}

#[cfg(test)]
mod tests;
