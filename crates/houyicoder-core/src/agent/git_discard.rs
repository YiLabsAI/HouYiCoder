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
mod tests;
