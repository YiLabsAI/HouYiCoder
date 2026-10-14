//! Tests for this module.

// Construction creates a Job Object but never restricts this process --
// only children assigned to it -- so these tests run under both fence
// outcomes (see the jobobject_smoke example for the kernel-side check).

use super::*;
use std::env::temp_dir;
use std::fs::create_dir_all;
use std::fs::remove_dir_all;
use std::fs::write;
use std::process::id;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

fn unique_dir(tag: &str) -> PathBuf {
    temp_dir().join(format!(
        "houyicoder-windows-{tag}-{}-{}",
        id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ))
}

#[test]
fn test_session_constructs_workspace() {
    let session = WindowsJobSession::new().expect("session");
    assert!(session.workspace.exists());
    assert!(session.workspace.is_dir());
}

#[test]
fn test_absolute_path_rejected() {
    let session = WindowsJobSession::new().expect("session");
    let resolved = session.resolve("C:/secret");
    assert!(matches!(resolved, Err(SandboxError::PathTraversal(_))));
}

#[test]
fn test_granted_dir_admitted() {
    let session = WindowsJobSession::new().expect("session");
    let granted = unique_dir("grant");
    create_dir_all(&granted).expect("create granted dir");
    let target = granted.join("notes.txt");
    write(&target, "x").expect("write target");
    let granted_str = granted.to_str().expect("granted str");
    let target_str = target.to_str().expect("target str");

    session.add_working_dir(granted_str).expect("grant");
    let resolved = session.resolve_write(target_str).expect("admitted");
    let canonical_granted = dunce::canonicalize(&granted).expect("canonical granted");
    assert!(resolved.starts_with(canonical_granted));

    session.remove_working_dir(granted_str);
    let revoked = session.resolve_write(target_str);
    assert!(matches!(revoked, Err(SandboxError::PathTraversal(_))));

    let _cleanup = remove_dir_all(&granted);
}

#[test]
fn test_coverage_unfenced() {
    // The job object carries no path primitive, so no grant set can back
    // a writable-roots claim; the answer stays Unfenced even with grants,
    // and the gate keeps asking for consent.
    let session = WindowsJobSession::new().expect("session");
    assert!(matches!(session.coverage(), Coverage::Unfenced));
    let granted = unique_dir("coverage");
    create_dir_all(&granted).expect("create granted dir");
    let granted_str = granted.to_str().expect("granted str");
    session.add_working_dir(granted_str).expect("grant");
    assert!(matches!(session.coverage(), Coverage::Unfenced));
    session.remove_working_dir(granted_str);
    let _cleanup = remove_dir_all(&granted);
}
