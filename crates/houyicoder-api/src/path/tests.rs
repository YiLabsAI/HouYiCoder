//! The spellings a typed path takes before it is compared with a stored one.

use super::{expand_home, preferred_home};
use std::path::{Path, PathBuf};

fn home() -> PathBuf {
    PathBuf::from("/home/ada")
}

/// The separators a tilde accepts follow the platform's path syntax. A
/// backslash is a separator on Windows and an ordinary file-name character
/// elsewhere, where a typed backslash names a file rather than a level.
#[cfg(windows)]
const SEPARATORS: &[&str] = &["/", "\\"];
#[cfg(not(windows))]
const SEPARATORS: &[&str] = &["/"];

/// A path typed as the home directory, with or without a trailing separator,
/// names the home directory itself.
#[test]
fn test_tilde_expands_to_home() {
    assert_eq!(expand_home(Path::new("~"), Some(&home())), home());
    for separator in SEPARATORS {
        let typed = format!("~{separator}");
        assert_eq!(
            expand_home(Path::new(&typed), Some(&home())),
            home(),
            "{typed}"
        );
    }
}

/// A path below the home directory joins onto it, with the separator the
/// platform accepts.
#[test]
fn test_tilde_child_joined() {
    let expected = Path::new("/home/ada/.houyicoder");
    for separator in SEPARATORS {
        let typed = format!("~{separator}.houyicoder");
        assert_eq!(
            expand_home(Path::new(&typed), Some(&home())),
            expected,
            "{typed}"
        );
    }
}

/// A backslash after the tilde names a file on Unix, so the path stays as
/// typed rather than being read as a level below home.
#[cfg(not(windows))]
#[test]
fn test_tilde_backslash_literal() {
    assert_eq!(
        expand_home(Path::new("~\\.houyicoder"), Some(&home())),
        PathBuf::from("~\\.houyicoder")
    );
}

/// Windows expands a tilde against the profile path, because a POSIX home
/// inherited from a shell names a path its file system cannot resolve. Unix
/// reads only HOME, so a profile variable is not a home to expand against.
#[test]
fn test_home_prefers_profile() {
    let posix = PathBuf::from("/c/Users/ada");
    let profile = PathBuf::from(r"C:\Users\ada");
    let chosen = preferred_home(Some(posix.clone()), Some(profile.clone()));
    if cfg!(windows) {
        assert_eq!(chosen, Some(profile));
    } else {
        assert_eq!(chosen, Some(posix));
    }
    assert_eq!(preferred_home(Some(PathBuf::new()), None), None);
}

/// A tilde naming another user, or a word merely starting with one, is not
/// the home directory and stays as typed so the refusal names what the user
/// actually wrote.
#[test]
fn test_other_tilde_left_alone() {
    for typed in ["~root/x", "~ada", "~not-a-home/file", "a~b"] {
        assert_eq!(
            expand_home(Path::new(typed), Some(&home())),
            PathBuf::from(typed)
        );
    }
}

/// Without a home directory to expand to, the path stays as typed rather than
/// being resolved against the process working directory.
#[test]
fn test_missing_home_keeps_input() {
    assert_eq!(expand_home(Path::new("~/x"), None), PathBuf::from("~/x"));
}

/// A path with no tilde is returned unchanged, separators and all.
#[test]
fn test_plain_path_unchanged() {
    assert_eq!(
        expand_home(Path::new("/srv/data/"), Some(&home())),
        PathBuf::from("/srv/data/")
    );
}

/// An empty spelling names nothing, so expanding it must not turn it into the
/// home directory.
#[test]
fn test_empty_path_unchanged() {
    assert_eq!(expand_home(Path::new(""), Some(&home())), PathBuf::new());
}
