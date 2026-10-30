//! Read-only classification for a compound command: which command tokens only
//! read, and which argument forms make one of them write instead. Split from
//! the attestability scan because the two answer different questions — that one
//! asks whether the shell syntax hides a write, this one whether the command
//! itself is a reader — and because the per-command argument rules grow on
//! their own.

use super::quotes_balanced;
use super::shell_word_spans;
use super::split_compound;
use super::strip_safe_redirects;
use crate::pipeline::detection::strip_quotes;

/// Commands whose effect is reading or printing, provided their arguments
/// carry no writing form. A segment whose first command token is in this set,
/// whose arguments pass the rule below, and whose only shell constructs are
/// safe redirects (stripped) and command substitution (whose contents are
/// themselves read-only) is read-only, so a pipe chain of read-only commands
/// auto-allows.
///
/// Absent are passthrough builtins (command, env, exec, xargs) that run a
/// wrapped command, commands with dangerous positional forms (date sets the
/// clock, hostname sets the host, printenv leaks secrets), and anything whose
/// read form can write or run a program. They still ask, which is safe.
///
/// The set mixes commands that read files, commands that read the filesystem,
/// one repository client, and one network client. Every one of them is
/// admitted by the argument rule, never by the name alone: the name says what
/// the command usually does, the rule says what this call does.
const READONLY_COMMANDS: &[&str] = &[
    "basename",
    "cat",
    "curl",
    "cut",
    "df",
    "diff",
    "dirname",
    "du",
    "echo",
    "egrep",
    "false",
    "fgrep",
    "file",
    "find",
    "git",
    "grep",
    "groups",
    "head",
    "id",
    "jq",
    "ls",
    "md5",
    "printf",
    "pwd",
    "realpath",
    "rg",
    "sed",
    "seq",
    "sha256sum",
    "shasum",
    "sort",
    "stat",
    "strings",
    "tail",
    "test",
    "tr",
    "true",
    "uname",
    "uniq",
    "wc",
    "whereis",
    "which",
    "whoami",
];

/// Repository subcommands that only inspect a repository. The hash form is
/// included because without its write flag it prints the hash instead of
/// storing the object; the writing form is rejected by the argument rule. The
/// configuration reader is absent because its default form writes.
const GIT_READ_SUBCOMMANDS: &[&str] = &[
    "status",
    "diff",
    "show",
    "log",
    "ls-files",
    "rev-parse",
    "rev-list",
    "hash-object",
];

/// Repository options that write a file, run a program the user did not
/// choose, or inject configuration. Matched by prefix so an abbreviation that
/// could reach one of them is treated as the writer it may be.
const GIT_WRITE_OPTIONS: &[&str] = &[
    "-o",
    "--output",
    "--ext-diff",
    "--textconv",
    "-c",
    "--exec-path",
];

/// Predicates that delete a file, run a program, or write output into one.
const FIND_WRITE_PREDICATES: &[&str] = &[
    "-delete", "-exec", "-execdir", "-ok", "-okdir", "-fprint", "-fprint0", "-fprintf", "-fls",
];

/// Long options of the network client that write a local file, upload one,
/// send a request body, or load configuration and command text.
const CURL_WRITE_LONG: &[&str] = &[
    "output",
    "remote",
    "upload-file",
    "config",
    "data",
    "form",
    "json",
    "url-query",
];

/// The short letters behind the long options above, plus the request method,
/// which a short option cluster can carry.
const CURL_WRITE_SHORT: &[char] = &['o', 'O', 'T', 'K', 'd', 'F', 'J', 'X'];

/// Whether a command with these arguments only reads. The argument rules are
/// why a name alone never decides: the same program that prints a count also
/// writes a file when one flag is added.
///
/// The network client is admitted only at the top level of a pipeline. Inside
/// a command substitution its request would run while the outer command is
/// still being judged, so the substitution stays a place where a network call
/// asks.
pub(super) fn command_reads_only(
    cmd: &str,
    args: &[&str],
    segment: &str,
    allow_network: bool,
) -> bool {
    if !READONLY_COMMANDS.contains(&cmd) {
        return false;
    }
    match cmd {
        "sort" | "uniq" | "printf" => !writes_output(cmd, segment),
        "find" => !args.iter().any(|arg| FIND_WRITE_PREDICATES.contains(arg)),
        "git" => git_reads_only(args),
        "rg" => !args.iter().any(|arg| arg.starts_with("--pre")),
        "sed" => sed_reads_only(args),
        "curl" => allow_network && curl_reads_only(segment, args),
        _ => true,
    }
}

/// Whether a sed call only reads. The in-place flags and the script-file flag
/// write, and a write command in the script writes, so each of them asks. The
/// rest print, so a sed that pages a file with -n and a print range reads.
fn sed_reads_only(args: &[&str]) -> bool {
    for &arg in args {
        if arg == "-i" || arg == "--in-place" || arg == "-f" || arg == "--file" {
            return false;
        }
        // A short cluster like -in or -i.bak carries the in-place letter.
        if let Some(cluster) = arg.strip_prefix('-')
            && !cluster.starts_with('-')
            && cluster.contains('i')
        {
            return false;
        }
        if arg.contains("/w ") || arg.contains(" w ") || arg.trim_end().ends_with(" w") {
            return false;
        }
        // A sed write command like 'w file' or 's/a/b/w file'
        if arg.starts_with("w ") || arg.starts_with("w/") {
            return false;
        }
    }
    true
}

fn git_reads_only(args: &[&str]) -> bool {
    let Some(sub) = args.first() else {
        return false;
    };
    if !GIT_READ_SUBCOMMANDS.contains(sub) {
        return false;
    }
    let rest = &args[1..];
    if rest.iter().any(|arg| git_writes(arg)) {
        return false;
    }
    // Only the hash form writes the object database, and only when asked to.
    if *sub == "hash-object" {
        return !rest.iter().any(|arg| short_cluster_has(arg, 'w'));
    }
    true
}

/// Whether one repository argument asks for a write, an outside program, or
/// injected configuration. A long option is matched with and without its
/// attached value. A short one is matched by its leading letter, because an
/// attached value leaves no separator to check.
fn git_writes(arg: &str) -> bool {
    if let Some(long) = arg.strip_prefix("--") {
        return GIT_WRITE_OPTIONS.iter().any(|option| {
            option
                .strip_prefix("--")
                .is_some_and(|name| long == name || long.starts_with(&format!("{name}=")))
        });
    }
    let Some(short) = arg.strip_prefix('-') else {
        return false;
    };
    GIT_WRITE_OPTIONS
        .iter()
        .filter_map(|option| option.strip_prefix('-'))
        .any(|letter| short.starts_with(letter))
}

/// Whether the network client would write, upload, or send a body. A request
/// reads only as a plain fetch whose every operand is an http or https URL and
/// whose text carries no expansion: a substitution inside the URL is what
/// turns a read into an upload of whatever the substitution printed.
fn curl_reads_only(segment: &str, args: &[&str]) -> bool {
    if segment.contains('$') || segment.contains('`') {
        return false;
    }
    let mut saw_url = false;
    let mut i = 0;
    while i < args.len() {
        let arg = args[i];
        i += 1;
        if let Some(long) = arg.strip_prefix("--") {
            let (name, value) = match long.split_once('=') {
                Some((name, value)) => (name, Some(value)),
                None => (long, None),
            };
            // The named-target option carries its own operand; either spelling
            // still has to name an http address.
            if name == "url" {
                let target = match value {
                    Some(value) => value,
                    None => match args.get(i) {
                        Some(next) => {
                            i += 1;
                            next
                        }
                        None => return false,
                    },
                };
                if !is_http_url(target) {
                    return false;
                }
                saw_url = true;
                continue;
            }
            if name.is_empty() {
                continue;
            }
            if CURL_WRITE_LONG
                .iter()
                .any(|option| option.starts_with(name))
            {
                return false;
            }
            continue;
        }
        if let Some(cluster) = arg.strip_prefix('-') {
            if cluster.chars().any(|c| CURL_WRITE_SHORT.contains(&c)) {
                return false;
            }
            continue;
        }
        if !is_http_url(arg) {
            return false;
        }
        saw_url = true;
    }
    saw_url
}

fn is_http_url(arg: &str) -> bool {
    arg.starts_with("http://") || arg.starts_with("https://")
}

/// Whether a short option cluster carries the given letter.
fn short_cluster_has(arg: &str, letter: char) -> bool {
    arg.strip_prefix('-')
        .is_some_and(|cluster| !cluster.starts_with('-') && cluster.contains(letter))
}

/// Whether a read-only command was asked to write instead of print.
///
/// The redirect scan cannot see these forms: the destination is an argument,
/// not an operator. sort names its output file with a flag and can also point
/// its temporary files at a directory or hand them to a program, both of which
/// write. uniq names its output file with a second positional operand. cut and
/// tr print only, so they need no check.
fn writes_output(cmd: &str, segment: &str) -> bool {
    if !matches!(cmd, "sort" | "uniq" | "printf") {
        return false;
    }
    let raw_args: Vec<&str> = shell_word_spans(segment).into_iter().skip(1).collect();
    // Fail closed when an argument carries a backslash escape or shell
    // expansion: a static check cannot prove that the expanded token is not
    // an output flag or a second path argument.
    if raw_args
        .iter()
        .any(|raw| raw.contains('\\') || raw.contains('$') || raw.contains('`'))
    {
        return true;
    }
    let args: Vec<String> = raw_args
        .into_iter()
        .map(|raw| raw.chars().filter(|&c| c != '"' && c != '\'').collect())
        .collect();
    match cmd {
        "sort" => args.iter().any(|arg| sort_writes(arg)),
        // uniq [OPTION]... [INPUT [OUTPUT]]: the second path argument is a file.
        "uniq" => args.iter().filter(|arg| is_path_arg(arg)).count() > 1,
        "printf" => args.iter().any(|arg| arg == "-v"),
        _ => false,
    }
}

/// Whether one argument of uniq names a path rather than an option. A lone
/// dash stands for standard input, so it is a path argument too.
fn is_path_arg(arg: &str) -> bool {
    arg == "-" || !arg.starts_with('-')
}

/// Whether one argument of sort asks it to write. The long forms are matched
/// by prefix because the option set is unambiguous from the first letter, and
/// a short cluster is matched by the letter it carries.
fn sort_writes(arg: &str) -> bool {
    if let Some(long) = arg.strip_prefix("--") {
        let name = long.split('=').next().unwrap_or("");
        if name.is_empty() {
            return false;
        }
        return ["output", "temporary-directory", "compress-program"]
            .iter()
            .any(|option| option.starts_with(name));
    }
    match arg.strip_prefix('-') {
        Some(cluster) => cluster.contains('o') || cluster.contains('T'),
        None => false,
    }
}

/// How many nested command substitutions the check follows before it refuses.
/// The same number is the top level: a body reached at a lower count is
/// nested, and the rules that only hold at the top are dropped there.
const READONLY_DEPTH: u8 = 3;

/// Whether a compound command is entirely read-only: every segment passes the
/// read-only rule, no segment writes a file (safe redirects are stripped
/// first, and an argument-named destination is rejected), and every command
/// substitution nests read-only commands (depth-limited, fail-closed). A
/// leading move into one literal directory is transparent, because reading a
/// directory the user names is not a write. The destructive validator's word
/// scan catches destructive verbs anywhere in the content, and the egress
/// validator catches network tools at the top level, so a read-only shell
/// around a destructive substitution still asks via the inner scan.
pub fn is_readonly_compound(segments: &[&str]) -> bool {
    is_readonly_compound_depth(segments, READONLY_DEPTH)
}

fn is_readonly_compound_depth(segments: &[&str], depth: u8) -> bool {
    if segments.is_empty() || depth == 0 {
        return false;
    }
    // A leading move into one literal directory is transparent: it changes
    // where the following read commands look, and it has no effect of its own
    // beyond this shell. Only the first segment can be one, because its whole
    // purpose is to place the reads that come after it.
    let head = usize::from(segments.len() > 1 && is_literal_cd(segments[0]));
    segments[head..]
        .iter()
        .all(|s| is_readonly_segment(s, depth))
}

/// Whether a segment is exactly a move into one directory named literally.
/// An expansion or redirect would make the destination something the check
/// cannot see, so anything but two plain tokens is refused.
fn is_literal_cd(segment: &str) -> bool {
    let tokens: Vec<&str> = segment.split_whitespace().collect();
    match tokens.as_slice() {
        ["cd", dir] => !dir.contains(['$', '`', '>', '<']),
        _ => false,
    }
}

/// Whether one segment is read-only at the given recursion depth. Strips
/// safe redirects, rejects unquoted file redirects, process substitution, and
/// a leading environment assignment, extracts command-substitution and
/// backtick bodies for recursive read-only checks, and requires the command
/// token plus its arguments to pass the read-only rule.
fn is_readonly_segment(seg: &str, depth: u8) -> bool {
    if depth == 0 {
        return false;
    }
    // Quoting this scan cannot follow may hide a separator, which would make
    // one segment two commands the shell runs.
    if !quotes_balanced(seg) {
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
        if in_single {
            i += 1;
            continue;
        }
        // Command substitution and backticks execute inside double quotes too.
        if c == '$' && i + 1 < n && chars[i + 1] == '(' {
            let Some(end) = find_matching_paren(&chars, i + 1) else {
                return false;
            };
            substs.push(chars[i + 2..end].iter().collect());
            i = end + 1;
            continue;
        }
        if c == '`' {
            let Some(rel) = chars[i + 1..].iter().position(|&ch| ch == '`') else {
                return false;
            };
            let end = i + 1 + rel;
            substs.push(chars[i + 1..end].iter().collect());
            i = end + 1;
            continue;
        }
        if in_double {
            i += 1;
            continue;
        }
        // Unquoted file redirect or process substitution writes or talks;
        // not read-only. Safe redirects were already stripped.
        if c == '>' || c == '<' {
            return false;
        }
        i += 1;
    }
    // A leading environment assignment is refused outright rather than
    // skipped. A variable set for one command can redirect the repository
    // directory, the shell startup file, or the loader search path, so the
    // same argv would no longer describe what runs; asking is the safe answer.
    let tokens: Vec<&str> = shell_word_spans(&stripped);
    let Some(first) = tokens.first() else {
        return false;
    };
    if first.contains('=') {
        return false;
    }
    let cmd = strip_quotes(first);
    let args: Vec<&str> = tokens[1..].iter().map(|t| strip_quotes(t)).collect();
    if !command_reads_only(cmd, &args, &stripped, depth >= READONLY_DEPTH) {
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
pub(super) fn find_matching_paren(chars: &[char], open: usize) -> Option<usize> {
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
