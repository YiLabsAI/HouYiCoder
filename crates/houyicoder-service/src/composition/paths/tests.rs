//! Tests for this module.

use super::*;
use std::env::current_dir;
use std::env::var;
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

/// The snapshot store sits outside the workspace when the home directory shares
/// the workspace's volume, so the agent's own find and grep do not walk cloned
/// trees. A store inside the tree both slows those searches and returns hits
/// from files that are not the working tree.
/// The store leaves the workspace when the config home shares its volume, and
/// stays workspace-local when it does not (clonefile needs one volume). Both
/// branches are asserted from the decision the function makes, so the test
/// holds on a host whose temp root is a separate filesystem.
#[test]
fn test_snapshot_store_location() {
    let dir = temp_dir("snap-store");
    let root = snapshot_store_root(&dir);
    let home_store = var("HOME")
        .ok()
        .map(PathBuf::from)
        .map(|home| home.join(".houyicoder").join("snapshots"));
    match home_store.filter(|store| same_filesystem(&dir, store)) {
        Some(store) => {
            assert_eq!(
                root,
                store.join(workspace_slug(&dir)),
                "same filesystem: the store sits under the config home, keyed by slug"
            );
            assert!(!root.starts_with(&dir), "and outside the workspace");
        }
        None => assert_eq!(
            root,
            dir.join(".houyicoder").join("snapshots"),
            "different filesystem: the store stays beside the workspace"
        ),
    }
    drop(fs::remove_dir_all(&dir));
}

/// Two workspaces with the same directory name get different stores: the slug
/// carries a hash of the full path, not just the last component.
#[test]
fn test_store_slug_keys_path() {
    let first = workspace_slug(Path::new("/work/a/repo"));
    let second = workspace_slug(Path::new("/work/b/repo"));
    assert_ne!(first, second, "same name, different path, different slug");
    assert!(first.starts_with("repo-"), "slug keeps the name: {first}");
    assert!(
        first
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.'),
        "slug must be a safe directory name: {first}"
    );
}

/// A workspace whose name carries separators or spaces still yields a safe
/// directory name.
#[test]
fn test_store_slug_replaces_unsafe() {
    let slug = workspace_slug(Path::new("/work/we ird:name"));
    assert!(
        slug.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.'),
        "unsafe characters must be replaced: {slug}"
    );
}

/// A long workspace name is capped so the store directory stays inside a
/// filesystem's name limit, and an empty name still yields a usable slug.
#[test]
fn test_store_slug_bounds() {
    let long = "a".repeat(200);
    let slug = workspace_slug(&PathBuf::from(format!("/work/{long}")));
    assert!(slug.len() <= 48 + 17, "slug stays bounded: {}", slug.len());
    assert!(
        slug.starts_with(&"a".repeat(48)),
        "the cap keeps the leading name: {slug}"
    );
    let root = workspace_slug(Path::new("/"));
    assert!(
        root.starts_with("workspace-"),
        "a path with no file name still yields a slug: {root}"
    );
}
