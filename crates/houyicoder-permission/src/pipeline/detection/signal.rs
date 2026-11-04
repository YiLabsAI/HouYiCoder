//! Detecting a command that signals another process.
//!
//! The sandbox profile lets a signal reach any process, because the decision
//! belongs to the permission gate rather than to the kernel. That only holds if
//! the gate can see the command, so this module is what the profile relies on.
//! It lives apart from the rest of the detection stage because the walk has its
//! own vocabulary and grows on its own.

use super::{
    decode_token, expand_inline_interpreter, interpreter_inline_code, is_shell_control_keyword,
    is_shell_tool, strip_quoted_heredoc_bodies, strip_quotes,
};

/// Detect signal-sending commands in bash: kill, pkill, killall.
///
/// The sandbox profile lets a signal reach any process, because the decision
/// belongs to the permission gate rather than to the kernel. That only holds
/// if the gate can see the command, so this is the check the profile relies on.
/// It reads the command position of each segment rather than any word in the
/// text: a commit message that mentions the word is not a signal, and the
/// destructive scan's wider match is not repeated here.
pub(super) fn should_ask_signal(tool_name: &str, content: &str) -> bool {
    if !is_shell_tool(tool_name) || content.is_empty() {
        return false;
    }
    let expanded = expand_inline_interpreter(content);
    // A quoted heredoc body is literal text, so a signal word inside one is
    // not a command. The destructive scan strips the same bodies.
    let scan = strip_quoted_heredoc_bodies(&expanded);
    let lower = scan.to_ascii_lowercase();
    // Code handed to an interpreter may contain separators of its own, so the
    // whole command is offered to the extractor before the split would cut a
    // quoted body apart.
    if segment_runs_signal_code(&lower) {
        return true;
    }
    for segment in lower.split(['&', ';', '|', '\n', '(', ')', '`']) {
        let segment = segment.trim().trim_start_matches('$');
        let tokens: Vec<String> = segment
            .split_whitespace()
            .map(decode_token)
            .filter(|t| !t.is_empty() && !t.contains('=') && !is_shell_control_keyword(t))
            .collect();
        if segment_signals(&tokens) || segment_runs_signal_code(segment) {
            return true;
        }
    }
    false
}

/// Whether one segment's tokens run a signal-sending command.
///
/// The command may sit behind a wrapper that runs it verbatim, and the
/// wrapper's own flags may take a value, so the segment is accepted as a
/// command only when it starts with a verb or a wrapper, and then any token
/// may be the verb. Reading a fixed position instead missed a wrapper whose
/// flag carried an argument, and the kernel rule no longer offers a backstop
/// for a signal.
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

/// Whether the segment hands code to an interpreter and that code signals.
///
/// Code is not a shell segment, so the token walk cannot see a call inside it.
/// The interpreter may itself sit behind a wrapper, so every suffix of the
/// segment is offered to the shared extractor rather than only the first.
fn segment_runs_signal_code(segment: &str) -> bool {
    let tokens: Vec<&str> = segment.split_whitespace().collect();
    for start in 0..tokens.len() {
        let suffix = tokens[start..].join(" ");
        let Some(code) = interpreter_inline_code(&suffix) else {
            continue;
        };
        if code_signals(code) {
            return true;
        }
    }
    false
}

/// Whether interpreter code signals: either an API call, or a shell command
/// inside the code when the interpreter is a shell.
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

fn calls_signal(code: &str) -> bool {
    let code = code.to_ascii_lowercase();
    ["kill(", "killall(", ".kill(", "pkill("]
        .iter()
        .any(|call| code.contains(call))
}

/// A token reduced to the word a shell would run: quotes and the punctuation
/// that can wrap a command in place are dropped.
fn normalize_word(word: &str) -> String {
    strip_quotes(word)
        .trim_matches(|c: char| matches!(c, '$' | '{' | '}' | ',' | ';' | '\\'))
        .to_string()
}

fn is_signal_verb(word: &str) -> bool {
    matches!(word, "kill" | "pkill" | "killall")
}

/// A command that runs another command verbatim, so the real command follows
/// it. The list is the wrappers a shell user reaches for, not every program
/// that can run something.
fn is_transparent_prefix(word: &str) -> bool {
    matches!(
        word,
        "env"
            | "nice"
            | "nohup"
            | "timeout"
            | "stdbuf"
            | "setsid"
            | "command"
            | "builtin"
            | "exec"
            | "doas"
            | "sudo"
            | "xargs"
    )
}
