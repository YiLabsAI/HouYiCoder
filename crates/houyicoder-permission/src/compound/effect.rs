//! Conservative checks for shell constructs and protected-path reads.
//!
//! Two questions live here. Whether a compound command only writes or removes
//! paths inside the session temporary directory, and whether a command that
//! reads a protected marker only inspects it. The first question is answered
//! against the execution fence: when the fence is active the kernel decides
//! where a write lands, so the gate only has to exclude constructs that can
//! change what the fence is asked to open.

use super::readonly::{find_matching_paren, is_readonly_compound};
use super::{is_env_assignment, split_compound, strip_safe_redirects};
use crate::git_discard::{decode_token, is_shell_control_keyword, should_ask_before_git};
use crate::heredoc::strip_quoted_heredoc_bodies;
use crate::pipeline::detection::{should_ask_egress, strip_quotes};
use crate::safety::marker_hit;

const MAX_DEPTH: u8 = 4;

const ALLOWED_ENV_NAMES: &[&str] = &[
    "CACERT",
    "RUSTC_WRAPPER",
    "HOUYICODER_FAST_TOKENS",
    "CARGO_TARGET_DIR",
    "COV_DIR",
    "LCOV",
    "i",
    "w",
    "c",
    "s",
    "e",
];

const READONLY_COMMANDS: &[&str] = &[
    "cat", "head", "tail", "grep", "egrep", "fgrep", "rg", "strings", "ls", "wc", "file", "which",
    "whereis", "basename", "dirname", "realpath", "stat", "du", "df", "pwd", "echo", "printf",
    "diff",
];

const READONLY_GIT_SUBCOMMANDS: &[&str] = &[
    "status",
    "diff",
    "show",
    "log",
    "ls-files",
    "rev-parse",
    "rev-list",
    "hash-object",
];

/// Commands a fenced session can predict. A segment that writes or removes a
/// session-temporary path is accepted only when every segment before it starts
/// with one of these, because those preceding segments are the only chance the
/// command has to plant a symbolic link the later write would follow.
///
/// Deliberately absent: anything that creates a link or runs arbitrary code
/// (ln, cp, mv, tar, install, rsync, python, shell interpreters), anything that
/// can rebind the temporary directory (getopts, printf -v, read, eval, export,
/// declare, unset), and the network clients, which the egress check owns.
const SUPPORTED_EFFECT_COMMANDS: &[&str] = &[
    "basename", "cargo", "cat", "cd", "cut", "df", "diff", "dirname", "du", "echo", "false",
    "file", "find", "git", "grep", "head", "jq", "ls", "make", "mkdir", "nohup", "pwd", "realpath",
    "rg", "rm", "rmdir", "sed", "sleep", "sort", "stat", "strings", "tail", "touch", "tr", "true",
    "uniq", "wc", "which",
];

/// Whether supported shell constructs avoid file output and nested risky
/// commands. This does not prove the behavior of arbitrary executables.
///
/// With fenced set, a write redirect or removal that targets a session
/// temporary child is accepted, because the fence decides where the open or
/// unlink actually lands. Without it, any file output still needs approval.
pub fn is_effect_safe(command: &str, fenced: bool) -> bool {
    let stripped = strip_quoted_heredoc_bodies(command);
    if sets_tmpdir(&stripped) {
        return false;
    }
    check_compound_depth(&stripped, MAX_DEPTH, fenced)
}

fn sets_tmpdir(command: &str) -> bool {
    if command.contains("+=") {
        return true;
    }
    for seg in split_compound(command) {
        for word in seg.split_whitespace() {
            if let Some((raw_name, _)) = word.split_once('=')
                && !raw_name.starts_with('-')
            {
                let name = decode_token(raw_name);
                if !ALLOWED_ENV_NAMES.contains(&name.as_str()) {
                    return true;
                }
            } else {
                break;
            }
        }
        let first = first_command_word(&seg).unwrap_or_default();
        if matches!(
            first.as_str(),
            "ln" | "pushd"
                | "popd"
                | "chroot"
                | "builtin"
                | "command"
                | "read"
                | "eval"
                | "source"
                | "."
                | "export"
                | "declare"
                | "typeset"
                | "local"
                | "unset"
        ) {
            return true;
        }
        if first == "printf"
            && seg
                .split_whitespace()
                .skip(1)
                .any(|w| decode_token(w).starts_with("-v"))
        {
            return true;
        }
    }
    let squashed: String = command
        .chars()
        .filter(|&c| c != '\'' && c != '"' && c != '\\')
        .collect();
    for var in ["TMPDIR", "TMPPREFIX"] {
        for (idx, _) in squashed.match_indices(var) {
            let before = &squashed[..idx];
            let after = &squashed[idx + var.len()..];
            if before.ends_with("environ[") {
                continue;
            }
            if before.ends_with("${") {
                if !after.starts_with('}') {
                    return true;
                }
                continue;
            }
            if before.ends_with('$') {
                if after.starts_with(['=', '[']) {
                    return true;
                }
                continue;
            }
            return true;
        }
    }
    false
}

fn check_compound_depth(command: &str, depth: u8, fenced: bool) -> bool {
    if depth == 0 {
        return false;
    }
    let segments = split_compound(command);
    if segments.is_empty() {
        return false;
    }
    let mut preceding_supported = true;
    for segment in &segments {
        if depth < MAX_DEPTH
            && (should_ask_egress("bash", segment)
                || should_ask_before_git("bash", segment).is_some())
        {
            return false;
        }
        if !segment_effect_safe(segment, depth, fenced, preceding_supported) {
            return false;
        }
        preceding_supported = preceding_supported && starts_with_supported_command(segment);
    }
    true
}

/// Whether a segment starts with a command whose effect a fenced session can
/// predict. A segment whose first word is unknown is not proof of danger, but
/// it is not proof of safety either, so it cannot precede a temporary write.
fn starts_with_supported_command(segment: &str) -> bool {
    command_word(segment).is_some_and(|w| SUPPORTED_EFFECT_COMMANDS.contains(&w.as_str()))
}

/// The command word of a segment. Leading NAME=value words are skipped by
/// shape, not by the value they carry: an assignment's value does not change
/// which program runs, and a value with an expansion in it is still an
/// assignment. Any substitution inside that value is scanned separately.
fn command_word(segment: &str) -> Option<String> {
    segment
        .split_whitespace()
        .map(decode_token)
        .find(|w| !w.is_empty() && !is_shell_control_keyword(w) && !is_assignment_word(w))
}

fn is_assignment_word(word: &str) -> bool {
    let Some((name, _)) = word.split_once('=') else {
        return false;
    };
    name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Whether a redirect target names a single child of the session temporary
/// directory. A nested path, a traversal, a glob, or an expansion cannot be
/// resolved from text, so none of them qualify.
pub(crate) fn is_temp_output_target(target: &str) -> bool {
    if marker_hit(target) {
        return false;
    }
    let rest = target
        .strip_prefix("$TMPDIR/")
        .or_else(|| target.strip_prefix("${TMPDIR}/"));
    let Some(suffix) = rest else {
        return false;
    };
    !suffix.is_empty()
        && suffix != "."
        && suffix != ".."
        && !suffix.contains(['/', '*', '?', '[', ']', '$', '`', '(', ')', '\\'])
}

fn segment_effect_safe(segment: &str, depth: u8, fenced: bool, preceding_supported: bool) -> bool {
    let cleaned = strip_safe_redirects(segment);
    if first_command_word(&cleaned).as_deref() == Some("eval") {
        return false;
    }
    let chars: Vec<char> = cleaned.chars().collect();
    let n = chars.len();
    let mut i = 0;
    let mut in_single = false;
    let mut in_double = false;
    let mut nested: Vec<String> = Vec::new();
    while i < n {
        let c = chars[i];
        if c == '\'' && !in_double {
            in_single = !in_single;
            i += 1;
            continue;
        }
        if c == '"' && !in_single {
            in_double = !in_double;
            i += 1;
            continue;
        }
        if in_single {
            i += 1;
            continue;
        }
        if c == '`' {
            return false;
        }
        if c == '$' && chars.get(i + 1) == Some(&'{') && !starts_with_tmpdir_brace(&chars, i) {
            return false;
        }
        if c == '$' && chars.get(i + 1) == Some(&'(') {
            let Some(close) = find_matching_paren(&chars, i + 1) else {
                return false;
            };
            nested.push(chars[i + 2..close].iter().collect());
            i = close + 1;
            continue;
        }
        if in_double {
            i += 1;
            continue;
        }
        if (c == '<' || c == '>') && chars.get(i + 1) == Some(&'(') {
            let Some(close) = find_matching_paren(&chars, i + 1) else {
                return false;
            };
            nested.push(chars[i + 2..close].iter().collect());
            i = close + 1;
            continue;
        }
        if c == '(' {
            let Some(close) = find_matching_paren(&chars, i) else {
                return false;
            };
            nested.push(chars[i + 1..close].iter().collect());
            i = close + 1;
            continue;
        }
        if c == '>' {
            if !(fenced && preceding_supported) {
                return false;
            }
            let after_op = if chars.get(i + 1) == Some(&'>') {
                i + 2
            } else {
                i + 1
            };
            let Some((target, next_i)) = read_redirect_token(&chars, after_op) else {
                return false;
            };
            if !is_temp_output_target(&target) {
                return false;
            }
            i = next_i;
            continue;
        }
        if c == '<' {
            let Some(next_i) = scan_redirect_at(&chars, i) else {
                return false;
            };
            i = next_i;
            continue;
        }
        i += 1;
    }
    nested
        .iter()
        .all(|body| check_compound_depth(body, depth - 1, fenced))
}

/// Whether every destructive token in the command is an rm call whose targets
/// are single children of the session temporary directory, with only
/// predictable commands before it.
pub fn removes_only_temp(command: &str, fenced: bool) -> bool {
    if !fenced {
        return false;
    }
    let stripped = strip_quoted_heredoc_bodies(command);
    if sets_tmpdir(&stripped) {
        return false;
    }
    let segments = split_compound(&stripped);
    if segments.is_empty() {
        return false;
    }
    let mut saw_rm = false;
    let mut preceding_supported = true;
    for segment in &segments {
        if is_rm_segment(segment) {
            saw_rm = true;
            if !preceding_supported || !rm_targets_are_temp(segment) {
                return false;
            }
        }
        preceding_supported = preceding_supported && starts_with_supported_command(segment);
    }
    saw_rm
}

fn is_rm_segment(segment: &str) -> bool {
    first_command_word(segment).as_deref() == Some("rm")
}

fn rm_targets_are_temp(segment: &str) -> bool {
    let cleaned = strip_safe_redirects(segment);
    let mut saw_target = false;
    for token in cleaned.split_whitespace().skip(1) {
        if is_env_assignment(token) {
            return false;
        }
        let word = strip_quotes(token);
        if word.is_empty() || word == "--" {
            continue;
        }
        if word.starts_with('-') {
            continue;
        }
        saw_target = true;
        if !is_temp_output_target(word) {
            return false;
        }
    }
    saw_target
}

fn scan_redirect_at(chars: &[char], i: usize) -> Option<usize> {
    let c = chars[i];
    if c == '<' && chars.get(i + 1) == Some(&'<') {
        let after = chars.get(i + 2).copied();
        return if matches!(after, Some('\'' | '"' | '-' | '\\')) {
            Some(i + 2)
        } else {
            None
        };
    }
    let (target, next_i) = read_redirect_token(chars, i + 1)?;
    is_safe_input_redirect(&target).then_some(next_i)
}

fn read_redirect_token(chars: &[char], start: usize) -> Option<(String, usize)> {
    let mut i = start;
    while i < chars.len() && matches!(chars[i], ' ' | '\t') {
        i += 1;
    }
    if i >= chars.len() {
        return None;
    }
    let first = chars[i];
    if first == '"' || first == '\'' {
        let mut j = i + 1;
        while j < chars.len() && chars[j] != first {
            j += 1;
        }
        if j >= chars.len() {
            return None;
        }
        let end = j + 1;
        if !matches!(
            chars.get(end),
            None | Some(' ' | '\t' | '\n' | '\r' | ';' | '|' | '&' | ')')
        ) {
            return None;
        }
        let token: String = chars[i + 1..j].iter().collect();
        return Some((token, end));
    }
    let mut j = i;
    while j < chars.len()
        && !matches!(
            chars[j],
            ' ' | '\t' | '\n' | '\r' | ';' | '|' | '&' | ')' | '<' | '>'
        )
    {
        if matches!(chars[j], '"' | '\'') {
            return None;
        }
        j += 1;
    }
    if j == i {
        return None;
    }
    let token: String = chars[i..j].iter().collect();
    Some((token, j))
}

fn is_safe_input_redirect(target: &str) -> bool {
    !target.is_empty()
        && !target.starts_with("/etc/")
        && !target.starts_with("/private/etc/")
        && !target.contains("..")
        && !target.contains('$')
        && !target.contains('`')
        && !target.contains('(')
        && !marker_hit(target)
}

fn starts_with_tmpdir_brace(chars: &[char], start: usize) -> bool {
    const TMPDIR_BRACE: [char; 9] = ['$', '{', 'T', 'M', 'P', 'D', 'I', 'R', '}'];
    chars.get(start..start + TMPDIR_BRACE.len()) == Some(&TMPDIR_BRACE)
}

/// Whether a shell command that references a protected marker only inspects it
/// with read-only commands and writes nothing to a protected path.
pub fn reads_only_protected(command: &str) -> bool {
    let stripped = strip_quoted_heredoc_bodies(command);
    let segments = split_compound(&stripped);
    if segments.is_empty() {
        return false;
    }
    let mut saw_protected = false;
    for segment in &segments {
        if !marker_hit(segment) {
            continue;
        }
        saw_protected = true;
        if !is_readonly_protected_segment(segment) {
            return false;
        }
    }
    saw_protected
}

fn is_readonly_protected_segment(segment: &str) -> bool {
    let cleaned = strip_safe_redirects(segment);
    if cleaned.contains('>')
        || cleaned.contains('<')
        || cleaned.contains('`')
        || cleaned.contains("$(")
    {
        return false;
    }
    let mut words: Vec<String> = Vec::new();
    for raw in cleaned.split_whitespace() {
        if is_env_assignment(raw) {
            continue;
        }
        let Some(decoded) = decode_shell_word(raw) else {
            return false;
        };
        words.push(decoded);
    }
    let Some(cmd) = words.first().map(String::as_str) else {
        return false;
    };
    if cmd == "printf" {
        return !words.iter().skip(1).any(|w| w.starts_with("-v"));
    }
    if cmd == "sort" || cmd == "uniq" {
        return is_readonly_compound(&[segment]);
    }
    if READONLY_COMMANDS.contains(&cmd) {
        return true;
    }
    if cmd == "find" {
        return !words.iter().any(|w| {
            matches!(
                w.as_str(),
                "-delete"
                    | "-exec"
                    | "-execdir"
                    | "-ok"
                    | "-okdir"
                    | "-fprint"
                    | "-fprint0"
                    | "-fprintf"
                    | "-fls"
            )
        });
    }
    if cmd == "git" {
        let sub = words.get(1).map(String::as_str).unwrap_or("");
        let has_write_or_exec_flag = words.iter().skip(2).any(|arg| {
            arg.starts_with("-o")
                || arg.starts_with("--output")
                || arg.starts_with("--ext-diff")
                || arg.starts_with("-c")
                || arg.starts_with("--exec-path")
        });
        return READONLY_GIT_SUBCOMMANDS.contains(&sub) && !has_write_or_exec_flag;
    }
    false
}

fn decode_shell_word(raw: &str) -> Option<String> {
    if raw.contains(['$', '`']) {
        return None;
    }
    let mut out = String::with_capacity(raw.len());
    let chars: Vec<char> = raw.chars().collect();
    let mut i = 0;
    let mut in_single = false;
    let mut in_double = false;
    while i < chars.len() {
        let c = chars[i];
        if c == '\'' && !in_double {
            in_single = !in_single;
            i += 1;
            continue;
        }
        if c == '"' && !in_single {
            in_double = !in_double;
            i += 1;
            continue;
        }
        if c == '\\' && !in_single {
            let next = *chars.get(i + 1)?;
            out.push(next);
            i += 2;
            continue;
        }
        out.push(c);
        i += 1;
    }
    if in_single || in_double {
        None
    } else {
        Some(out)
    }
}

fn first_command_word(segment: &str) -> Option<String> {
    segment
        .split_whitespace()
        .filter(|w| !is_env_assignment(w))
        .map(decode_token)
        .find(|w| !w.is_empty() && !is_shell_control_keyword(w))
}
