//! Read-only classification for a compound command: which command tokens only
//! read, and which argument forms make one of them write instead. Split from
//! the attestability scan because the two answer different questions — that one
//! asks whether the shell syntax hides a write, this one whether the command
//! itself is a reader — and because the per-command argument rules grow on
//! their own.

use super::split_compound;
use super::strip_safe_redirects;
use crate::pipeline::detection::strip_quotes;

/// Commands whose effect is reading or printing, never writing or executing.
/// A segment whose first command token is in this set and whose only shell
/// constructs are safe redirects (stripped) and command substitution (whose
/// contents are themselves read-only) is read-only, so a pipe chain of
/// read-only commands with command substitution auto-allows in Auto. Absent
/// are passthrough builtins (command, env, exec, xargs) that run a wrapped
/// command, and commands with dangerous positional forms (date sets the
/// clock, hostname sets the host, printenv leaks secrets); the conditional
/// ones need per-flag validation a follow-up ports. They still ask, which
/// is safe. The stream filters belong here because a counting or sorting tail
/// is what a survey command ends with, and asking for one buries the asks that
/// matter. Two of them can also write, so their arguments are checked
/// separately.
const READONLY_COMMANDS: &[&str] = &[
    "cat", "head", "tail", "grep", "egrep", "fgrep", "rg", "strings", "ls", "wc", "file", "which",
    "whereis", "basename", "dirname", "realpath", "stat", "du", "df", "pwd", "uname", "whoami",
    "id", "groups", "echo", "printf", "seq", "test", "true", "false", "sort", "uniq", "cut", "tr",
];

/// Whether a read-only command was asked to write instead of print.
///
/// The redirect scan cannot see these forms: the destination is an argument,
/// not an operator. sort names its output file with a flag and can also point
/// its temporary files at a directory or hand them to a program, both of which
/// write. uniq names its output file with a second positional operand. cut and
/// tr print only, so they need no check.
fn writes_output(cmd: &str, segment: &str) -> bool {
    let args: Vec<&str> = segment.split_whitespace().skip(1).collect();
    match cmd {
        "sort" => args.iter().any(|arg| sort_writes(arg)),
        // uniq [OPTION]... [INPUT [OUTPUT]]: the second operand is a file.
        "uniq" => args.iter().filter(|arg| is_operand(arg)).count() > 1,
        _ => false,
    }
}

/// Whether one argument of uniq names a file rather than an option. A lone
/// dash stands for standard input, so it is an operand too.
fn is_operand(arg: &str) -> bool {
    arg == "-" || !arg.starts_with('-')
}

/// Whether one argument of sort asks it to write. The long forms are matched
/// by prefix because the option set is unambiguous from the first letter, and
/// a short cluster is matched by the letter it carries.
fn sort_writes(arg: &str) -> bool {
    if let Some(long) = arg.strip_prefix("--") {
        let name = long.split('=').next().unwrap_or("");
        return ["output", "temporary-directory", "compress-program"]
            .iter()
            .any(|option| option.starts_with(name));
    }
    match arg.strip_prefix('-') {
        Some(cluster) => cluster.contains('o') || cluster.contains('T'),
        None => false,
    }
}

/// Whether a compound command is entirely read-only: every segment's first
/// command token is in the read-only set, no segment writes a file (safe
/// redirects are stripped first, and an argument-named destination is
/// rejected), and every
/// command substitution nests read-only commands (depth-limited,
/// fail-closed). The destructive validator's word scan catches destructive
/// verbs anywhere in the content, and the egress validator catches network
/// tools at the top level, so a read-only shell around a destructive
/// substitution still asks via the inner scan.
pub fn is_readonly_compound(segments: &[&str]) -> bool {
    is_readonly_compound_depth(segments, 3)
}

fn is_readonly_compound_depth(segments: &[&str], depth: u8) -> bool {
    if segments.is_empty() || depth == 0 {
        return false;
    }
    segments.iter().all(|s| is_readonly_segment(s, depth))
}

/// Whether one segment is read-only at the given recursion depth. Strips
/// safe redirects, rejects unquoted file redirects and process substitution,
/// extracts command-substitution and backtick bodies for recursive read-only
/// checks, and requires the first non-assignment token to be a read-only
/// command.
fn is_readonly_segment(seg: &str, depth: u8) -> bool {
    if depth == 0 {
        return false;
    }
    let stripped = strip_safe_redirects(seg);
    let chars: Vec<char> = stripped.chars().collect();
    let n = chars.len();
    let mut i = 0;
    let mut in_single = false;
    let mut in_double = false;
    let mut substs: Vec<String> = Vec::new();
    while i < n {
        let c = chars[i];
        match c {
            '\'' if !in_double => {
                in_single = !in_single;
                i += 1;
                continue;
            }
            '"' if !in_single => {
                in_double = !in_double;
                i += 1;
                continue;
            }
            _ => {}
        }
        if in_single || in_double {
            i += 1;
            continue;
        }
        // Unquoted file redirect or process substitution writes or talks;
        // not read-only. Safe redirects were already stripped.
        if c == '>' || c == '<' {
            return false;
        }
        // Command substitution: dollar plus open paren. Extract the body for
        // a recursive read-only check.
        if c == '$' && i + 1 < n && chars[i + 1] == '(' {
            let Some(end) = find_matching_paren(&chars, i + 1) else {
                return false;
            };
            substs.push(chars[i + 2..end].iter().collect());
            i = end + 1;
            continue;
        }
        // Backtick substitution. Extract the body for a recursive check.
        if c == '`' {
            let Some(rel) = chars[i + 1..].iter().position(|&ch| ch == '`') else {
                return false;
            };
            let end = i + 1 + rel;
            substs.push(chars[i + 1..end].iter().collect());
            i = end + 1;
            continue;
        }
        i += 1;
    }
    // First non-assignment command token must be a read-only command. Skip
    // leading env assignments (FOO=bar) the way the egress scan does.
    let first = stripped.split_whitespace().find(|t| !t.contains('='));
    let Some(cmd) = first else { return false };
    let cmd = strip_quotes(cmd);
    if !READONLY_COMMANDS.contains(&cmd) {
        return false;
    }
    if writes_output(cmd, &stripped) {
        return false;
    }
    // Every substitution body is itself a read-only compound (split on pipes,
    // and-or, and semicolon so each stage is checked, depth-limited).
    for s in &substs {
        let segs = split_compound(s);
        let refs: Vec<&str> = segs.iter().map(|x| x.as_str()).collect();
        if !is_readonly_compound_depth(&refs, depth - 1) {
            return false;
        }
    }
    true
}

/// Find the index of the close paren matching the open paren at the given
/// position, quote-aware so a paren inside quotes is not counted. Returns
/// None when unbalanced (the caller fails closed).
fn find_matching_paren(chars: &[char], open: usize) -> Option<usize> {
    let mut depth = 0;
    let mut in_single = false;
    let mut in_double = false;
    let mut i = open;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            _ => {
                if !in_single && !in_double {
                    if c == '(' {
                        depth += 1;
                    } else if c == ')' {
                        depth -= 1;
                        if depth == 0 {
                            return Some(i);
                        }
                    }
                }
            }
        }
        i += 1;
    }
    None
}
