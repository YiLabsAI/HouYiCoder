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
            if !matches!(sub2, "drop" | "clear" | "pop") {
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
    let lower = tool_name.to_ascii_lowercase();
    if !matches!(lower.as_str(), "bash" | "sh" | "exec" | "shell") {
        return None;
    }
    if content.is_empty() {
        return None;
    }
    let scan = crate::pipeline::detection::interpreter_inline_code(content).unwrap_or(content);
    let lower_scan = scan.to_ascii_lowercase();
    for segment in lower_scan.split(['&', ';', '|', '\n']) {
        let tokens: Vec<&str> = segment.split_whitespace().collect();
        let Some((sub, args)) = git_sub_and_args(&tokens) else {
            continue;
        };
        match sub {
            "commit" => return Some("commit"),
            "rebase" => return Some("rebase"),
            "reset" => return Some("reset"),
            "tag" => return Some("tag"),
            _ => {}
        }
        if let Some(word) = git_discard_consent_word(sub, &args) {
            return Some(word);
        }
    }
    None
}

/// Find the git subcommand + its args in a segment's tokens, skipping leading
/// shell env-assignments (FOO=bar), subshell punctuation on the git token,
/// and git global options that take a value (-C val, -c val, --git-dir[=val],
/// --work-tree[=val], --namespace[=val], --exec-path[=val]). Returns None
/// when the segment's first command is not git. Matches core's
/// agent::git_discard::git_sub_and_args — keep both in sync.
fn git_sub_and_args<'a>(tokens: &[&'a str]) -> Option<(&'a str, Vec<&'a str>)> {
    let mut i = 0;
    // Skip leading shell env-assignments (FOO=bar) to reach "git".
    while i < tokens.len() {
        let t = trim_shell_punct(tokens[i]);
        if t.is_empty() {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Shared drift vector: the commands where the conservative consent
    /// matcher + core's aggressive snapshot matcher MUST agree (clear-cut
    /// discards). Core's agent::git_discard holds an identical vector + an
    /// expected table; keep both in sync. Asymmetric cases (single-path
    /// checkout, bare stash) live in their own crate-specific tests.
    const DRIFT_VECTOR: &[&str] = &[
        "git checkout .",
        "git checkout -- file.rs",
        "git checkout -f main",
        "git checkout --force",
        "git restore file.rs",
        "git restore -w file.rs",
        "git clean -fd",
        "git clean --force",
        "git stash drop",
        "git stash clear",
        "git stash pop",
        "git branch -D feature",
        "git branch -d feature",
        "git push --force origin main",
        "git push --force-with-lease",
        "git checkout main && git status",
        "git status",
        "git log --oneline",
        "git stash list",
        "git stash apply",
    ];

    #[test]
    fn test_drift_vector_consent_matches() {
        // Matches core's drift_vector_conservative_matches — if either drifts,
        // the other crate's test fails too. Expected: true for the 15 clear
        // discards, false for the 5 non-discards (switch + status ops).
        let expected = [
            true, true, true, true, true, true, true, true, true, true, true, true, true, true,
            true, false, false, false, false, false,
        ];
        assert_eq!(DRIFT_VECTOR.len(), expected.len());
        for (cmd, exp) in DRIFT_VECTOR.iter().zip(expected.iter()) {
            let lower = cmd.to_ascii_lowercase();
            // Split on compound separators like the real matchers, take the
            // first git segment.
            let (sub, args) = lower
                .split(['&', ';', '|', '\n'])
                .find_map(|seg| git_sub_and_args(&seg.split_whitespace().collect::<Vec<_>>()))
                .unwrap_or_else(|| panic!("no git sub in {cmd}"));
            assert_eq!(
                git_discard_consent_word(sub, &args).is_some(),
                *exp,
                "consent drift on {cmd}"
            );
        }
    }

    /// The bypasses that used to slip past the consent gate (review cases A,
    /// C, + the --force-with-lease= nit). Each now correctly consents.
    #[test]
    fn test_consent_bypasses_closed() {
        for cmd in [
            "git -C /path checkout .",
            "git -c user.name=x checkout .",
            "git --work-tree /x checkout .",
            "git --git-dir=/x checkout .",
            "git restore file.rs",
            "git clean -fd",
            "git stash drop",
            "git branch -D feature",
            "git push --force origin main",
            "git push --force-with-lease=origin/main",
            "(git checkout .)",
            "{ git checkout . ; }",
            "FOO=bar git checkout .",
            "bash -c \"git checkout .\"",
        ] {
            assert!(
                should_ask_before_git("bash", cmd).is_some(),
                "{cmd} should consent (bypass closed)"
            );
        }
        // Non-discards + single-path checkout (asymmetric: consent does NOT
        // ask; the snapshot trigger in core does snapshot it).
        for cmd in [
            "git checkout main",
            "git checkout src/main.rs",
            "git status",
            "git log",
            "git stash list",
            "git stash apply",
            "git branch feature",
            "git restore --staged file.rs",
            "ls -la",
        ] {
            assert!(
                should_ask_before_git("bash", cmd).is_none(),
                "{cmd} should NOT consent"
            );
        }
    }
}
