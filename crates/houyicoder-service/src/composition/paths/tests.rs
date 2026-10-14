//! Tests for this module.

use super::*;
use std::env::current_dir;
use std::fs;
use std::process::id;

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("houyi-paths-{tag}-{}", id()));
    drop(fs::remove_dir_all(&dir));
    fs::create_dir_all(&dir).expect("mkdir");
    dir
}

/// The explicit argument beats the environment value, matching the CLI
/// --project precedence; an empty raw value is skipped rather than
/// treated as an override.
#[test]
fn test_override_precedence() {
    let dir = temp_dir("precedence");
    let canonical = dunce::canonicalize(&dir).expect("canonical");
    let dir_str = dir.to_str().expect("dir str");

    let from_arg = override_workspace(Some(dir_str), Some("/ignored")).expect("arg wins");
    assert_eq!(from_arg, canonical);
    let from_env = override_workspace(Some(""), Some(dir_str)).expect("env wins");
    assert_eq!(from_env, canonical, "an empty argument falls to the env");
    assert!(
        override_workspace(None, Some("")).is_none(),
        "no override means the manifest walk decides"
    );
    fs::remove_dir_all(&dir).ok();
}

/// An override pointing at a directory that does not exist yet is still
/// an override, returned unchanged instead of falling through to the
/// walk.
#[test]
fn test_override_missing_passthrough() {
    let root = temp_dir("missing");
    let raw = root.join("not-created-yet");
    let raw_str = raw.to_str().expect("raw str");
    let got = override_workspace(Some(raw_str), None).expect("passthrough");
    assert_eq!(got, raw);
    fs::remove_dir_all(&root).ok();
}

/// The resolver honors an explicit argument end to end, in the plain
/// canonical form every consumer compares against.
#[test]
fn test_resolve_explicit_project() {
    let dir = temp_dir("resolve");
    let canonical = dunce::canonicalize(&dir).expect("canonical");
    let got = resolve_project_workspace(Some(dir.to_string_lossy().into_owned()))
        .expect("explicit project");
    assert_eq!(got, canonical);
    fs::remove_dir_all(&dir).ok();
}

/// The fallback canonicalizes, so a non-project dir's sessions match
/// across symlinked paths.
#[test]
fn test_cwd_fallback_canonical() {
    let cwd = current_dir().expect("cwd");
    let expected = dunce::canonicalize(&cwd).expect("canonical cwd");
    assert_eq!(cwd_fallback().expect("fallback"), expected);
}
