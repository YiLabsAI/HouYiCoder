//! Tests for this module.

use super::arguments;

/// Each answering tool pairs with the arguments that reach the clipboard,
/// and an unknown one takes none rather than borrowing another's.
#[test]
fn test_arguments_per_tool() {
    assert_eq!(arguments("xclip"), &["-selection", "clipboard"] as &[&str]);
    assert_eq!(arguments("xsel"), &["--clipboard", "--input"]);
    assert_eq!(arguments("wl-copy"), &[] as &[&str]);
}
