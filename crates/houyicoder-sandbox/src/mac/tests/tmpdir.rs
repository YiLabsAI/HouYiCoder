//! Temp-directory naming: the session scratch path has to stay short enough
//! that a build tool's Unix-domain socket fits under it.

use super::super::temp_dir::MAX_TEMP_PATH_BYTES;
use super::super::temp_dir::mkdtemp;
use std::fs::canonicalize;
use std::fs::remove_dir_all;
use std::path::Path;
use std::path::PathBuf;

/// The prefix a session gives the directory it hands out as TMPDIR.
const SESSION_PREFIX: &str = "hy-tmp-";

/// The path a session would mint from the real system temp root.
///
/// A test suite often runs inside a session that has already replaced TMPDIR
/// with its own scratch directory. Minting there nests one session directory
/// inside another, so the measured length covers two levels where production
/// has one. Removing the enclosing session directories before measuring keeps
/// the assertion about the single level a real session produces.
fn from_system_root(minted: &Path) -> PathBuf {
    let Some(leaf) = minted.file_name() else {
        return minted.to_path_buf();
    };
    let mut base = minted.parent();
    while let Some(dir) = base {
        let nested = dir
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with(SESSION_PREFIX));
        if !nested {
            break;
        }
        base = dir.parent();
    }
    match base {
        Some(root) => root.join(leaf),
        None => minted.to_path_buf(),
    }
}

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
    let measured = from_system_root(&path);
    let len = measured.as_os_str().len();
    let _cleanup = remove_dir_all(&minted);
    assert!(
        len < MAX_TEMP_PATH_BYTES,
        "session temp path is {len} bytes, over the {MAX_TEMP_PATH_BYTES}-byte budget: {}",
        measured.display()
    );
}

/// Enclosing session directories are removed from the measurement, so a suite
/// running inside one still measures the single level production mints.
#[test]
fn test_nesting_is_stripped() {
    let one = Path::new("/private/var/folders/T/hy-tmp-41437-1/hy-tmp-96686-0");
    assert_eq!(
        from_system_root(one),
        Path::new("/private/var/folders/T/hy-tmp-96686-0"),
        "one enclosing session directory is removed"
    );
    let deep = Path::new("/private/var/folders/T/hy-tmp-1-1/hy-tmp-2-2/hy-tmp-3-3");
    assert_eq!(
        from_system_root(deep),
        Path::new("/private/var/folders/T/hy-tmp-3-3"),
        "every enclosing level is removed, not only the innermost"
    );
    let plain = Path::new("/private/var/folders/T/hy-tmp-96686-0");
    assert_eq!(
        from_system_root(plain),
        Path::new("/private/var/folders/T/hy-tmp-96686-0"),
        "a path already at the system root is unchanged"
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
