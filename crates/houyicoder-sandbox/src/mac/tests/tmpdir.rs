//! Temp-directory naming: the session scratch path has to stay short enough
//! that a build tool's Unix-domain socket fits under it.

use super::super::temp_dir::MAX_TEMP_PATH_BYTES;
use super::super::temp_dir::mkdtemp;
use std::fs::canonicalize;
use std::fs::remove_dir_all;

/// The minted session temp path leaves room for a socket name under SUN_LEN.
/// A path that eats that budget fails every build in the session, and the
/// error names the socket rather than the temp dir, so the length is asserted
/// at the source instead.
#[test]
fn test_temp_path_is_short() {
    let minted = mkdtemp("hy-tmp").expect("mint temp dir");
    // The session canonicalizes before exporting TMPDIR, and on macOS that
    // turns /var into /private/var: measure the path the child actually gets,
    // not the shorter spelling minted here.
    let path = canonicalize(&minted).expect("canonicalize");
    let len = path.as_os_str().len();
    let _cleanup = remove_dir_all(&minted);
    assert!(
        len < MAX_TEMP_PATH_BYTES,
        "session temp path is {len} bytes, over the {MAX_TEMP_PATH_BYTES}-byte budget: {}",
        path.display()
    );
}

/// Two mints in one process never collide, so a session cannot inherit a
/// previous session's scratch directory.
#[test]
fn test_temp_dirs_are_distinct() {
    let first = mkdtemp("hy-tmp").expect("first");
    let second = mkdtemp("hy-tmp").expect("second");
    assert_ne!(first, second);
    let _a = remove_dir_all(&first);
    let _b = remove_dir_all(&second);
}
