//! Tests for this module.

use super::*;

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("houyi-diag-{tag}-{}", std::process::id()));
    drop(std::fs::remove_dir_all(&d));
    d
}

/// A path whose parent does not exist yet still opens: naming a log under
/// a fresh directory should not require creating it first.
#[test]
fn test_open_creates_parent_dirs() {
    let dir = temp_dir("parent");
    let path = dir.join("nested").join("debug.log");
    drop(open_log_file(&path).expect("open under a fresh parent"));
    assert!(path.exists(), "the log file was not created");
    drop(std::fs::remove_dir_all(&dir));
}

/// A parent that is a regular file cannot hold a log, and the caller is
/// told at once rather than discovering later that nothing was recorded.
#[test]
fn test_open_reports_bad_parent() {
    let dir = temp_dir("badparent");
    std::fs::create_dir_all(&dir).expect("create the test dir");
    let blocker = dir.join("a-file");
    std::fs::write(&blocker, b"not a directory").expect("write the blocker");
    let err = open_log_file(&blocker.join("debug.log")).expect_err("must not open");
    assert!(
        matches!(err, InstallError::File(_)),
        "expected a file error, got {err:?}"
    );
    assert!(
        err.to_string().contains("diagnostic log"),
        "the message should name what failed, got {err}"
    );
    drop(std::fs::remove_dir_all(&dir));
}

/// The already-installed case is a wiring mistake, so its message names
/// the slot that was taken instead of quoting an underlying error.
#[test]
fn test_already_installed_names_slot() {
    assert!(
        InstallError::AlreadyInstalled
            .to_string()
            .contains("already installed"),
        "unexpected message: {}",
        InstallError::AlreadyInstalled
    );
}

/// The handle turns recording on and off, and the file is the witness.
/// Asserting the handle returned Ok would prove only that a filter was
/// swapped, not that the swap changed what gets written -- and what gets
/// written is the whole point.
///
/// Driven as a scoped default so this test does not fix the subscriber
/// for the rest of the binary.
#[test]
fn test_handle_toggles_recording() {
    let dir = temp_dir("toggle");
    let path = dir.join("debug.log");
    let (subscriber, handle) = build(open_log_file(&path).expect("open"), path.clone());
    tracing::subscriber::with_default(subscriber, || {
        tracing::debug!("while disabled");
        handle.set_level(LevelFilter::DEBUG).expect("raise");
        tracing::debug!("while enabled");
        handle.disable().expect("disable");
        tracing::debug!("after disabling");
    });
    let body = std::fs::read_to_string(&path).expect("read the log");
    assert!(
        !body.contains("while disabled"),
        "a disabled sink recorded anyway: {body}"
    );
    assert!(
        body.contains("while enabled"),
        "an enabled sink recorded nothing: {body}"
    );
    assert!(
        !body.contains("after disabling"),
        "the switch latched on instead of turning off: {body}"
    );
    drop(std::fs::remove_dir_all(&dir));
}

/// The handle carries the path it was installed with, so a /debug reply
/// can show the user where the file is without guessing. Drives build
/// directly rather than install, so it does not claim the process-wide
/// subscriber slot.
#[test]
fn test_handle_carries_path() {
    let dir = temp_dir("path");
    let path = dir.join("debug.log");
    let (_subscriber, handle) = build(open_log_file(&path).expect("open"), path.clone());
    let got = handle.path().expect("the handle carries a path");
    assert_eq!(got, path, "the path is the one the handle was built with");
    drop(std::fs::remove_dir_all(&dir));
}

/// install claims the process-wide subscriber slot, stores the handle for
/// handle(), and returns it. A second call fails with AlreadyInstalled
/// because the slot is one-time. This test claims the slot for the rest
/// of the lib test process; the with_default-scoped tests above are
/// unaffected because with_default does not touch the global default.
#[test]
fn test_install_claims_rejects_second() {
    let dir = temp_dir("install");
    let path = dir.join("debug.log");
    let h1 = install(&path).expect("first install succeeds");
    // handle() now sees the stored handle.
    assert!(handle().is_some(), "handle() returns Some after install");
    // installed_path() reads the path from the stored handle.
    let got = installed_path().expect("installed_path returns Some");
    assert_eq!(got, path, "the path matches what was installed");
    // A second install fails: the global slot is already taken.
    if let Err(e) = install(&path) {
        assert!(
            matches!(e, InstallError::AlreadyInstalled),
            "expected AlreadyInstalled, got {e}"
        );
    } else {
        panic!("second install must fail with AlreadyInstalled");
    }
    // The first handle still works.
    assert!(h1.path().is_some(), "the first handle is still valid");
    drop(std::fs::remove_dir_all(&dir));
}

/// The InvalidFrame diagnostic lands the raw failing wire line + the
/// parse error in the log when the sink is on. Pins that a future field
/// rename, level mismatch, or unwired sink does not silently break
/// emission.
#[test]
fn test_log_bad_frame_writes() {
    let dir = temp_dir("badframe");
    let path = dir.join("debug.log");
    let (subscriber, handle) = build(open_log_file(&path).expect("open"), path.clone());
    let bad = "not-a-client-frame";
    let e = serde_json::from_str::<houyicoder_protocol::envelope::ClientFrame>(bad).unwrap_err();
    tracing::subscriber::with_default(subscriber, || {
        handle.set_level(LevelFilter::WARN).expect("raise");
        crate::server::log_bad_frame(bad, &e);
    });
    let body = std::fs::read_to_string(&path).expect("read the log");
    assert!(body.contains(bad), "raw line not logged: {body}");
    assert!(
        body.contains("client frame parse failed"),
        "message not logged: {body}"
    );
    drop(std::fs::remove_dir_all(&dir));
}
