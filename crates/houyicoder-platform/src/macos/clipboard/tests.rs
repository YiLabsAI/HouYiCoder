//! Tests for this module. The macOS parent gates the module, so the file
//! carries no platform conditional.

use std::path::Path;
use std::process::{Command, Stdio};

use super::{CLIP_TOOL, copy};

/// The real clipboard round trip: the copy tool writes what the paste tool
/// reads back. Ignored by default because it overwrites the user's
/// clipboard; run it on hardware when the delivery path changes.
#[expect(
    clippy::disallowed_methods,
    reason = "test scaffolding that reads the clipboard the copy wrote"
)]
#[test]
#[ignore = "writes the real system clipboard; run on hardware"]
fn test_clipboard_round_trips() {
    let text = "中文剪贴板 round trip\n第二行";
    copy(text).expect("the copy tool runs");
    let paste = Command::new("/usr/bin/pbpaste")
        .stdin(Stdio::null())
        .output()
        .expect("the paste tool runs");
    assert_eq!(
        String::from_utf8_lossy(&paste.stdout),
        text,
        "the clipboard holds the copied text"
    );
}

/// The tool path is absolute, so the copy cannot pick up a same-named
/// program earlier on the search path.
#[test]
fn test_tool_path_is_absolute() {
    assert!(
        Path::new(CLIP_TOOL).is_absolute(),
        "the paste tool is named by absolute path"
    );
}
