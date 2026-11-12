//! Tests for the shell value types and the trust rules that judge a candidate.

use std::path::{Path, PathBuf};

use crate::host::{default_shell, helper_program};

use super::*;

fn candidates<'a>(
    standard: &'a [PathBuf],
    searched: &'a [PathBuf],
    untrusted: &'a [PathBuf],
) -> Candidates<'a> {
    Candidates {
        configured: None,
        standard,
        searched,
        search: SearchOutcome::Ran,
        untrusted,
    }
}

fn yes(_: &Path) -> bool {
    true
}

fn identity(path: &Path) -> Option<PathBuf> {
    Some(path.to_path_buf())
}

#[test]
fn test_cmd_preference_returns_cmd() {
    let selection = select(
        ShellPreference::Cmd,
        &candidates(&[], &[], &[]),
        yes,
        identity,
    );
    assert_eq!(selection.program, Some(ShellProgram::Cmd));
    assert!(selection.notice.is_none());
}

#[test]
fn test_accepts_git_bash() {
    let path = PathBuf::from("C:\\Program Files\\Git\\bin\\bash.exe");
    let standard = vec![path.clone()];
    let untrusted = vec![PathBuf::from("C:/tmp")];
    let selection = select(
        ShellPreference::Auto,
        &candidates(&standard, &[], &untrusted),
        yes,
        identity,
    );
    assert_eq!(selection.program, Some(ShellProgram::GitBash(path)));
}

#[test]
fn test_refuses_wsl_launcher() {
    let path = PathBuf::from("C:\\Windows\\System32\\bash.exe");
    let selection = select(
        ShellPreference::Auto,
        &candidates(&[path], &[], &[]),
        yes,
        identity,
    );
    // The subsystem launcher is refused, so the host falls back to the
    // command interpreter rather than accepting it as Git Bash.
    assert_eq!(selection.program, Some(ShellProgram::Cmd));
    assert!(
        !matches!(selection.program, Some(ShellProgram::GitBash(_))),
        "the WSL launcher must not be accepted as Git Bash"
    );
}

#[test]
fn test_refuses_untrusted_root() {
    let path = PathBuf::from("C:\\repo\\bash.exe");
    let untrusted = vec![PathBuf::from("C:/repo")];
    let selection = select(
        ShellPreference::Auto,
        &candidates(&[path], &[], &untrusted),
        yes,
        identity,
    );
    assert_eq!(selection.program, Some(ShellProgram::Cmd));
}

#[test]
fn test_configured_refusal_named() {
    // A configured path the rules refuse is named in the notice, so a user
    // sees that their settings value did not run rather than a silent swap.
    let path = PathBuf::from("C:\\repo\\bash.exe");
    let untrusted = vec![PathBuf::from("C:/repo")];
    let candidates = Candidates {
        configured: Some(&path),
        standard: &[],
        searched: &[],
        search: SearchOutcome::Ran,
        untrusted: &untrusted,
    };
    let selection = select(ShellPreference::Auto, &candidates, yes, identity);
    let notice = selection
        .notice
        .expect("a refused configured path is named");
    assert!(
        notice.contains("refused"),
        "the notice names the refusal: {notice}"
    );
}

#[test]
fn test_falls_back_to_cmd() {
    let selection = select(
        ShellPreference::Auto,
        &candidates(&[], &[], &[]),
        yes,
        identity,
    );
    assert_eq!(selection.program, Some(ShellProgram::Cmd));
    assert!(selection.notice.is_some());
}

#[test]
fn test_arguments_per_program() {
    let cmd = ShellProgram::Cmd.arguments("echo hi");
    assert_eq!(cmd, vec!["/C".to_owned(), "echo hi".to_owned()]);
    let posix = ShellProgram::Posix.arguments("echo hi");
    assert_eq!(posix, vec!["-c".to_owned(), "echo hi".to_owned()]);
    let gitbash = ShellProgram::GitBash(PathBuf::from("C:\\bash.exe")).arguments("echo hi");
    assert_eq!(gitbash, vec!["-c".to_owned(), "echo hi".to_owned()]);
    let pwsh = ShellProgram::PowerShell {
        path: PathBuf::from("C:\\pwsh.exe"),
        edition: PwshEdition::Core,
    }
    .arguments("echo hi");
    assert_eq!(
        pwsh,
        vec![
            "-NoProfile".to_owned(),
            "-NonInteractive".to_owned(),
            "-Command".to_owned(),
            "echo hi".to_owned(),
        ]
    );
}

#[test]
fn test_shell_dialect_mapping() {
    assert_eq!(ShellProgram::Posix.shell(), Shell::Posix);
    assert_eq!(ShellProgram::Cmd.shell(), Shell::Cmd);
    assert_eq!(
        ShellProgram::GitBash(PathBuf::from("C:\\bash.exe")).shell(),
        Shell::GitBash
    );
    assert_eq!(
        ShellProgram::PowerShell {
            path: PathBuf::from("C:\\pwsh.exe"),
            edition: PwshEdition::Desktop,
        }
        .shell(),
        Shell::PowerShell(PwshEdition::Desktop)
    );
}

#[test]
fn test_under_any_contains() {
    let roots = vec![PathBuf::from("C:/repo")];
    assert!(under_any(Path::new("C:\\repo\\sub\\bash.exe"), &roots));
    assert!(!under_any(Path::new("C:\\other\\bash.exe"), &roots));
}

#[test]
fn test_executable_per_program() {
    assert_eq!(ShellProgram::Posix.executable(), PathBuf::from("sh"));
    assert_eq!(ShellProgram::Cmd.executable(), PathBuf::from("cmd"));
    let path = PathBuf::from("C:\\bash.exe");
    assert_eq!(ShellProgram::GitBash(path.clone()).executable(), path);
    assert_eq!(
        ShellProgram::PowerShell {
            path: path.clone(),
            edition: PwshEdition::Core,
        }
        .executable(),
        path
    );
}

#[test]
fn test_default_shell_returns_selection() {
    let _selection = default_shell();
}

#[test]
fn test_helper_program_sh_resolves() {
    assert_eq!(helper_program(HelperShell::Sh), Some(ShellProgram::Posix));
}

#[cfg(unix)]
#[test]
fn test_unavailable_helper_shells_none() {
    assert!(helper_program(HelperShell::Cmd).is_none());
    assert!(helper_program(HelperShell::PowerShell).is_none());
    assert!(helper_program(HelperShell::GitBash).is_none());
}
