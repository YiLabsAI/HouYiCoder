//! Conservative checks for shell constructs and protected-path reads.
//!
//! Two questions live here. Whether a compound command only writes or removes
//! paths inside the session temporary directory, and whether a command that
//! reads a protected marker only inspects it. The first question is answered
//! against the execution fence: when the fence confines writes the kernel
//! decides where a write lands, so the gate only has to exclude constructs
//! that can change what the fence is asked to open.

use houyicoder_api::sandbox::names_credential_path;
use houyicoder_api::shell::heredoc::strip_quoted_heredoc_bodies;

use super::readonly::find_matching_paren;
use super::temp_cwd::{TempCwd, chain_links, scratch_delete};
use super::{is_env_assignment, quotes_balanced, split_compound, strip_safe_redirects};
use crate::git_safety::should_ask_before_git;
use crate::pipeline::detection::{should_ask_egress, strip_quotes};
use crate::safety::marker_hit;
use crate::shell::{
    Stage, decode_token, leading_command_substitution, program_is_unresolved, stage_program,
    substitution_bodies,
};

const MAX_DEPTH: u8 = 4;

/// Environment variables whose assignment can rebind the temporary directory,
/// the loader search path, the shell startup file, or repository identity. A
/// leading assignment to one of these is treated as a tmpdir-setter because the
/// same argv would no longer describe what runs. Names outside this set are
/// ordinary local variables a compound command may set without changing the
/// execution environment.
const DANGEROUS_ENV_NAMES: &[&str] = &[
    "TMPDIR",
    "TMPPREFIX",
    "PATH",
    "IFS",
    "BASH_ENV",
    "ENV",
    "CDPATH",
    "GLOBIGNORE",
    "SHELLOPTS",
    "BASHOPTS",
];

/// Whether an environment variable name can rebind the temporary directory or
/// the loader, shell, or repository configuration. Prefixed families cover the
/// dynamic loader and the git identity and path settings.
fn is_dangerous_env_name(name: &str) -> bool {
    DANGEROUS_ENV_NAMES.contains(&name)
        || name.starts_with("LD_")
        || name.starts_with("DYLD_")
        || name.starts_with("GIT_")
}

/// Commands a fenced session can predict. A segment that writes or removes a
/// session-temporary path is accepted only when every segment before it starts
/// with one of these, because those preceding segments are the only chance the
/// command has to plant a symbolic link the later write would follow.
///
/// cp is admitted only by the argument rule in segment_effect_safe, which
/// requires its destination to name a session-temporary child or a safe
/// workspace relative path; the name alone never clears it.
///
/// Deliberately absent: anything that creates a link or runs arbitrary code
/// (ln, mv, tar, install, rsync, python, shell interpreters), anything that
/// can rebind the temporary directory (getopts, printf -v, eval, export,
/// declare, unset), and the network clients, which the egress check owns.
/// read is admitted only while the names it assigns are ordinary variables;
/// assigning a name that rebinds the temporary directory or the loader is
/// judged by the same rule as an environment assignment.
const SUPPORTED_EFFECT_COMMANDS: &[&str] = &[
    ":", "basename", "cargo", "cat", "cd", "cp", "cut", "df", "diff", "dirname", "du", "echo",
    "false", "file", "find", "git", "grep", "head", "jq", "ls", "make", "mkdir", "nohup", "printf",
    "pwd", "read", "realpath", "rg", "rm", "rmdir", "rustc", "sed", "sleep", "sort", "stat",
    "strings", "tail", "touch", "tr", "true", "uniq", "wc", "which",
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

pub(crate) fn sets_tmpdir(command: &str) -> bool {
    if command.contains("+=") {
        return true;
    }
    for seg in split_compound(command) {
        for word in seg.split_whitespace() {
            if let Some((raw_name, _)) = word.split_once('=')
                && !raw_name.starts_with('-')
            {
                let name = decode_token(raw_name);
                if is_dangerous_env_name(&name) {
                    return true;
                }
            } else {
                break;
            }
        }
        let first = first_command_word(&seg).unwrap_or_default();
        if first == "read" {
            if read_assigns_dangerous_name(&seg) {
                return true;
            }
            continue;
        }
        if matches!(
            first.as_str(),
            "ln" | "pushd"
                | "popd"
                | "chroot"
                | "builtin"
                | "command"
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

/// Whether a read call assigns a name that rebinds the execution environment.
/// An option value and a flag are not names, and a prompt given to -p is text,
/// so only an operand that is a plain identifier can count.
fn read_assigns_dangerous_name(segment: &str) -> bool {
    let mut skipped_value = false;
    for token in segment.split_whitespace().skip(1) {
        if skipped_value {
            skipped_value = false;
            continue;
        }
        let word = decode_token(token);
        if word.starts_with('-') {
            if matches!(
                word.as_str(),
                "-p" | "-n" | "-d" | "-t" | "-u" | "-a" | "-i"
            ) {
                skipped_value = true;
            }
            continue;
        }
        if is_dangerous_env_name(&word) {
            return true;
        }
    }
    false
}

fn check_compound_depth(command: &str, depth: u8, fenced: bool) -> bool {
    if depth == 0 {
        return false;
    }
    // Quoting this scan cannot follow may hide a separator, which would make
    // one segment describe a program the shell runs as two.
    if !quotes_balanced(command) {
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

/// Whether a segment runs only commands whose effect a fenced session can
/// predict. A segment whose program is unknown is not proof of danger, but it
/// is not proof of safety either, so it cannot precede a temporary write; a
/// segment that names no program at all is judged by the bodies its
/// substitutions run, because those are the only commands it has.
pub(crate) fn starts_with_supported_command(segment: &str) -> bool {
    if let Some(program) = program_of(segment)
        && !SUPPORTED_EFFECT_COMMANDS.contains(&program.as_str())
    {
        return false;
    }
    match substitution_bodies(segment) {
        Some(bodies) => bodies
            .iter()
            .all(|body| starts_with_supported_command(body)),
        None => false,
    }
}

/// Whether the segment's own program is one the scan resolved, so a write it
/// opens is judged on that stage rather than borrowing confidence from an
/// earlier one. A stage whose argument list the unwrapper refused is not
/// resolved: a substitution in argument position can rewrite which program
/// the shell starts.
fn stage_resolved(segment: &str) -> bool {
    match stage_program(segment) {
        Stage::Program { exact, .. } => exact,
        Stage::Empty => starts_with_supported_command(segment),
    }
}

/// The program a segment runs, once wrappers, control-flow structure, and
/// leading assignments are stepped over. A segment of pure structure names no
/// program.
fn program_of(segment: &str) -> Option<String> {
    match stage_program(segment) {
        Stage::Program { command, .. } => Some(command.program().to_string()),
        Stage::Empty => None,
    }
}

/// Whether a redirect target names a single child of the session temporary
/// directory. A nested path, a traversal, a glob, or an expansion cannot be
/// resolved from text, so none of them qualify. The literal /tmp and
/// /private/tmp roots are admitted alongside the expansion forms because the
/// seatbelt fence allows writes there on every platform the gate runs on.
pub(crate) fn is_temp_output_target(target: &str) -> bool {
    if marker_hit(target) {
        return false;
    }
    let rest = target
        .strip_prefix("$TMPDIR/")
        .or_else(|| target.strip_prefix("${TMPDIR}/"))
        .or_else(|| target.strip_prefix("/tmp/"))
        .or_else(|| target.strip_prefix("/private/tmp/"));
    let Some(suffix) = rest else {
        return false;
    };
    is_single_temp_child(suffix)
}

/// Whether a suffix names one non-traversing child of a temporary directory.
fn is_single_temp_child(suffix: &str) -> bool {
    !suffix.is_empty()
        && suffix != "."
        && suffix != ".."
        && !suffix.contains(['/', '*', '?', '[', ']', '$', '`', '(', ')', '\\'])
}

/// Whether a redirect target names a safe relative path inside the workspace.
/// The path must not start from root or home, must not traverse, and must not
/// carry a glob, an expansion, or a marker the fence protects. With the fence
/// holding, the kernel decides where the write lands, so the gate only excludes
/// constructs that can change what the fence is asked to open.
fn is_safe_workspace_redirect_target(target: &str) -> bool {
    if marker_hit(target) {
        return false;
    }
    !target.is_empty()
        && !target.starts_with('/')
        && !target.starts_with('~')
        && !target.contains("..")
        && !target.contains(['*', '?', '[', ']', '$', '`', '(', ')', '\\'])
}

/// Whether a cp call writes its destination into a session-temporary child or a
/// safe workspace relative path. The destination is the last non-option
/// operand; earlier operands are sources.
fn copy_destination_is_safe(segment: &str) -> bool {
    let mut destination: Option<&str> = None;
    for token in segment.split_whitespace().skip(1) {
        if is_env_assignment(token) {
            continue;
        }
        let word = strip_quotes(token);
        if word.is_empty() || word.starts_with('-') {
            continue;
        }
        destination = Some(word);
    }
    destination.is_some_and(|d| is_temp_output_target(d) || is_safe_workspace_redirect_target(d))
}

fn segment_effect_safe(segment: &str, depth: u8, fenced: bool, preceding_supported: bool) -> bool {
    // A stage whose program name is a substitution or a variable runs text
    // the scan never reads as a program, so no stage-level check vouches for
    // it.
    if leading_command_substitution(segment) || program_is_unresolved(segment) {
        return false;
    }
    // A write borrows no confidence from its neighbours: the stage that opens
    // the file must itself be one the scan resolved, or the command is not
    // statically attestable whatever ran before it.
    let current_resolved = stage_resolved(segment);
    let cleaned = strip_safe_redirects(segment);
    if first_command_word(&cleaned).as_deref() == Some("eval") {
        return false;
    }
    // cp is admitted only when its destination names a session-temporary child
    // or a safe workspace relative path, so a copy into an arbitrary location
    // still asks.
    if first_command_word(&cleaned).as_deref() == Some("cp") && !copy_destination_is_safe(&cleaned)
    {
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
        // Inside double quotes a backslash escapes the next character, so a
        // closing quote or backtick it precedes is literal rather than a
        // boundary the scan can act on.
        if in_double && c == '\\' {
            i += 2;
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
            let Some(next_i) = scan_write_redirect_at(
                &chars,
                i,
                fenced && preceding_supported && current_resolved,
            ) else {
                return false;
            };
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
    let links = chain_links(&stripped);
    if links.is_empty() {
        return false;
    }
    let mut saw_rm = false;
    let mut preceding_supported = true;
    let mut cwd = TempCwd::Outside;
    for link in &links {
        cwd = cwd.advance(link);
        if is_rm_segment(&link.text) {
            saw_rm = true;
            if !preceding_supported || !rm_targets_are_scratch(&link.text, cwd) {
                return false;
            }
        }
        preceding_supported = preceding_supported && starts_with_supported_command(&link.text);
    }
    saw_rm
}

fn is_rm_segment(segment: &str) -> bool {
    first_command_word(segment).as_deref() == Some("rm")
}

fn rm_targets_are_scratch(segment: &str, cwd: TempCwd) -> bool {
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
        if !scratch_delete(word, cwd) {
            return false;
        }
    }
    saw_target
}

/// The index past a write redirect whose target the scan can vouch for, or
/// None when the stage may not write there or may not write at all.
fn scan_write_redirect_at(chars: &[char], i: usize, attestable: bool) -> Option<usize> {
    if !attestable {
        return None;
    }
    let after_op = if chars.get(i + 1) == Some(&'>') {
        i + 2
    } else {
        i + 1
    };
    let (target, next_i) = read_redirect_token(chars, after_op)?;
    (is_temp_output_target(&target) || is_safe_workspace_redirect_target(&target)).then_some(next_i)
}

fn scan_redirect_at(chars: &[char], i: usize) -> Option<usize> {
    let c = chars[i];
    if c == '<' && chars.get(i + 1) == Some(&'<') {
        // Only a quoted delimiter feeds literal text. A bare one, with or
        // without the tab-stripping dash, lets the shell expand the body, so
        // the stage is not one this check can vouch for.
        return match chars.get(i + 2).copied() {
            Some('\'' | '"' | '\\') => Some(i + 2),
            Some('-') if matches!(chars.get(i + 3), Some('\'' | '"' | '\\')) => Some(i + 3),
            _ => None,
        };
    }
    let (target, next_i) = read_redirect_token(chars, i + 1)?;
    is_safe_input_redirect(&target).then_some(next_i)
}

pub(crate) fn read_redirect_token(chars: &[char], start: usize) -> Option<(String, usize)> {
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

pub(crate) fn is_safe_input_redirect(target: &str) -> bool {
    if marker_hit(target) || names_credential_path(target) {
        return false;
    }
    if is_temp_output_target(target) {
        return true;
    }
    !target.is_empty()
        && !target.starts_with("/etc/")
        && !target.starts_with("/private/etc/")
        && !target.contains("..")
        && !target.contains('$')
        && !target.contains('`')
        && !target.contains('(')
}

fn starts_with_tmpdir_brace(chars: &[char], start: usize) -> bool {
    const TMPDIR_BRACE: [char; 9] = ['$', '{', 'T', 'M', 'P', 'D', 'I', 'R', '}'];
    chars.get(start..start + TMPDIR_BRACE.len()) == Some(&TMPDIR_BRACE)
}

fn first_command_word(segment: &str) -> Option<String> {
    program_of(segment)
}
