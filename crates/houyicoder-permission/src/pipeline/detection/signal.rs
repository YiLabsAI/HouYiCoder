//! Detecting a command that signals another process.
//!
//! The sandbox profile lets a signal reach any process, because the decision
//! belongs to the permission gate rather than to the kernel. That only holds if
//! the gate can see the command, so this module is what the profile relies on.
//! It lives apart from the rest of the detection stage because the concern has
//! its own vocabulary: a signal can arrive as a shell command, as a wrapped
//! command, or as a library call inside the code an interpreter is handed.

use super::is_shell_tool;
use crate::shell::{
    ShellCommand, canonical_scan, decode_token, is_launcher, is_shell_control_keyword, program_name,
};

/// Detect signal-sending commands in bash: kill, pkill, killall.
///
/// The sandbox profile lets a signal reach any process, because the decision
/// belongs to the permission gate rather than to the kernel. That only holds
/// if the gate can see the command, so this is the check the profile relies on.
/// It reads the command position of each stage and the code handed to an
/// interpreter, never an arbitrary word in the text: a commit message that
/// mentions the word is not a signal.
pub(super) fn should_ask_signal(tool_name: &str, content: &str) -> bool {
    if !is_shell_tool(tool_name) || content.is_empty() {
        return false;
    }
    let scan = canonical_scan(content);
    if scan.commands().iter().any(command_signals) {
        return true;
    }
    // Code handed to an interpreter outside the shell family is not a shell
    // stage, so a library call in it cannot be a command. The shell family's
    // code was walked as shell text, so it is already covered above.
    scan.codes().iter().any(|code| code_signals(code))
}

/// Whether one invocation sends a signal. The program may sit behind a prefix
/// that runs the command written after its own arguments, in which case the
/// verb follows that prefix.
fn command_signals(command: &ShellCommand) -> bool {
    let program = program_name(command.program()).to_ascii_lowercase();
    if is_signal_verb(&program) {
        return true;
    }
    is_transparent_prefix(&program)
        && command
            .args()
            .iter()
            .any(|word| is_signal_verb(&normalize_word(word)))
}

/// Whether interpreter code signals: either an API call, or a shell command
/// inside the code.
fn code_signals(code: &str) -> bool {
    if calls_signal(code) {
        return true;
    }
    let lower = code.to_ascii_lowercase();
    for segment in lower.split(['&', ';', '|', '\n', '(', ')', '`']) {
        let tokens: Vec<String> = segment
            .split_whitespace()
            .map(decode_token)
            .filter(|t| !t.is_empty() && !t.contains('=') && !is_shell_control_keyword(t))
            .collect();
        if segment_signals(&tokens) {
            return true;
        }
    }
    false
}

/// Whether one code segment runs a signal-sending command.
fn segment_signals(tokens: &[String]) -> bool {
    let Some(first) = tokens.first().map(|t| normalize_word(t)) else {
        return false;
    };
    // A find action names the command it runs after its own flag.
    if first == "find" {
        return tokens.windows(2).any(|pair| {
            matches!(
                normalize_word(&pair[0]).as_str(),
                "-exec" | "-execdir" | "-ok"
            ) && is_signal_verb(&normalize_word(&pair[1]))
        });
    }
    if !is_signal_verb(&first) && !is_transparent_prefix(&first) {
        return false;
    }
    tokens.iter().any(|t| is_signal_verb(&normalize_word(t)))
}

fn calls_signal(code: &str) -> bool {
    let code = code.to_ascii_lowercase();
    ["kill(", "killall(", ".kill(", "pkill("]
        .iter()
        .any(|call| code.contains(call))
}

/// A word reduced to the name a shell would run: the punctuation that can wrap
/// a command in place is dropped. Quoting is already resolved by the scan.
fn normalize_word(word: &str) -> String {
    word.trim_matches(|c: char| matches!(c, '$' | '{' | '}' | ',' | ';' | '\\'))
        .to_ascii_lowercase()
}

fn is_signal_verb(word: &str) -> bool {
    matches!(word, "kill" | "pkill" | "killall")
}

/// A command that runs another command verbatim, so the real command follows
/// it. The launcher table answers for the programs the canonical scan already
/// steps over, and the two shell builtins answer for the words it does not.
fn is_transparent_prefix(word: &str) -> bool {
    is_launcher(word) || matches!(word, "builtin" | "xargs")
}
