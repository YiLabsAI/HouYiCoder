//! Tests for this module.

use super::*;
use std::fs;

fn settings_file(tag: &str) -> PathBuf {
    let dir = temp_dir().join(format!("trust-cfg-{tag}-{}", id()));
    drop(fs::remove_dir_all(&dir));
    fs::create_dir_all(&dir).unwrap();
    dir.join("settings.json")
}

/// Persisting a path then checking it returns true, and the write
/// preserves other keys in the settings file.
#[test]
fn test_persist_then_trusted() {
    let path = settings_file("roundtrip");
    // Seed an unrelated key so the merge-preserving write must keep it.
    fs::write(&path, r#"{"model":{"id":"opencoder"}}"#).unwrap();
    let proj = temp_dir().join("trust-proj-roundtrip");
    drop(fs::remove_dir_all(&proj));
    fs::create_dir_all(&proj).unwrap();

    assert!(!is_path_trusted(&path, &proj), "not trusted before persist");
    persist_project_trust(&path, &proj).unwrap();
    assert!(is_path_trusted(&path, &proj), "trusted after persist");

    // The unrelated key survives the merge-preserving write.
    let after: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(after["model"]["id"], "opencoder", "other keys preserved");

    drop(fs::remove_dir_all(path.parent().expect("parent exists")));
    drop(fs::remove_dir_all(&proj));
}

/// Trusting a parent directory trusts its children without a second ask
/// (ancestor walk-up): persist the parent, ask about a child, get true.
#[test]
fn test_ancestor_trust_covers_child() {
    let path = settings_file("ancestor");
    let root = temp_dir().join("trust-ancestor-root");
    drop(fs::remove_dir_all(&root));
    let parent = root.join("parent");
    let child = parent.join("child");
    fs::create_dir_all(&child).unwrap();

    persist_project_trust(&path, &parent).unwrap();
    assert!(
        is_path_trusted(&path, &child),
        "trusting the parent must cover the child"
    );
    assert!(is_path_trusted(&path, &parent), "parent itself trusted");

    drop(fs::remove_dir_all(&root));
    drop(fs::remove_dir_all(path.parent().unwrap()));
}

/// A path with no recorded trust returns false, and a missing settings
/// file also returns false (no crash, no trust).
#[test]
fn test_untrusted_returns_false() {
    let path = settings_file("none");
    let proj = temp_dir().join("trust-proj-none");
    drop(fs::remove_dir_all(&proj));
    fs::create_dir_all(&proj).unwrap();

    assert!(!is_path_trusted(&path, &proj), "no trust recorded");

    // A settings file that does not exist at all: still no crash, false.
    let missing = path.parent().unwrap().join("absent.json");
    assert!(
        !is_path_trusted(&missing, &proj),
        "missing settings file yields false, not a crash"
    );

    drop(fs::remove_dir_all(proj));
    drop(fs::remove_dir_all(path.parent().unwrap()));
}
