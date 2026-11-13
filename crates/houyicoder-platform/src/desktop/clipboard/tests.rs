//! The utility runner, driven by a trivial child.

use super::{native_clipboard_preferred, run_utility};

/// The host that carries a native utility is the one whose terminal may
/// refuse the escape a copy also emits.
#[test]
fn test_macos_prefers_native() {
    assert!(
        native_clipboard_preferred(),
        "a desktop terminal has a utility worth running"
    );
}

/// A clean run is a successful copy, and the text reaches the utility on
/// standard input.
#[test]
fn test_utility_writes_stdin() {
    assert!(
        run_utility("/bin/sh", &["-c", "cat > /dev/null"], "copied").is_ok(),
        "a clean utility run is a copy"
    );
}

/// A utility that exits non-zero is reported, which is how the Linux probe
/// tells one that worked from one that did not.
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
