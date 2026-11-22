//! Shell command classifiers used to separate Level 1 auto-approvable
//! operations from Level 2 manual confirmations on approval cards.

use std::mem::take;

use houyicoder_api::shell::interpreter::without_read_only_code;

use super::scratch_local::{
    acts_on_named_files, advance_scratch, names_only_scratch_paths, split_steps, unpacks_in_place,
};

/// Whether the network access in a command only reads.
///
/// A command is a list of segments, and only some of them reach the network.
/// A leading change of directory, a formatter at the end of a pipe, or a
/// redirect that moves a stream says nothing about the destination, so each is
/// skipped rather than read as an unrecognized command. What remains must be a
/// recognized read, and the command must contain at least one: a send, an
/// interactive transport, or a segment this walk does not know is held.
pub(super) fn reads_network(command: &str) -> bool {
    let mut saw_network_read = false;
    // An interpreter's inline code carries the separators the split below cuts
    // on, so it is read and set aside first. A body that is not a verified
    // read-only program stops the walk: the countdown resolves unattended, and
    // an opaque program beside a request does not get one.
    let Some(without_code) = without_read_only_code(command) else {
        return false;
    };
    let stripped = without_stream_redirects(&without_code);
    // The command keeps its case: a short flag is case-sensitive, and folding
    // it would read an upload flag as the fail flag beside it.
    let mut in_scratch = false;
    for step in split_steps(&stripped) {
        let words: Vec<&str> = step
            .text
            .split_whitespace()
            .map(|w| w.trim_matches(|c: char| c == '"' || c == '\''))
            .filter(|w| !w.is_empty() && !is_assignment(w))
            .collect();
        // A command substitution body arrives as its own split segment
        // below; a leading control keyword or case label is structure, not
        // a program, so it steps aside and the program word decides.
        let words: Vec<&str> = skip_control_shell(words);
        let words: Vec<&str> = skip_timeout(words);
        in_scratch = advance_scratch(in_scratch, step.chained, &words);
        let Some(&first) = words.first() else {
            continue;
        };
        let args = &words[1..];
        match program_name(first).to_ascii_lowercase().as_str() {
            "git" => match git_subcommand(args) {
                "push" => {
                    if !git_push_dry_run(args) {
                        return false;
                    }
                    saw_network_read = true;
                }
                "pull" | "fetch" | "clone" | "ls-remote" => saw_network_read = true,
                // A local repository verb says nothing about a destination.
                _ => {}
            },
            "curl" | "wget" => {
                if words.iter().any(|w| sends_body(w)) || names_write_method(&words) {
                    return false;
                }
                saw_network_read = true;
            }
            "gh" => {
                let group = words.get(1).copied().unwrap_or("");
                let verb = words.get(2).copied().unwrap_or("");
                let reads = matches!(group, "version" | "help" | "status" | "search")
                    || (group == "auth" && verb == "status")
                    || (matches!(
                        group,
                        "pr" | "issue"
                            | "repo"
                            | "release"
                            | "run"
                            | "workflow"
                            | "label"
                            | "cache"
                    ) && matches!(verb, "view" | "list" | "diff" | "checks" | "status"));
                if !reads {
                    return false;
                }
                saw_network_read = true;
            }
            // An interactive transport carries whatever the far side asks
            // for. Local rsync stays local unless it names a remote target or
            // transport flag.
            "scp" | "sftp" | "ssh" | "nc" | "netcat" | "telnet" => return false,
            "rsync" => {
                if rsync_uses_remote(args) || !names_only_scratch_paths(&words, in_scratch) {
                    return false;
                }
            }
            "npm" | "cargo" if args.first() == Some(&"publish") => return false,
            "pip" if args.first() == Some(&"upload") => return false,
            local if is_local_consumer(local) => {}
            // A local file program beside the request changes only files it
            // names on its own command line. A name that reaches outside the
            // session scratch space, or a remote path, is not that, so it is
            // held.
            local if acts_on_named_files(local) && names_only_scratch_paths(&words, in_scratch) => {
            }
            // An extraction that unpacks a relative archive in place also
            // stays beside the request. Its own check already requires every
            // path in the step to stay in the scratch space.
            _ if unpacks_in_place(&words, in_scratch) => {}
            // Not a form this walk knows. A countdown that publishes work is
            // the failure this exists to prevent, so it is held.
            _ => return false,
        }
    }
    saw_network_read
}

/// Whether a command invokes git push with force or verification-skip flags,
/// without being fooled by unrelated flags like rm -f or message strings.
pub(super) fn command_has_forced_push(command: &str, depth: u8) -> bool {
    if depth == 0 {
        return false;
    }
    let cleaned = without_stream_redirects(command);
    for step in split_steps(&cleaned) {
        let owned = split_shell_words(step.text);
        let words: Vec<&str> = owned
            .iter()
            .map(String::as_str)
            .filter(|w| !w.is_empty() && !is_assignment(w))
            .collect();
        let words = skip_control_shell(words);
        let words = skip_timeout(words);
        let Some(&first) = words.first() else {
            continue;
        };
        let prog = program_name(first);
        let args = &words[1..];
        if prog.eq_ignore_ascii_case("git") && git_subcommand(args).eq_ignore_ascii_case("push") {
            let sub_args = git_subcommand_args(args);
            if push_args_force(sub_args) {
                return true;
            }
        }
        if matches!(
            prog.to_ascii_lowercase().as_str(),
            "bash" | "sh" | "dash" | "zsh"
        ) && let Some(pos) = args.iter().position(|a| *a == "-c" || *a == "-lc")
            && let Some(inner) = args.get(pos + 1)
            && command_has_forced_push(inner, depth - 1)
        {
            return true;
        }
    }
    false
}

/// Remove the redirects that only move a stream.
///
/// The split below cuts on a bare ampersand, which would turn 2>&1 into a
/// segment of its own and read it as a command. Removing the stream forms
/// first keeps the split on the command separators it is meant for.
fn without_stream_redirects(command: &str) -> String {
    let mut out = command.to_string();
    for form in [
        "2>&1",
        "1>&2",
        "&>/dev/null",
        "2>/dev/null",
        ">/dev/null",
        "2>&-",
    ] {
        out = out.replace(form, " ");
    }
    out
}

/// Whether a request names a method that writes.
///
/// A request body is not the only way to write on the far side: a method on
/// its own says so. The read methods are the ones that leave the far side
/// unchanged.
fn names_write_method(words: &[&str]) -> bool {
    let mut index = 0;
    while index < words.len() {
        let word = words[index];
        index += 1;
        let value = word
            .strip_prefix("--request=")
            .map(str::to_string)
            .or_else(|| {
                let rest = word
                    .strip_prefix("--request")
                    .or_else(|| word.strip_prefix("-X"))?;
                if !rest.is_empty() {
                    return Some(rest.trim_start_matches('=').to_string());
                }
                let next = words.get(index).copied().unwrap_or("");
                index += 1;
                Some(next.to_string())
            });
        if let Some(value) = value {
            let method = value.trim().to_ascii_uppercase();
            if !method.is_empty() && !matches!(method.as_str(), "GET" | "HEAD" | "OPTIONS") {
                return true;
            }
        }
    }
    false
}

/// Whether a word is a leading NAME=value assignment rather than a command.
fn is_assignment(word: &str) -> bool {
    let Some((name, _)) = word.split_once('=') else {
        return false;
    };
    if matches!(
        name,
        "GIT_SSH_COMMAND"
            | "GIT_SSH"
            | "GIT_EXEC_PATH"
            | "GIT_ASKPASS"
            | "SSH_ASKPASS"
            | "CURL_HOME"
            | "HTTP_PROXY"
            | "HTTPS_PROXY"
            | "ALL_PROXY"
            | "http_proxy"
            | "https_proxy"
            | "all_proxy"
    ) || name.starts_with("GIT_CONFIG")
        || name.starts_with("LD_")
        || name.starts_with("DYLD_")
    {
        return false;
    }
    !name.is_empty()
        && name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Step over the shell structure words that lead a split segment: loop and
/// conditional keywords, case labels, and the capture half of an
/// assignment substitution.
fn skip_control_shell(words: Vec<&str>) -> Vec<&str> {
    let mut start = 0;
    while start < words.len() {
        let word = words[start];
        let lowered = word.to_ascii_lowercase();
        let bare = lowered.trim_end_matches(';');
        match bare {
            "do" | "done" | "then" | "else" | "elif" | "fi" | "esac" | "break" | "continue" => {
                start += 1;
            }
            "for" | "while" | "until" | "case" | "if" => {
                start += 1;
                let opener = if bare == "case" { "in" } else { "do" };
                while start < words.len() && words[start] != opener {
                    start += 1;
                }
                start = (start + 1).min(words.len());
            }
            _ if is_assignment(word) && word.contains('$') => {
                start += 1;
            }
            _ if (word.ends_with(')') && !word.starts_with('('))
                || (bare.starts_with('*') && bare.ends_with('*')) =>
            {
                start += 1;
            }
            _ => break,
        }
    }
    words[start..].to_vec()
}

/// Step over a leading timeout wrapper: its duration and options are not the
/// program the walk decides about, while the command it runs is.
fn skip_timeout(words: Vec<&str>) -> Vec<&str> {
    let mut start = 0;
    while words
        .get(start)
        .is_some_and(|word| word.eq_ignore_ascii_case("timeout"))
    {
        start += 1;
        while words.get(start).is_some_and(|word| word.starts_with('-')) {
            let takes_value = matches!(words[start], "-s" | "-k" | "--signal" | "--kill-after");
            start += if takes_value { 2 } else { 1 };
        }
        if words.get(start).is_some_and(|word| !word.starts_with('-')) {
            start += 1;
        }
    }
    words[start.min(words.len())..].to_vec()
}

/// Whether a segment is a local command that only reads.
fn is_local_consumer(word: &str) -> bool {
    matches!(
        word,
        "cd" | "echo"
            | "printf"
            | "head"
            | "tail"
            | "cat"
            | "jq"
            | "grep"
            | "egrep"
            | "fgrep"
            | "rg"
            | "sed"
            | "awk"
            | "wc"
            | "sort"
            | "uniq"
            | "cut"
            | "tr"
            | "true"
            | "false"
            | "test"
            | "ls"
            | "pwd"
            | "basename"
            | "dirname"
            | "realpath"
            | "date"
            | "stat"
            | "file"
            | "strings"
            | "shasum"
            | "sha256sum"
            | "md5"
            | "diff"
            | "which"
            | "whereis"
            | "whoami"
            | "id"
            | "uname"
            | "df"
            | "du"
            | "groups"
            | "seq"
            | "sleep"
    )
}

/// Bare executable name stripped of directory prefix and .exe suffix.
fn program_name(word: &str) -> &str {
    let file = word.rsplit(['/', '\\']).next().unwrap_or(word);
    file.strip_suffix(".exe")
        .or_else(|| file.strip_suffix(".EXE"))
        .unwrap_or(file)
}

/// The subcommand a git invocation runs, past the flags that precede it.
fn git_subcommand<'a>(args: &[&'a str]) -> &'a str {
    let mut index = 0;
    while index < args.len() {
        let arg = args[index];
        if arg.starts_with('-') {
            index += if matches!(
                arg,
                "-C" | "-c" | "--exec-path" | "--git-dir" | "--namespace" | "--work-tree"
            ) {
                2
            } else {
                1
            };
            continue;
        }
        return arg;
    }
    ""
}

fn git_push_dry_run(args: &[&str]) -> bool {
    let mut saw_dry_run = false;
    for &arg in args {
        if arg == "--" {
            break;
        }
        if matches!(arg, "--dry-run" | "-n") {
            saw_dry_run = true;
        }
        if matches!(
            arg,
            "-f" | "--force" | "--force-with-lease" | "--force-if-includes" | "--no-verify"
        ) || arg.starts_with("--force-with-lease=")
        {
            return false;
        }
    }
    saw_dry_run
}

fn git_subcommand_args<'a, 'b>(args: &'a [&'b str]) -> &'a [&'b str] {
    let mut index = 0;
    while index < args.len() {
        let arg = args[index];
        if arg.starts_with('-') {
            index += if matches!(
                arg,
                "-C" | "-c" | "--exec-path" | "--git-dir" | "--namespace" | "--work-tree"
            ) {
                2
            } else {
                1
            };
            continue;
        }
        return &args[index + 1..];
    }
    &[]
}

fn push_args_force(args: &[&str]) -> bool {
    for &arg in args {
        if arg == "--" {
            break;
        }
        if matches!(
            arg,
            "--force" | "--force-with-lease" | "--force-if-includes" | "--no-verify"
        ) || arg.starts_with("--force-with-lease=")
            || (arg.starts_with('-') && !arg.starts_with("--") && arg.contains('f'))
        {
            return true;
        }
    }
    false
}

fn rsync_uses_remote(args: &[&str]) -> bool {
    args.iter().any(|&arg| {
        matches!(
            arg,
            "-e" | "--rsh"
                | "--rsync-path"
                | "--daemon"
                | "--port"
                | "--address"
                | "--sockopts"
                | "--connect-program"
                | "--password-file"
        ) || arg.starts_with("--rsh=")
            || arg.starts_with("--rsync-path=")
            || arg.starts_with("--port=")
            || arg.starts_with("--address=")
            || arg.starts_with("--sockopts=")
            || arg.starts_with("--connect-program=")
            || arg.starts_with("--password-file=")
            || (!arg.starts_with("--") && arg.starts_with('-') && arg.contains('e'))
            || arg.contains(':')
    })
}

fn split_shell_words(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut in_single = false;
    let mut in_double = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' if !in_double => {
                in_single = !in_single;
                in_word = true;
            }
            '"' if !in_single => {
                in_double = !in_double;
                in_word = true;
            }
            '\\' if !in_single => {
                if let Some(next) = chars.next() {
                    cur.push(next);
                    in_word = true;
                }
            }
            c if c.is_ascii_whitespace() && !in_single && !in_double => {
                if in_word {
                    out.push(take(&mut cur));
                    in_word = false;
                }
            }
            other => {
                cur.push(other);
                in_word = true;
            }
        }
    }
    if in_word {
        out.push(cur);
    }
    out
}

/// Whether an argument turns a curl or wget request into an upload or a write
/// on the far side.
fn sends_body(word: &str) -> bool {
    if let Some(long) = word.strip_prefix("--") {
        let name = long.split('=').next().unwrap_or(long);
        return matches!(
            name,
            "data"
                | "data-raw"
                | "data-binary"
                | "data-urlencode"
                | "form"
                | "upload-file"
                | "json"
                | "post-data"
                | "post-file"
                | "body-data"
        );
    }
    let Some(short) = word.strip_prefix('-') else {
        return false;
    };
    !short.starts_with('-') && short.chars().any(|c| matches!(c, 'd' | 'F' | 'T'))
}
