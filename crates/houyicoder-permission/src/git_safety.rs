//! The git-discard consent classifier (CONSERVATIVE). This is the
//! asymmetric half of the two-gate design:
//!
//! - consent (this crate, git_discard_consent_word): CONSERVATIVE — only the
//!   clear-cut discard forms Ask. A false positive asks the user once
//!   (annoying but safe); a false negative runs silently (the snapshot
//!   trigger in core is the backstop). Stays conservative to avoid
//!   over-asking on branch switches.
//! - snapshot (core, agent::git_safety::git_snapshot_form): AGGRESSIVE — any
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

use houyicoder_api::tool::is_shell_tool;

use crate::shell::canonical_scan;

/// The git subcommand word for a discard form, for session-scoped consent.
/// Returns None for a non-discard (a branch switch, an unstage, a stash save,
/// a normal push, a branch delete that Git itself guards).
///
/// args arrive as written. Their case carries meaning: -d and -D are two
/// different acts, and folding them together makes the guarded delete ask for
/// consent it does not need.
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
            // A branch delete has two spellings and they are not the same act.
            // The lowercase one makes Git refuse to delete a branch holding
            // work that is not merged; the uppercase one deletes it anyway.
            // Only the forced spelling discards anything, so only it asks.
            // Force may also arrive beside the safe spelling, which makes the
            // safe spelling forced.
            let forced = has("-D") || has("-f") || has("--force");
            let deleting = has("-d") || has("--delete") || forced;
            if !deleting || !forced {
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
/// the canonical scan's git_subcommand. Core's snapshot trigger keeps its own
/// matcher, so the clear-cut forms are pinned by a drift vector both copies
/// share.
pub(crate) fn should_ask_before_git(tool_name: &str, content: &str) -> Option<&'static str> {
    collect_git_ask_ops(tool_name, content).into_iter().next()
}

/// Every git checkpoint or discard subcommand present across all segments of a
/// shell command, in order of appearance.
///
/// The walk reads the canonical scan, so a git call wrapped in a timeout, a
/// loop, a substitution, or an interpreter's inline code is found where it
/// runs. Words are already decoded and quoting resolved, and the subcommand
/// keeps its case: git's branch delete has a safe spelling and a forced one,
/// and they differ only by the case of a single letter.
pub(crate) fn collect_git_ask_ops(tool_name: &str, content: &str) -> Vec<&'static str> {
    if !is_shell_tool(tool_name) {
        return Vec::new();
    }
    if content.is_empty() {
        return Vec::new();
    }
    let scan = canonical_scan(content);
    let mut ops = Vec::new();
    for command in scan.commands() {
        let Some((sub, args)) = command.git_subcommand() else {
            continue;
        };
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let sub_lower = sub.to_ascii_lowercase();
        let matched = match sub_lower.as_str() {
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
            _ => git_discard_consent_word(&sub_lower, &args),
        };
        if let Some(word) = matched {
            ops.push(word);
        }
    }
    ops
}

#[cfg(test)]
mod tests;
