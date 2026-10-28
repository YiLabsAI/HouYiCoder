//! Tests for the durable file publish: the bytes land whole, the previous
//! contents survive a failed publish, and no temporary file is left behind.

use std::env::temp_dir;
use std::fs::create_dir_all;
use std::fs::read;
use std::fs::read_dir;
use std::fs::remove_dir_all;
use std::fs::write;
use std::io::{Error as IoError, ErrorKind};
use std::path::PathBuf;
use std::process::id;

use super::{WriteError, write_atomic};

/// A directory of this test's own, removed by the caller.
fn scratch(name: &str) -> PathBuf {
    let dir = temp_dir().join(format!("durable-{name}-{}", id()));
    remove_dir_all(&dir).ok();
    create_dir_all(&dir).expect("create the scratch directory");
    dir
}

#[test]
fn test_write_publishes_whole_contents() {
    let dir = scratch("whole");
    let target = dir.join("record.json");
    write_atomic(&target, b"{\"a\":1}").expect("publish");
    assert_eq!(read(&target).expect("read back"), b"{\"a\":1}");
    let leftovers: Vec<_> = read_dir(&dir)
        .expect("list")
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "the temporary file is gone");
    remove_dir_all(&dir).ok();
}

#[test]
fn test_write_replaces_previous_contents() {
    let dir = scratch("replace");
    let target = dir.join("record.json");
    write(&target, "old").expect("seed");
    write_atomic(&target, b"new").expect("publish");
    assert_eq!(read(&target).expect("read back"), b"new");
    remove_dir_all(&dir).ok();
}

#[test]
fn test_missing_directory_reports_create() {
    let dir = scratch("missing");
    let target = dir.join("absent").join("record.json");
    let error = write_atomic(&target, b"x").expect_err("cannot publish");
    assert!(matches!(error, WriteError::Create { .. }), "got {error:?}");
    let name = error
        .path()
        .file_name()
        .expect("a file name")
        .to_string_lossy()
        .into_owned();
    assert!(
        name.starts_with(".record.json."),
        "names the temporary file"
    );
    assert!(!target.exists(), "the target was never created");
    remove_dir_all(&dir).ok();
}

#[test]
fn test_publish_onto_directory_refused() {
    let dir = scratch("onto-dir");
    let error = write_atomic(&dir, b"x").expect_err("cannot replace a directory");
    assert!(matches!(error, WriteError::Publish { .. }), "got {error:?}");
    assert!(!error.published(), "the target still holds its old state");
    assert!(dir.is_dir(), "the directory survived");
    remove_dir_all(&dir).ok();
}

#[test]
fn test_error_names_its_step() {
    let path = PathBuf::from("state.json");
    let reason = || IoError::new(ErrorKind::PermissionDenied, "denied");
    let cases = [
        (
            WriteError::Create {
                path: path.clone(),
                source: reason(),
            },
            "create",
            false,
        ),
        (
            WriteError::Write {
                path: path.clone(),
                source: reason(),
            },
            "write",
            false,
        ),
        (
            WriteError::Publish {
                path: path.clone(),
                source: reason(),
            },
            "publish",
            false,
        ),
        (
            WriteError::Sync {
                path: path.clone(),
                source: reason(),
            },
            "flush",
            true,
        ),
    ];
    for (error, step, published) in cases {
        assert_eq!(error.path(), path, "names the file it failed on");
        assert_eq!(error.published(), published, "publishing state for {step}");
        assert!(
            error.to_string().contains(step) && error.to_string().contains("state.json"),
            "the message names the step and the file: {error}"
        );
    }
}
