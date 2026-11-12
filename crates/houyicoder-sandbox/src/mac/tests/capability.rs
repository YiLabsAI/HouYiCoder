//! What the seatbelt fence declares it enforces, per axis, and the revision
//! counter a directory install reads before and after.

use super::super::fence::capability_for;
use super::super::*;
use houyicoder_api::sandbox::{
    Containment, FenceAxis, SandboxSession, is_within_bounds, normalize_tool_path,
};
use std::env::temp_dir;
use std::fs::create_dir_all;
use std::fs::remove_dir_all;
use std::process::id;

/// The filesystem axes are enforced by the profile itself; the network axis
/// follows the session policy, because a contained policy emits the deny and
/// an open one leaves every destination reachable. Read through the pure
/// function so the decision is tested without the process-wide no-enforce
/// hatch the gate runner sets.
#[test]
fn test_capability_follows_network() {
    let contained = capability_for(false, false);
    assert!(contained.confines(FenceAxis::Writes));
    assert!(contained.confines(FenceAxis::Reads));
    assert!(
        contained.confines(FenceAxis::Network),
        "a contained policy makes the kernel decide egress"
    );

    let open = capability_for(false, true);
    assert!(
        !open.confines(FenceAxis::Network),
        "an open policy contains nothing on the network axis"
    );
}

/// When the fence is not enforced, no axis is contained, so a shell write is
/// not downgraded by a claim the kernel is not backing.
#[test]
fn test_unfenced_claims_no_axis() {
    let capability = capability_for(true, false);
    assert!(!capability.confines(FenceAxis::Writes));
    assert!(!capability.confines(FenceAxis::Reads));
    assert!(!capability.confines(FenceAxis::Network));
}

/// An enforced fence claims the file axes regardless of the network policy.
#[test]
fn test_enforced_claims_file_axes() {
    let capability = capability_for(false, true);
    assert!(capability.confines(FenceAxis::Writes));
    assert!(capability.confines(FenceAxis::Reads));
}

/// A directory grant that changes the fence advances the revision; a repeated
/// grant and a revoke that changes nothing do not, so a caller comparing the
/// counter before and after an install learns whether it took effect.
#[test]
fn test_grant_advances_revision() {
    let scratch = temp_dir().join(format!("houyi-capability-{}", id()));
    create_dir_all(&scratch).expect("mkdir scratch");
    let target = scratch.to_string_lossy();
    let session = MacSeatbeltSession::new().expect("session");

    let before = session.capability_revision();
    session.add_working_dir(&target).expect("grant write");
    let after_write = session.capability_revision();
    assert!(after_write > before, "a new write grant widens the fence");

    session.add_working_dir(&target).expect("repeat grant");
    assert_eq!(
        session.capability_revision(),
        after_write,
        "a grant the fence already holds changes nothing"
    );

    session.add_reading_dir(&target).expect("grant read");
    let after_read = session.capability_revision();
    assert_eq!(
        after_read, after_write,
        "a read grant on a directory already granted for writing adds nothing"
    );

    session.remove_working_dir(&target);
    let after_revoke = session.capability_revision();
    assert!(
        after_revoke > after_read,
        "a revoked grant narrows the fence"
    );

    let untouched = scratch.join("never-granted");
    create_dir_all(&untouched).expect("mkdir untouched");
    session.remove_working_dir(&untouched.to_string_lossy());
    assert_eq!(
        session.capability_revision(),
        after_revoke,
        "a revoke that removes nothing changes no capability"
    );

    remove_dir_all(&scratch).ok();
}

/// The shared temp root is reported as inside the fence, because the profile
/// grants it outright. A tool that writes there is not escaping the workspace,
/// so the gate must have nothing to ask about.
#[test]
fn test_shared_temp_in_bounds() {
    let session = MacSeatbeltSession::new().expect("session");
    let roots = session.shared_temp_roots();
    assert!(
        !roots.is_empty(),
        "the profile grants the shared temp root, so it is reported"
    );
    let shared = PathBuf::from("/tmp");
    let canonical = dunce::canonicalize(&shared).unwrap_or(shared);
    assert!(
        roots.contains(&canonical),
        "the canonical shared temp root is among them: {roots:?}"
    );
}

/// One directory is reported once. The two spellings of the shared temp root
/// name the same directory here, so listing both would put a duplicate in the
/// bounds the gate reads and in whatever a user-facing report walks them.
#[test]
fn test_shared_temp_listed_once() {
    let session = MacSeatbeltSession::new().expect("session");
    let roots = session.shared_temp_roots();
    let mut unique = roots.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(
        unique.len(),
        roots.len(),
        "no directory appears twice: {roots:?}"
    );

    // The two spellings must resolve to one entry, not two.
    let canonical: Vec<PathBuf> = roots
        .iter()
        .map(|root| dunce::canonicalize(root).unwrap_or_else(|_| root.clone()))
        .collect();
    let mut distinct = canonical.clone();
    distinct.sort();
    distinct.dedup();
    assert_eq!(
        distinct.len(),
        canonical.len(),
        "and no two entries resolve to the same directory: {roots:?}"
    );
}

/// A path spelled through the short form of the shared temp root still reads
/// as inside once both sides are resolved. The bounds hold the canonical
/// directory, and a tool names whichever spelling the user typed, so the
/// comparison has to happen after resolution rather than on the raw strings.
#[test]
fn test_short_spelling_in_bounds() {
    let session = MacSeatbeltSession::new().expect("session");
    let roots = session.shared_temp_roots();
    let workspace = session.workspace_root();

    let short = PathBuf::from("/tmp/houyi-probe.txt");
    let resolved = normalize_tool_path(&short).expect("resolve");
    assert!(
        is_within_bounds(&resolved, &workspace, &roots),
        "a path typed through the short spelling is inside: resolved {resolved:?} roots {roots:?}"
    );
}
