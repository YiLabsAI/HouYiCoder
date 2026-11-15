//! Tests for this module.

use std::ffi::OsStr;
use std::path::Path;

use super::clip_program;

/// The program is the system root's own clip, never a bare name a search
/// path could redirect.
#[test]
fn test_program_under_root() {
    let program = clip_program(Some(OsStr::new("C:\\Windows"))).expect("program");
    assert!(
        program.ends_with("System32\\clip.exe"),
        "the utility is System32's own clip: {program}"
    );
    assert!(
        Path::new(&program).is_absolute(),
        "the utility path is absolute"
    );
}

/// No system root in the environment leaves the copy to the terminal escape
/// the caller also emits.
#[test]
fn test_program_root_absent() {
    assert!(clip_program(None).is_none());
}
