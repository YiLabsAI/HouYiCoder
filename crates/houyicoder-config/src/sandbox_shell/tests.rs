//! Tests for the sandbox shell setting loader.
//!
//! The load-bearing property is that a setting which cannot be honoured is
//! reported rather than dropped: the user who writes a mode this version does
//! not implement, or a path that a later mode ignores, must be told which entry
//! had no effect. Silence here reads as a bug in the agent, not as a settings
//! mistake.

use super::*;
use std::env::temp_dir;
use std::fs::{create_dir_all, write};
use std::process::id;
use std::thread::current;

fn write_settings(dir: &Path, body: &str) -> PathBuf {
    let path = dir.join("settings.json");
    write(&path, body).expect("write settings fixture");
    path
}

fn tempdir() -> PathBuf {
    let base = temp_dir().join(format!("hc-shell-cfg-{}-{:?}", id(), current().id()));
    create_dir_all(&base).expect("create fixture dir");
    base
}

#[test]
fn test_default_searches() {
    let c = SandboxShellConfig::default();
    assert_eq!(c.mode, ShellSetting::Auto);
    assert_eq!(c.git_bash_path, None);
}

#[test]
fn test_modes_parse() {
    let dir = tempdir();
    for (body, expected) in [
        ("auto", ShellSetting::Auto),
        ("git_bash", ShellSetting::GitBash),
        ("cmd", ShellSetting::Cmd),
    ] {
        let path = write_settings(
            &dir,
            &format!(r#"{{"sandbox":{{"shell":{{"mode":"{body}"}}}}}}"#),
        );
        assert_eq!(load_sandbox_shell_from(&path).mode, expected, "{body}");
    }
}

#[test]
fn test_path_with_spaces_parses() {
    let dir = tempdir();
    let path = write_settings(
        &dir,
        r#"{"sandbox":{"shell":{"git_bash_path":"C:\\Program Files\\Git\\bin\\bash.exe"}}}"#,
    );
    assert_eq!(
        load_sandbox_shell_from(&path).git_bash_path,
        Some(PathBuf::from("C:\\Program Files\\Git\\bin\\bash.exe"))
    );
}

/// A mode this version does not implement must be named, and must not be read
/// as the command interpreter without a word about it.
#[test]
fn test_unknown_mode_reported() {
    let dir = tempdir();
    let path = write_settings(&dir, r#"{"sandbox":{"shell":{"mode":"powershell"}}}"#);
    let c = load_sandbox_shell_from(&path);
    assert_eq!(c.mode, ShellSetting::Unsupported("powershell".into()));
    let w = c.deferred_warnings();
    assert_eq!(w.len(), 1, "got {w:?}");
    assert!(
        w[0].contains("powershell"),
        "the warning names the value: {w:?}"
    );
}

/// A path under an explicit command interpreter does not take effect, so the
/// user is told instead of being left with a setting that quietly does nothing.
#[test]
fn test_path_ignored_under_cmd() {
    let c = SandboxShellConfig::new(
        ShellSetting::Cmd,
        Some(PathBuf::from("C:\\Git\\bin\\bash.exe")),
    );
    let w = c.deferred_warnings();
    assert_eq!(w.len(), 1, "got {w:?}");
    assert!(w[0].contains("git_bash_path"), "got {w:?}");
}

#[test]
fn test_missing_file_uses_default() {
    let dir = tempdir();
    let path = dir.join("absent-settings.json");
    assert_eq!(
        load_sandbox_shell_from(&path),
        SandboxShellConfig::default(),
        "an absent file leaves the search and its fallback in place"
    );
}

/// A settings file that cannot be parsed leaves the defaults in place and says
/// so: a mode the user wrote there would otherwise take effect as the default.
#[test]
fn test_corrupt_file_reported() {
    let dir = tempdir();
    let path = write_settings(&dir, r#"{"sandbox":{"shell":{"mode":"cmd",}}}"#);
    let c = load_sandbox_shell_from(&path);
    assert_eq!(c.mode, ShellSetting::Auto, "the section falls back");
    let w = c.deferred_warnings();
    assert_eq!(w.len(), 1, "got {w:?}");
    assert!(w[0].contains("could not be read"), "got {w:?}");
}

#[test]
fn test_unknown_key_reported() {
    let dir = tempdir();
    let path = write_settings(&dir, r#"{"sandbox":{"shell":{"shell_path":"x"}}}"#);
    let c = load_sandbox_shell_from(&path);
    assert_eq!(c.git_bash_path, None, "a misspelled key sets nothing");
    let w = c.deferred_warnings();
    assert_eq!(w.len(), 1);
    assert!(w[0].contains("unknown key"), "got {w:?}");
}

#[test]
fn test_acted_settings_quiet() {
    let dir = tempdir();
    let path = write_settings(
        &dir,
        r#"{"sandbox":{"shell":{"mode":"git_bash","git_bash_path":"C:\\Git\\bin\\bash.exe"}}}"#,
    );
    assert!(
        load_sandbox_shell_from(&path)
            .deferred_warnings()
            .is_empty()
    );
}
