//! The utility runner, driven by a trivial child.

use super::run_utility;
#[cfg(target_os = "macos")]
use super::{MACOS_CLIP_TOOL, native_clipboard_preferred};

/// The macOS tool is an absolute path, never a bare name a directory earlier
/// on the search path could answer for. A regression to a bare name is a
/// working-directory hijack on copy.
#[cfg(target_os = "macos")]
#[test]
fn test_macos_tool_is_absolute() {
    assert!(
        MACOS_CLIP_TOOL.starts_with('/'),
        "the macOS tool is an absolute path"
    );
    assert_eq!(MACOS_CLIP_TOOL, "/usr/bin/pbcopy");
}

/// The host that carries a native utility is the one whose terminal may
/// refuse the escape a copy also emits. The answer is true only on macOS, so
/// the assertion is gated to it: on the other targets the function answers
/// false by design, and asserting otherwise reddens a host the code never
/// claimed to cover.
#[cfg(target_os = "macos")]
#[test]
fn test_macos_prefers_native() {
    assert!(
        native_clipboard_preferred(),
        "a desktop terminal has a utility worth running"
    );
}

/// A clean run is a successful copy, and the text reaches the utility on
/// standard input. The utility is a POSIX shell, so the assertion runs where
/// one exists.
#[cfg(unix)]
#[test]
fn test_utility_writes_stdin() {
    assert!(
        run_utility("/bin/sh", &["-c", "cat > /dev/null"], "copied").is_ok(),
        "a clean utility run is a copy"
    );
}

/// A utility that exits non-zero is reported, which is how the Linux probe
/// tells one that worked from one that did not.
#[cfg(unix)]
#[test]
fn test_utility_reports_failure() {
    assert!(
        run_utility("/bin/sh", &["-c", "exit 1"], "copied").is_err(),
        "a failed utility run is reported"
    );
}

/// A utility that cannot start is an error rather than a panic, so a host
/// missing it still copies through the terminal escape.
#[test]
fn test_utility_reports_missing() {
    assert!(
        run_utility("/nonexistent/pbcopy", &[], "copied").is_err(),
        "a missing utility is reported"
    );
}
