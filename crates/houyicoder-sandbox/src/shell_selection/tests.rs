//! Peer tests for the shell choice: each refusal, the fallback reports, and
//! the directory set a program must stay out of.

use super::rules::{Candidates, SearchOutcome, select, untrusted_roots};
#[cfg(not(target_os = "windows"))]
use super::select_shell;
use super::{ShellPreference, ShellProgram};
use houyicoder_api::shell::Shell;
use std::path::{Path, PathBuf};

const STANDARD: &str = "C:\\Program Files\\Git\\bin\\bash.exe";

fn paths(items: &[&str]) -> Vec<PathBuf> {
    items.iter().map(PathBuf::from).collect()
}

fn present(_: &Path) -> bool {
    true
}

fn absent(_: &Path) -> bool {
    false
}

fn unchanged(path: &Path) -> Option<PathBuf> {
    Some(path.to_path_buf())
}

/// A candidate set with no settings path and no search result.
fn standard_only<'a>(standard: &'a [PathBuf], untrusted: &'a [PathBuf]) -> Candidates<'a> {
    Candidates {
        configured: None,
        standard,
        searched: &[],
        search: SearchOutcome::Ran,
        untrusted,
    }
}

/// The settings path outranks the standard install locations, so an explicit
/// choice is the one that runs, and the default setting says which shell that
/// is.
#[test]
fn test_configured_path_wins() {
    let configured = PathBuf::from("D:\\tools\\git\\bin\\bash.exe");
    let standard = paths(&[STANDARD]);
    let candidates = Candidates {
        configured: Some(&configured),
        standard: &standard,
        searched: &[],
        search: SearchOutcome::Ran,
        untrusted: &[],
    };
    let selection = select(ShellPreference::Auto, &candidates, present, unchanged);
    assert_eq!(
        selection.program,
        Some(ShellProgram::GitBash(configured.clone())),
        "the path from settings is the one that runs"
    );
    let notice = selection.notice.expect("the default setting reports");
    assert!(notice.contains("D:\\tools\\git"), "{notice}");
}

/// A candidate inside the working tree is refused even when it is the first
/// one, because a repository that can write it could supply the program.
#[test]
fn test_untrusted_candidate_refused() {
    let workspace = PathBuf::from("C:\\work\\repo");
    let outside = PathBuf::from(STANDARD);
    let inside = workspace.join("bin").join("bash.exe");
    let standard = vec![inside, outside.clone()];
    let untrusted = vec![workspace];
    let candidates = standard_only(&standard, &untrusted);
    let selection = select(ShellPreference::Auto, &candidates, present, unchanged);
    assert_eq!(
        selection.program,
        Some(ShellProgram::GitBash(outside)),
        "the candidate inside the working tree is skipped"
    );
}

/// A directory that merely shares the root's name prefix is beside it rather
/// than inside it, so a checkout named after the workspace cannot supply the
/// program.
#[test]
fn test_sibling_root_accepted() {
    let workspace = PathBuf::from("C:\\work\\repo");
    let sibling = PathBuf::from("C:\\work\\repo-extra\\bin\\bash.exe");
    let standard = vec![sibling.clone()];
    let untrusted = vec![workspace];
    let candidates = standard_only(&standard, &untrusted);
    let selection = select(ShellPreference::Auto, &candidates, present, unchanged);
    assert_eq!(
        selection.program,
        Some(ShellProgram::GitBash(sibling)),
        "the boundary test compares whole components, not a name prefix"
    );
}

/// The same root written with other case and the other separator still covers
/// its files, so a candidate cannot escape the check by spelling the path
/// differently.
#[test]
fn test_root_spelling_folded() {
    let workspace = PathBuf::from("C:\\Work\\Repo");
    let inside = PathBuf::from("c:/work/repo/bin/bash.exe");
    let outside = PathBuf::from(STANDARD);
    let standard = vec![inside, outside.clone()];
    let untrusted = vec![workspace];
    let candidates = standard_only(&standard, &untrusted);
    let selection = select(ShellPreference::Auto, &candidates, present, unchanged);
    assert_eq!(
        selection.program,
        Some(ShellProgram::GitBash(outside)),
        "case and separator spelling do not change which root a file is under"
    );
}

/// A bare file name is resolved against the current directory by the system,
/// so it is refused before any file check runs.
#[test]
fn test_relative_candidate_refused() {
    let standard = paths(&["bin/bash.exe", STANDARD]);
    let candidates = standard_only(&standard, &[]);
    let selection = select(ShellPreference::Auto, &candidates, present, unchanged);
    assert_eq!(
        selection.program,
        Some(ShellProgram::GitBash(PathBuf::from(STANDARD))),
        "only the absolute candidate is eligible"
    );
}

/// A path that exists but resolves into the working tree is refused: the
/// question is where the file that runs lives, not what its name says.
#[test]
fn test_replaced_target_refused() {
    let workspace = PathBuf::from("C:\\work\\repo");
    let planted = workspace.join("payload").join("bash.exe");
    let standard = paths(&[STANDARD]);
    let untrusted = vec![workspace];
    let candidates = standard_only(&standard, &untrusted);
    let selection = select(ShellPreference::Auto, &candidates, present, |_| {
        Some(planted.clone())
    });
    assert_eq!(selection.program, Some(ShellProgram::Cmd));
    assert!(selection.notice.is_some(), "the fallback is reported");
}

/// A candidate whose file name is not bash cannot be the shell, however
/// eligible its location is.
#[test]
fn test_other_program_refused() {
    let configured = PathBuf::from("C:\\tools\\git\\bin\\git.exe");
    let candidates = Candidates {
        configured: Some(&configured),
        standard: &[],
        searched: &[],
        search: SearchOutcome::Ran,
        untrusted: &[],
    };
    let selection = select(ShellPreference::Auto, &candidates, present, unchanged);
    assert_eq!(selection.program, Some(ShellProgram::Cmd));
    let notice = selection.notice.expect("the refusal is reported");
    assert!(notice.contains("does not name bash"), "{notice}");
}

/// A share path is an absolute Windows path, and a path with spaces is an
/// ordinary one; neither is refused for its shape.
#[test]
fn test_network_path_accepted() {
    let share = PathBuf::from("\\\\fileserver\\tools\\git\\bin\\bash.exe");
    let standard = vec![share.clone()];
    let candidates = standard_only(&standard, &[]);
    let selection = select(ShellPreference::Auto, &candidates, present, unchanged);
    assert_eq!(selection.program, Some(ShellProgram::GitBash(share)));
}

/// With no candidate eligible, the command interpreter runs the command and
/// the user is told, so the change of shell is not a silent one.
#[test]
fn test_missing_bash_degrades() {
    let standard = paths(&[STANDARD]);
    let candidates = standard_only(&standard, &[]);
    let selection = select(ShellPreference::Auto, &candidates, absent, unchanged);
    assert_eq!(selection.program, Some(ShellProgram::Cmd));
    let notice = selection.notice.expect("a fallback is reported");
    assert!(notice.contains("no Git Bash was found"), "{notice}");
}

/// A search that could not run is reported as that, rather than as a machine
/// with no Git Bash installed.
#[test]
fn test_search_unavailable_reported() {
    let candidates = Candidates {
        configured: None,
        standard: &[],
        searched: &[],
        search: SearchOutcome::Unavailable,
        untrusted: &[],
    };
    let selection = select(ShellPreference::Auto, &candidates, absent, unchanged);
    assert_eq!(selection.program, Some(ShellProgram::Cmd));
    let notice = selection.notice.expect("the failed search is reported");
    assert!(notice.contains("could not run"), "{notice}");
}

/// The report names the path from settings and why it was refused, so the
/// user can correct it instead of guessing why the setting had no effect.
#[test]
fn test_refused_setting_gone() {
    let configured = PathBuf::from(STANDARD);
    let candidates = Candidates {
        configured: Some(&configured),
        standard: &[],
        searched: &[],
        search: SearchOutcome::Ran,
        untrusted: &[],
    };
    let selection = select(ShellPreference::Auto, &candidates, absent, unchanged);
    let notice = selection.notice.expect("a fallback is reported");
    assert!(notice.contains(STANDARD), "{notice}");
    assert!(notice.contains("no file exists there"), "{notice}");
}

/// A settings path that was refused while another program was found is still
/// named: the user stated which program runs, and a different one does.
#[test]
fn test_refused_setting_still_named() {
    let configured = PathBuf::from("D:\\moved\\git\\bin\\bash.exe");
    let standard = paths(&[STANDARD]);
    let candidates = Candidates {
        configured: Some(&configured),
        standard: &standard,
        searched: &[],
        search: SearchOutcome::Ran,
        untrusted: &[],
    };
    let found = |path: &Path| path == Path::new(STANDARD);
    let selection = select(ShellPreference::Auto, &candidates, found, unchanged);
    assert_eq!(
        selection.program,
        Some(ShellProgram::GitBash(PathBuf::from(STANDARD)))
    );
    let notice = selection.notice.expect("the ignored setting is reported");
    assert!(notice.contains("D:\\moved"), "{notice}");
    assert!(notice.contains("no file exists there"), "{notice}");
}

/// Asking for Git Bash by name and not getting it is a broken promise, so the
/// report says which promise broke rather than reading as a default choice.
#[test]
fn test_requested_bash_reports() {
    let candidates = standard_only(&[], &[]);
    let selection = select(ShellPreference::GitBash, &candidates, present, unchanged);
    assert_eq!(selection.program, Some(ShellProgram::Cmd));
    let notice = selection.notice.expect("a fallback is reported");
    assert!(notice.contains("asked for by name"), "{notice}");
}

/// Asking for Git Bash by name and getting it needs no report: the user asked
/// for the shell that runs.
#[test]
fn test_requested_bash_quiet() {
    let standard = paths(&[STANDARD]);
    let candidates = standard_only(&standard, &[]);
    let selection = select(ShellPreference::GitBash, &candidates, present, unchanged);
    assert_eq!(
        selection.program,
        Some(ShellProgram::GitBash(PathBuf::from(STANDARD)))
    );
    assert_eq!(selection.notice, None);
}

/// A settings value that names the interpreter never starts a search, so a
/// later search result cannot silently change what a command means.
#[test]
fn test_cmd_skips_search() {
    let standard = paths(&[STANDARD]);
    let candidates = standard_only(&standard, &[]);
    let selection = select(ShellPreference::Cmd, &candidates, present, unchanged);
    assert_eq!(selection.program, Some(ShellProgram::Cmd));
    assert_eq!(
        selection.notice, None,
        "an explicit command interpreter is not a fallback"
    );
}

/// The directory a session runs commands in and the temp directory are always
/// directories a program must stay out of, whatever the caller named, and a
/// root is kept both as named and as resolved.
#[test]
fn test_roots_add_session_dirs() {
    let named = paths(&["C:\\work\\repo"]);
    let working = Path::new("/work/session");
    let temp = Path::new("/var/tmp");
    let roots = untrusted_roots(&named, Some(working), temp);
    for expected in [
        PathBuf::from("C:\\work\\repo"),
        working.to_path_buf(),
        temp.to_path_buf(),
    ] {
        assert!(
            roots.contains(&expected),
            "missing {expected:?} in {roots:?}"
        );
    }
}

/// Each program is told which shell it is, and each takes a command line in
/// its own form: the tool layer translates for the shell named here.
#[test]
fn test_arguments_per_shell() {
    let bash = ShellProgram::GitBash(PathBuf::from(STANDARD));
    assert_eq!(bash.shell(), Shell::GitBash);
    assert_eq!(bash.arguments("echo hi"), vec!["-c", "echo hi"]);
    assert_eq!(bash.executable(), PathBuf::from(STANDARD));
    assert_eq!(ShellProgram::Cmd.shell(), Shell::Cmd);
    assert_eq!(
        ShellProgram::Cmd.arguments("echo hi"),
        vec!["/C", "echo hi"]
    );
}

/// A host that runs its own shell has no program to select, and says so when
/// the settings ask for a Windows shell.
#[cfg(not(target_os = "windows"))]
#[test]
fn test_posix_ignores_setting() {
    let selection = select_shell(ShellPreference::GitBash, None, &[]);
    assert_eq!(selection.program, None);
    let notice = selection.notice.expect("the ignored setting is reported");
    assert!(
        notice.contains("only selects a shell on Windows"),
        "{notice}"
    );
    assert_eq!(
        select_shell(ShellPreference::Auto, None, &[]).notice,
        None,
        "the default setting has nothing to report"
    );
}

/// A bash path that cannot apply is named rather than dropped, so a setting
/// shared across hosts does not silently do nothing on this one.
#[cfg(not(target_os = "windows"))]
#[test]
fn test_posix_reports_bash_path() {
    let configured = PathBuf::from("/opt/git/bin/bash.exe");
    let selection = select_shell(ShellPreference::Auto, Some(&configured), &[]);
    assert_eq!(selection.program, None);
    let notice = selection.notice.expect("the ignored path is reported");
    assert!(notice.contains("/opt/git/bin/bash.exe"), "{notice}");
}
