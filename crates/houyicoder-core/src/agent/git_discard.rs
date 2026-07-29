//! Classify git commands that may discard uncommitted or unpushed work. Two
//! asymmetric predicates serve the two gates that need this signal:
//!
//! - git_discard_form (CONSERVATIVE): the clear-cut discard forms only. The
//!   consent gate (permission crate) uses this — a false positive asks the
//!   user once (annoying but safe), a false negative runs silently (the
//!   snapshot trigger is the backstop), so it stays conservative to avoid
//!   over-asking on branch switches.
//! - git_snapshot_form (AGGRESSIVE): any git op that MAY touch the working
//!   tree. The snapshot trigger (is_destructive_command, this crate) uses
//!   this — a false positive is one free CoW snapshot (silent), a false
//!   negative is permanent data loss, so the threshold is LOW.
//!
//! The asymmetry is by design: the two gates have opposite cost profiles, so
//! they carry different thresholds. Layering forbids the permission crate from
//! importing this matcher (permission depends on core, not vice versa), so the
//! permission crate holds its own conservative copy — the two are kept in
//! sync by a shared test vector (DRIFT_VECTOR) duplicated in both crates.
//! A bare git checkout <branch> switch is NOT a discard (conservative) but
//! DOES trigger a snapshot (aggressive) — the static matcher cannot tell a
//! branch name from a file path, so the snapshot side treats any single-path
//! checkout as a potential discard.

/// True when an already-extracted git subcommand + its args form a CLEAR-CUT
/// working-tree or history discard. args are lowercased by the caller (git
/// flags -d/-D collapse to the same intent — both delete a branch — so
/// case-folding is safe). A bare git checkout <branch> switch is NOT a
/// discard (the consent gate stays conservative; the snapshot trigger covers
/// the ambiguous case).
pub fn git_discard_form(sub: &str, args: &[&str]) -> bool {
    let sub = sub.to_ascii_lowercase();
    let has_flag = |flag: &str| args.contains(&flag);
    let non_flag_count = || {
        args.iter()
            .filter(|a| !a.starts_with('-') && **a != "--")
            .count()
    };
    match sub.as_str() {
        "checkout" | "switch" => {
            has_flag("--")
                || has_flag(".")
                || has_flag("-f")
                || has_flag("--force")
                || non_flag_count() > 1
        }
        "restore" => {
            let staged = has_flag("--staged") || has_flag("-s");
            let worktree = has_flag("--worktree") || has_flag("-w");
            !staged || worktree
        }
        "clean" => args.iter().any(|a| {
            *a == "--force" || (a.starts_with('-') && !a.starts_with("--") && a.contains('f'))
        }),
        "stash" => {
            let sub2 = args
                .iter()
                .find(|a| !a.starts_with('-'))
                .copied()
                .unwrap_or("");
            matches!(sub2, "drop" | "clear" | "pop")
        }
        "branch" => has_flag("-d") || has_flag("--delete"),
        "push" => args.iter().any(|a| a.starts_with("--force") || *a == "-f"),
        _ => false,
    }
}

/// True when a git op MAY touch the working tree — the aggressive threshold
/// for the snapshot trigger. Any checkout/restore/switch with a non-flag arg
/// (a single token could be a file path the static matcher cannot
/// distinguish from a branch), plus bare git stash (save + remove from the
/// worktree), plus the clear discard forms. A bare git checkout (no args)
/// prints usage -> no snapshot.
pub fn git_snapshot_form(sub: &str, args: &[&str]) -> bool {
    let sub = sub.to_ascii_lowercase();
    let has_flag = |flag: &str| args.contains(&flag);
    let non_flag_count = || {
        args.iter()
            .filter(|a| !a.starts_with('-') && **a != "--")
            .count()
    };
    match sub.as_str() {
        "checkout" | "switch" | "restore" => {
            non_flag_count() >= 1 || has_flag("--") || has_flag("-f") || has_flag("--force")
        }
        "clean" => args.iter().any(|a| {
            *a == "--force" || (a.starts_with('-') && !a.starts_with("--") && a.contains('f'))
        }),
        "stash" => {
            let sub2 = args
                .iter()
                .find(|a| !a.starts_with('-'))
                .copied()
                .unwrap_or("");
            sub2.is_empty() || matches!(sub2, "drop" | "clear" | "pop")
        }
        "branch" => has_flag("-d") || has_flag("--delete"),
        "push" => args.iter().any(|a| a.starts_with("--force") || *a == "-f"),
        _ => false,
    }
}

/// True when a full bash command string contains a git op that should trigger
/// a pre-run snapshot. Splits on compound separators (& ; | \n), unwraps
/// bash -c "..." inline code, skips leading shell env-assignments
/// (FOO=bar) + subshell punctuation, and skips git global options that take a
/// value (-C val, -c val, --git-dir[=val], --work-tree[=val],
/// --namespace[=val], --exec-path[=val]) before reading the subcommand — so
/// git -C /path checkout . and git -c user.name=x checkout . are NOT
/// bypassed. Uses the aggressive git_snapshot_form (the snapshot trigger
/// tolerates false positives; the consent gate uses its own conservative copy).
pub fn command_triggers_git_snapshot(command: &str) -> bool {
    let scan = interpreter_inline_code(command).unwrap_or(command);
    let lower = scan.to_ascii_lowercase();
    for segment in lower.split(['&', ';', '|', '\n']) {
        let tokens: Vec<&str> = segment.split_whitespace().collect();
        if let Some((sub, args)) = git_sub_and_args(&tokens)
            && git_snapshot_form(sub, &args)
        {
            return true;
        }
    }
    false
}

/// Find the git subcommand + its args in a segment's tokens, skipping leading
/// shell env-assignments (FOO=bar), subshell punctuation on the git token,
/// and git global options that take a value. Returns None when the segment's
/// first command is not git. Tokens are lowercased by the caller; this helper
/// does not re-lowercase (so callers can pass mixed-case + lower themselves).
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
    let git_i = i;
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
    let _ = git_i;
    Some((sub, args))
}

/// True when a flag is a git global option that consumes the following token
/// as its value (-C <dir>, -c <k=v>, --git-dir <path>, --work-tree <path>,
/// --namespace <ns>, --exec-path <path>).
fn is_git_global_value_flag(flag: &str) -> bool {
    matches!(
        flag,
        "-C" | "-c" | "--git-dir" | "--work-tree" | "--namespace" | "--exec-path"
    )
}

/// Strip leading ( { and trailing ) } shell punctuation so a subshell-wrapped
/// command like (git checkout .) or { git checkout . ; } still parses.
fn trim_shell_punct(s: &str) -> &str {
    let s = s.trim_start_matches(['(', '{']);
    s.trim_end_matches([')', '}'])
}

/// Strip a matching pair of surrounding quotes from a token so git "push"
/// matches the bare push word.
fn strip_quotes(s: &str) -> &str {
    let b = s.as_bytes();
    if s.len() >= 2 && (b[0] == b'"' || b[0] == b'\'') && b[0] == b[s.len() - 1] {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

/// Unwrap bash -c "git ..." / sh -c "..." to the inline code. Returns
/// None when the command is not an interpreter invocation.
fn interpreter_inline_code(content: &str) -> Option<&str> {
    let trimmed = content.trim_start();
    for interp in ["bash -c", "sh -c", "zsh -c", "exec -c"] {
        if let Some(rest) = trimmed.strip_prefix(interp) {
            let rest = rest.trim_start();
            let code = rest.strip_prefix('"').or_else(|| rest.strip_prefix('\''))?;
            let code = code
                .strip_suffix('"')
                .or_else(|| code.strip_suffix('\''))
                .unwrap_or(code);
            return Some(code.trim());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_checkout_switch_not_consent() {
        assert!(!git_discard_form("checkout", &["main"]));
        assert!(!git_discard_form("checkout", &["feature-branch"]));
        assert!(!git_discard_form("checkout", &[]));
    }

    #[test]
    fn test_checkout_path_force_discard() {
        assert!(git_discard_form("checkout", &["--", "file.rs"]));
        assert!(git_discard_form("checkout", &["."]));
        assert!(git_discard_form("checkout", &["-f", "main"]));
        assert!(git_discard_form("checkout", &["--force"]));
        assert!(git_discard_form("checkout", &["file1", "file2"]));
        assert!(git_discard_form("checkout", &["main", "file"]));
    }

    #[test]
    fn test_restore_discard_rules() {
        assert!(git_discard_form("restore", &["file.rs"]));
        assert!(!git_discard_form("restore", &["--staged", "file.rs"]));
        assert!(git_discard_form(
            "restore",
            &["--staged", "--worktree", "file.rs"]
        ));
        assert!(git_discard_form("restore", &["-w", "file.rs"]));
    }

    #[test]
    fn test_clean_needs_force_flag() {
        assert!(git_discard_form("clean", &["-f"]));
        assert!(git_discard_form("clean", &["-fd"]));
        assert!(git_discard_form("clean", &["-df"]));
        assert!(git_discard_form("clean", &["--force"]));
        assert!(!git_discard_form("clean", &["-x"]));
        assert!(!git_discard_form("clean", &[]));
    }

    #[test]
    fn test_stash_drop_clear_pop() {
        assert!(git_discard_form("stash", &["drop"]));
        assert!(git_discard_form("stash", &["drop", "stash@{0}"]));
        assert!(git_discard_form("stash", &["clear"]));
        assert!(git_discard_form("stash", &["pop"]));
        assert!(!git_discard_form("stash", &[]));
        assert!(!git_discard_form("stash", &["list"]));
        assert!(!git_discard_form("stash", &["apply"]));
    }

    #[test]
    fn test_branch_delete_discard() {
        assert!(git_discard_form("branch", &["-d", "feature"]));
        assert!(git_discard_form("branch", &["--delete", "feature"]));
        assert!(!git_discard_form("branch", &["feature"]));
        assert!(!git_discard_form("branch", &["-a"]));
    }

    #[test]
    fn test_push_force_discard() {
        assert!(git_discard_form("push", &["--force"]));
        assert!(git_discard_form("push", &["--force-with-lease"]));
        assert!(git_discard_form("push", &["-f", "origin", "main"]));
        // = form not eaten by an env filter (the global-option skip handles it).
        assert!(git_discard_form(
            "push",
            &["--force-with-lease=origin/main"]
        ));
        assert!(!git_discard_form("push", &["origin", "main"]));
    }

    #[test]
    fn test_non_discard_subcommands() {
        assert!(!git_discard_form("add", &["file"]));
        assert!(!git_discard_form("commit", &["-m", "x"]));
        assert!(!git_discard_form("status", &[]));
        assert!(!git_discard_form("log", &[]));
    }

    /// Aggressive snapshot: a single-path checkout DOES trigger a snapshot
    /// (the static matcher cannot tell a branch from a file path). A bare
    /// branch switch also snapshots (cheap, harmless) — the asymmetry vs the
    /// conservative consent matcher is by design.
    #[test]
    fn test_snapshot_path_checkout_triggers() {
        assert!(git_snapshot_form("checkout", &["src/main.rs"]));
        assert!(git_snapshot_form("checkout", &["main"]));
        assert!(git_snapshot_form("checkout", &["--", "file.rs"]));
        assert!(git_snapshot_form("restore", &["file.rs"]));
        assert!(git_snapshot_form("stash", &[])); // bare stash saves + removes
        assert!(git_snapshot_form("stash", &["drop"]));
        assert!(git_snapshot_form("clean", &["-fd"]));
        assert!(git_snapshot_form("branch", &["-d", "x"]));
        assert!(git_snapshot_form("push", &["--force"]));
        // bare checkout (no args) + non-touching ops do NOT snapshot.
        assert!(!git_snapshot_form("checkout", &[]));
        assert!(!git_snapshot_form("status", &[]));
        assert!(!git_snapshot_form("log", &[]));
        assert!(!git_snapshot_form("stash", &["list"]));
    }

    /// The global-option + subshell + env-assignment bypasses (review cases A,
    /// C, + the --force-with-lease= nit). Each of these used to slip past the
    /// matcher entirely.
    #[test]
    fn test_snapshot_bypasses_closed() {
        // git -C <dir> checkout . — was bypassed (sub read as "-c").
        assert!(command_triggers_git_snapshot("git -C /path checkout ."));
        // git -c user.name=x checkout . — was bypassed (-c value eaten).
        assert!(command_triggers_git_snapshot(
            "git -c user.name=x checkout ."
        ));
        // git --work-tree /x checkout . — was bypassed.
        assert!(command_triggers_git_snapshot(
            "git --work-tree /x checkout ."
        ));
        // git --git-dir=/x checkout . — = form.
        assert!(command_triggers_git_snapshot("git --git-dir=/x checkout ."));
        // single-path checkout — the original data-loss path, now snapshots.
        assert!(command_triggers_git_snapshot("git checkout src/main.rs"));
        // subshell-wrapped.
        assert!(command_triggers_git_snapshot("(git checkout .)"));
        assert!(command_triggers_git_snapshot("{ git checkout . ; }"));
        // env-assignment prefix.
        assert!(command_triggers_git_snapshot("FOO=bar git checkout ."));
        // --force-with-lease= not eaten by an = filter.
        assert!(command_triggers_git_snapshot(
            "git push --force-with-lease=origin/main"
        ));
        // compound + interpreter-wrapped.
        assert!(command_triggers_git_snapshot(
            "git checkout . && git status"
        ));
        assert!(command_triggers_git_snapshot("bash -c \"git checkout .\""));
        assert!(command_triggers_git_snapshot("sh -c 'git clean -fd'"));
        // non-git + bare branch switch (aggressive: snapshots anyway, harmless).
        assert!(!command_triggers_git_snapshot("ls -la"));
        assert!(!command_triggers_git_snapshot("rm file"));
        assert!(!command_triggers_git_snapshot("git status"));
        assert!(!command_triggers_git_snapshot("git log"));
    }

    /// Shared drift vector: the commands where the conservative consent
    /// matcher + the aggressive snapshot matcher MUST agree (clear-cut
    /// discards). The permission crate holds an identical vector asserting
    /// its conservative copy — keep both in sync. Asymmetric cases (single-
    /// path checkout, bare stash) live in their own crate-specific tests.
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
        "git checkout main && git status", // switch segment not a discard
        "git status",
        "git log --oneline",
        "git stash list",
        "git stash apply",
    ];

    #[test]
    fn test_drift_vector_conservative_matches() {
        // The conservative git_discard_form (matches the permission crate's
        // git_discard_consent_word) on the shared DRIFT_VECTOR. If this
        // drifts from the permission side, the other crate's drift test
        // fails too — see permission/src/git_discard.rs DRIFT_VECTOR.
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
                git_discard_form(sub, &args),
                *exp,
                "conservative drift on {cmd}"
            );
        }
    }
}
