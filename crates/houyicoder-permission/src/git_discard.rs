//! The git-discard consent classifier (CONSERVATIVE). This is the
//! asymmetric half of the two-gate design:
//!
//! - consent (this crate, git_discard_consent_word): CONSERVATIVE — only the
//!   clear-cut discard forms Ask. A false positive asks the user once
//!   (annoying but safe); a false negative runs silently (the snapshot
//!   trigger in core is the backstop). Stays conservative to avoid
//!   over-asking on branch switches.
//! - snapshot (core, agent::git_discard::git_snapshot_form): AGGRESSIVE — any
//!   git op that MAY touch the working tree triggers a pre-run CoW snapshot.
//!   A false positive is one free snapshot (silent); a false negative is
//!   permanent data loss. The static matcher cannot tell a branch name from a
//!   file path, so the snapshot side treats any single-path checkout as a
//!   potential discard.
//!
//! The asymmetry is by design — the two gates have opposite cost profiles.
//! Layering forbids the permission crate from importing core's matcher
//! (permission depends on core, not vice versa), so this crate holds its own
//! conservative copy. The two stay in sync on the clear-cut forms via a
//! shared DRIFT_VECTOR duplicated in both crates' tests.

use crate::compound::strip_safe_redirects;
use crate::pipeline::detection::expand_inline_interpreter;

/// The git subcommand word for a discard form, for session-scoped consent.
/// Returns None for a non-discard (a branch switch, an unstage, a stash save,
/// a normal push). args are lowercased by the caller (git flags -d/-D
/// collapse to the same intent — both delete a branch — so case-folding is
/// safe).
pub(crate) fn git_discard_consent_word(sub: &str, args: &[&str]) -> Option<&'static str> {
    let sub = sub.to_ascii_lowercase();
    let has = |flag: &str| args.contains(&flag);
    let non_flag_count = || {
        args.iter()
            .filter(|a| !a.starts_with('-') && **a != "--")
            .count()
    };
    let word = match sub.as_str() {
        "checkout" | "switch" => {
            if has("--") || has(".") || has("-f") || has("--force") || non_flag_count() > 1 {
                "checkout"
            } else {
                return None;
            }
        }
        "restore" => {
            let staged = has("--staged") || has("-s");
            let worktree = has("--worktree") || has("-w");
            if !staged || worktree {
                "restore"
            } else {
                return None; // staged-only: unstage, reversible by re-adding
            }
        }
        "clean" => {
            if !args.iter().any(|a| {
                *a == "--force" || (a.starts_with('-') && !a.starts_with("--") && a.contains('f'))
            }) {
                return None;
            }
            "clean"
        }
        "stash" => {
            let sub2 = args
                .iter()
                .find(|a| !a.starts_with('-'))
                .copied()
                .unwrap_or("");
            if matches!(sub2, "list" | "show" | "apply" | "branch") {
                return None;
            }
            "stash"
        }
        "branch" => {
            if !(has("-d") || has("--delete")) {
                return None;
            }
            "branch"
        }
        "push" => {
            if !args.iter().any(|a| a.starts_with("--force") || *a == "-f") {
                return None;
            }
            "push"
        }
        _ => return None,
    };
    Some(word)
}

/// Detect git ops that warrant a human checkpoint: the history-rewriting
/// ops (commit / rebase / reset / tag) AND the argument-aware discard forms.
/// These are neither destructive (recoverable via reflog) nor egress (local),
/// so they fall through the other gates and would otherwise silently Allow in
/// Auto. Returns the matched git subcommand word (for session-scoped consent)
/// or None. Closes the global-option / subshell / env-assignment bypasses
/// (git -C <dir> checkout ., (git checkout .), FOO=bar git checkout .) via
/// git_sub_and_args — the same fix core's snapshot trigger uses.
pub(crate) fn should_ask_before_git(tool_name: &str, content: &str) -> Option<&'static str> {
    collect_git_ask_ops(tool_name, content).into_iter().next()
}

/// Every git checkpoint or discard subcommand present across all segments of a
/// shell command, in order of appearance.
pub(crate) fn collect_git_ask_ops(tool_name: &str, content: &str) -> Vec<&'static str> {
    let lower = tool_name.to_ascii_lowercase();
    if !matches!(lower.as_str(), "bash" | "sh" | "exec" | "shell") {
        return Vec::new();
    }
    if content.is_empty() {
        return Vec::new();
    }
    let expanded = expand_inline_interpreter(content);
    let cleaned = strip_safe_redirects(&expanded);
    let lower_scan = cleaned.to_ascii_lowercase();
    let mut ops = Vec::new();
    for segment in lower_scan.split(['&', ';', '|', '\n', '(', ')', '`']) {
        let raw_tokens: Vec<&str> = segment.split_whitespace().collect();
        let unquoted: Vec<String> = raw_tokens.iter().map(|t| decode_token(t)).collect();
        let token_refs: Vec<&str> = unquoted.iter().map(String::as_str).collect();
        let Some((sub, args)) = git_sub_and_args(&token_refs) else {
            continue;
        };
        let matched = match sub {
            "commit" => Some("commit"),
            "rebase" => Some("rebase"),
            "reset" => {
                if args
                    .iter()
                    .take_while(|&&a| a != "--")
                    .any(|&a| matches!(a, "--hard" | "--merge" | "--keep"))
                {
                    Some("reset --hard")
                } else {
                    Some("reset")
                }
            }
            "tag" => Some("tag"),
            _ => git_discard_consent_word(sub, &args),
        };
        if let Some(word) = matched {
            ops.push(word);
        }
    }
    ops
}

/// Find the git subcommand + its args in a segment's tokens, skipping leading
/// shell env-assignments (FOO=bar), subshell punctuation on the git token,
/// and git global options that take a value (-C val, -c val, --git-dir[=val],
/// --work-tree[=val], --namespace[=val], --exec-path[=val]). Returns None
/// when the segment's first command is not git. Matches core's
/// agent::git_discard::git_sub_and_args — keep both in sync.
fn git_sub_and_args<'a>(tokens: &[&'a str]) -> Option<(&'a str, Vec<&'a str>)> {
    let mut i = 0;
    // Skip leading shell control keywords and env-assignments (FOO=bar) to reach "git".
    while i < tokens.len() {
        let t = trim_shell_punct(tokens[i]);
        if t.is_empty() || is_shell_control_keyword(t) {
            i += 1;
            continue;
        }
        if t == "git" {
            break;
        }
        if !t.starts_with('-') && t.contains('=') {
            i += 1;
            continue;
        }
        return None;
    }
    i += 1; // past "git"
    while i < tokens.len() {
        let t = trim_shell_punct(tokens[i]);
        // --flag=value form of a global value option.
        if let Some(eq) = t.find('=')
            && is_git_global_value_flag(&t[..eq])
        {
            i += 1;
            continue;
        }
        // bare global option taking a value (next token consumed).
        if is_git_global_value_flag(t) {
            i += 2;
            continue;
        }
        break;
    }
    if i >= tokens.len() {
        return None; // bare git with no subcommand
    }
    let sub = strip_quotes(trim_shell_punct(tokens[i]));
    let args: Vec<&str> = tokens[i + 1..]
        .iter()
        .map(|t| strip_quotes(trim_shell_punct(t)))
        .collect();
    Some((sub, args))
}

/// True when a flag is a git global option that consumes the following token
/// as its value.
fn is_git_global_value_flag(flag: &str) -> bool {
    matches!(
        flag,
        "-C" | "-c" | "--git-dir" | "--work-tree" | "--namespace" | "--exec-path"
    )
}

/// Strip leading ( { and trailing ) } shell punctuation so a subshell-wrapped
/// command still parses.
fn trim_shell_punct(s: &str) -> &str {
    let s = s.trim_start_matches(['(', '{']);
    s.trim_end_matches([')', '}'])
}

/// Strip a matching pair of surrounding quotes from a token (so git "push"
/// matches the bare push word). Delegates to mode::strip_quotes to avoid a
/// second copy of the quote-strip logic.
fn strip_quotes(s: &str) -> &str {
    crate::pipeline::detection::strip_quotes(s)
}

pub(crate) fn is_shell_control_keyword(word: &str) -> bool {
    matches!(
        word,
        "if" | "then"
            | "elif"
            | "else"
            | "fi"
            | "for"
            | "while"
            | "until"
            | "do"
            | "done"
            | "time"
            | "!"
    )
}

pub(crate) fn decode_token(raw: &str) -> String {
    let trimmed = trim_shell_punct(raw);
    let mut out = String::with_capacity(trimmed.len());
    let chars: Vec<char> = trimmed.chars().collect();
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
        if c == '\\' && !in_single && i + 1 < chars.len() {
            out.push(chars[i + 1]);
            i += 2;
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests;
