//! Tests for the Windows shell resolution helpers.

use super::*;

#[test]
fn test_command_interpreter_is_cmd() {
    let cmd = command_interpreter();
    let text = cmd.to_string_lossy();
    assert!(
        text.ends_with("cmd.exe") || text == "cmd",
        "the interpreter is cmd: {text}"
    );
}
