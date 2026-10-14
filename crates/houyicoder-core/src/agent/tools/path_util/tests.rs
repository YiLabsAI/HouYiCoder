//! Tests for this module.

use super::*;

/// A path inside the root confines to itself. Pins the happy path so a
/// regression that over-rejects is caught.
#[test]
fn test_confine_path_accepts_inside() {
    let dir = temp_dir().join(format!("path-util-inside-{}", id()));
    create_dir_all(&dir).expect("mkdir");
    let file = dir.join("a.txt");
    write(&file, b"x").expect("write");
    let confined = confine_path(&dir, &[], "a.txt").expect("inside root");
    assert_eq!(confined, canonicalize(&file).unwrap());
    remove_dir_all(&dir).ok();
}

/// Forward slashes whatever the host separator is. Built with join so
/// strip_prefix sees a real platform path (backslash on Windows).
#[test]
fn test_relativize_normalizes_separators() {
    let root = temp_dir().join("path-util-sep-root");
    let nested = root.join("src").join("a.rs");
    assert_eq!(relativize(&root, &nested), "src/a.rs");
}

/// Under root strips to the remainder; outside root keeps the full path.
#[test]
fn test_relativize_strips_root() {
    let root = Path::new("/workspace");
    let file = Path::new("/workspace/src/main.rs");
    assert_eq!(relativize(root, file), "src/main.rs");
    let outside = Path::new("/etc/passwd");
    assert_eq!(relativize(root, outside), "/etc/passwd");
}

/// A parent-dir escape is rejected lexically without touching the
/// escaped target, so the wording is stable whether or not the target
/// exists outside the workspace.
#[test]
fn test_confine_path_rejects_escape() {
    let dir = temp_dir().join(format!("path-util-esc-{}", id()));
    create_dir_all(&dir).expect("mkdir");
    let err = confine_path(&dir, &[], "../../etc").expect_err("escape rejected");
    assert!(
        matches!(err, ToolError::PathEscapes(_)),
        "escape surfaces as PathEscapes"
    );
    remove_dir_all(&dir).ok();
}

/// require_dir rejects a file path with a clear error naming the path.
#[test]
fn test_require_dir_rejects_file() {
    let dir = temp_dir().join(format!("path-util-dir-{}", id()));
    create_dir_all(&dir).expect("mkdir");
    let file = dir.join("a.txt");
    write(&file, b"x").expect("write");
    assert!(require_dir(&file).is_err(), "a file is not a directory");
    assert!(require_dir(&dir).is_ok(), "the dir itself is a directory");
    remove_dir_all(&dir).ok();
}

/// The shared boundary predicate: a canonical candidate is within bounds
/// under the root or an additional authorized dir; outside both it is not.
/// Single source of truth for confine_path + the gate so the two layers
/// cannot drift on what "inside the workspace" means.
#[test]
fn test_within_bounds_covers_extra() {
    let root = temp_dir().join(format!("path-util-bounds-{}", id()));
    create_dir_all(&root).expect("mkdir");
    let extra = temp_dir().join(format!("path-util-extra-{}", id()));
    create_dir_all(&extra).expect("mkdir extra");
    let croot = canonicalize(&root).unwrap();
    let cextra = canonicalize(&extra).unwrap();
    let additional = vec![cextra.clone()];
    assert!(
        is_within_bounds(&croot.join("a.txt"), &croot, &additional),
        "inside root is within bounds"
    );
    assert!(
        is_within_bounds(&cextra.join("b.txt"), &croot, &additional),
        "inside an additional dir is within bounds"
    );
    let outside = temp_dir().join(format!("path-util-out-{}", id()));
    assert!(
        !is_within_bounds(&outside, &croot, &additional),
        "outside both is not within bounds"
    );
    remove_dir_all(&root).ok();
    remove_dir_all(&extra).ok();
}
