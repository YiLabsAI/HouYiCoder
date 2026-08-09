//! Git-operation surfacing for the collapsed tool-use summary: when a bash
//! command runs git commit, git push, gh pr create, etc, the summary
//! reads "committed abc123, pushed to main, created PR #42" instead of
//! counting the call as a generic bash command. A git-operation detector:
//! regexes on the command (tolerating git's global -c/-C
//! options) + parsing the SHA / branch / PR URL out of stdout+stderr.

use std::sync::LazyLock;

use regex::Regex;

/// Build a regex matching git <subcmd> while tolerating git's global options
/// between git and the subcommand (-c key=val, -C path, --git-dir=path).
/// The model retries with git -c commit.gpgsign=false commit after a
/// signing failure; this still matches.
fn git_cmd_re(subcmd: &str, suffix: &str) -> Regex {
    Regex::new(&format!(
        r"\bgit(?:\s+-[cC]\s+\S+|\s+--\S+=\S+)*\s+{subcmd}\b{suffix}"
    ))
    .unwrap()
}

static GIT_COMMIT_RE: LazyLock<Regex> = LazyLock::new(|| git_cmd_re("commit", ""));
static GIT_PUSH_RE: LazyLock<Regex> = LazyLock::new(|| git_cmd_re("push", ""));
static GIT_CHERRY_PICK_RE: LazyLock<Regex> = LazyLock::new(|| git_cmd_re("cherry-pick", ""));
static GIT_MERGE_RE: LazyLock<Regex> = LazyLock::new(|| git_cmd_re("merge", ""));
static GIT_REBASE_RE: LazyLock<Regex> = LazyLock::new(|| git_cmd_re("rebase", ""));

static PR_COMMIT_ID_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[[\w./-]+(?: \(root-commit\))? ([0-9a-f]+)\]").unwrap());

static PR_PUSH_BRANCH_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^\s*[+\-*!= ]?\s*(?:\[new branch\]|\S+\.\.+\S+)\s+\S+\s*->\s*(\S+)").unwrap()
});

static PR_URL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"https://github\.com/([^/]+/[^/]+)/pull/(\d+)").unwrap());

static PR_URL_FIND_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"https://github\.com/[^/\s]+/[^/\s]+/pull/\d+").unwrap());

static PR_NUM_FROM_TEXT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[Pp]ull request (?:\S+#)?#?(\d+)").unwrap());

static AMEND_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\bamend\b").unwrap());

/// The kind of commit a git commit produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum CommitKind {
    Committed,
    Amended,
    CherryPicked,
}

/// The branch action a git merge / git rebase produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BranchAction {
    Merged,
    Rebased,
}

/// The PR action a gh pr <verb> produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PrAction {
    Created,
    Edited,
    Merged,
    Commented,
    Closed,
    Ready,
}

/// A parsed PR (number + optional URL + the action).
#[derive(Debug, Clone)]
pub(crate) struct PrInfo {
    pub number: u32,
    pub url: Option<String>,
    pub action: PrAction,
}

/// One git operation parsed from a bash command + its output. Stored per
/// fold group so the summary can surface "committed abc123, pushed to main".
#[derive(Debug, Clone)]
pub(crate) struct GitOp {
    pub commit: Option<(String, CommitKind)>,
    pub push: Option<String>,
    pub branch: Option<(String, BranchAction)>,
    pub pr: Option<PrInfo>,
}

/// Whether a bash command is a git operation worth surfacing (used to skip
/// counting it as a generic bash command in the summary — it leads the line
/// as a git op instead). Checks the command only; the SHA/branch/PR parsing
/// runs in detect_git_operation with the output.
pub(crate) fn is_git_op_command(command: &str) -> bool {
    GIT_COMMIT_RE.is_match(command)
        || GIT_PUSH_RE.is_match(command)
        || GIT_CHERRY_PICK_RE.is_match(command)
        || GIT_MERGE_RE.is_match(command)
        || GIT_REBASE_RE.is_match(command)
        || gh_pr_action(command).is_some()
}

/// The gh pr <verb> action, if the command is one.
fn gh_pr_action(command: &str) -> Option<PrAction> {
    if Regex::new(r"\bgh\s+pr\s+create\b")
        .unwrap()
        .is_match(command)
    {
        Some(PrAction::Created)
    } else if Regex::new(r"\bgh\s+pr\s+edit\b").unwrap().is_match(command) {
        Some(PrAction::Edited)
    } else if Regex::new(r"\bgh\s+pr\s+merge\b")
        .unwrap()
        .is_match(command)
    {
        Some(PrAction::Merged)
    } else if Regex::new(r"\bgh\s+pr\s+comment\b")
        .unwrap()
        .is_match(command)
    {
        Some(PrAction::Commented)
    } else if Regex::new(r"\bgh\s+pr\s+close\b")
        .unwrap()
        .is_match(command)
    {
        Some(PrAction::Closed)
    } else if Regex::new(r"\bgh\s+pr\s+ready\b")
        .unwrap()
        .is_match(command)
    {
        Some(PrAction::Ready)
    } else {
        None
    }
}

/// Parse the commit SHA from git commit output ([branch sha] message,
/// [branch (root-commit) sha] for the first commit). Returns the short
/// 6-char form the summary shows.
fn parse_commit_id(output: &str) -> Option<String> {
    PR_COMMIT_ID_RE
        .captures(output)
        .and_then(|c| c.get(1))
        .map(|m| {
            let sha = m.as_str();
            sha.get(..6).unwrap_or(sha).to_string()
        })
}

/// Parse the branch name from git push output (the ref-update line writes
/// to stderr; the caller passes stdout+stderr concatenated).
fn parse_push_branch(output: &str) -> Option<String> {
    PR_PUSH_BRANCH_RE
        .captures(output)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())
}

/// Find a GitHub PR URL embedded in stdout + parse the number.
fn find_pr_in_output(output: &str) -> Option<(u32, String)> {
    let m = PR_URL_FIND_RE.find(output)?;
    let caps = PR_URL_RE.captures(m.as_str())?;
    let num = caps.get(2)?.as_str().parse().ok()?;
    Some((num, m.as_str().to_string()))
}

/// Parse a PR number from gh pr merge/close/ready text output (no URL;
/// prints "pull request #N").
fn parse_pr_number_from_text(output: &str) -> Option<u32> {
    PR_NUM_FROM_TEXT_RE
        .captures(output)
        .and_then(|c| c.get(1))
        .and_then(|m| m.as_str().parse().ok())
}

/// Extract the target ref from git merge <ref> / git rebase <ref>,
/// skipping flags + keywords. First non-flag arg after the verb.
fn parse_ref_from_command(command: &str, verb: &str) -> Option<String> {
    let re = git_cmd_re(verb, "");
    let after = re.split(command).nth(1)?;
    for t in after.split_whitespace() {
        if t.starts_with('&') || t.starts_with('|') || t.starts_with(';') || t.starts_with('>') {
            break;
        }
        if t.starts_with('-') {
            continue;
        }
        return Some(t.to_string());
    }
    None
}

/// Scan a bash command + output (stdout+stderr concatenated) for git
/// operations to surface. Checks the command (so a SHA in unrelated git log
/// output does not match) + parses the SHA/branch/PR from the output — a
/// detect-git-operation pass.
pub(crate) fn detect_git_operation(command: &str, output: &str) -> GitOp {
    let mut op = GitOp {
        commit: None,
        push: None,
        branch: None,
        pr: None,
    };
    let is_cherry_pick = GIT_CHERRY_PICK_RE.is_match(command);
    if (GIT_COMMIT_RE.is_match(command) || is_cherry_pick)
        && let Some(sha) = parse_commit_id(output)
    {
        let kind = if is_cherry_pick {
            CommitKind::CherryPicked
        } else if AMEND_RE.is_match(command) {
            CommitKind::Amended
        } else {
            CommitKind::Committed
        };
        op.commit = Some((sha, kind));
    }
    if GIT_PUSH_RE.is_match(command)
        && let Some(branch) = parse_push_branch(output)
    {
        op.push = Some(branch);
    }
    if GIT_MERGE_RE.is_match(command)
        && (output.contains("Fast-forward") || output.contains("Merge made by"))
        && let Some(ref_) = parse_ref_from_command(command, "merge")
    {
        op.branch = Some((ref_, BranchAction::Merged));
    }
    if GIT_REBASE_RE.is_match(command)
        && output.contains("Successfully rebased")
        && let Some(ref_) = parse_ref_from_command(command, "rebase")
    {
        op.branch = Some((ref_, BranchAction::Rebased));
    }
    if let Some(action) = gh_pr_action(command) {
        if let Some((num, url)) = find_pr_in_output(output) {
            op.pr = Some(PrInfo {
                number: num,
                url: Some(url),
                action,
            });
        } else if let Some(num) = parse_pr_number_from_text(output) {
            op.pr = Some(PrInfo {
                number: num,
                url: None,
                action,
            });
        }
    }
    op
}

/// Build the (verb, value) pairs the summary renders for a group's git ops,
/// in a fixed order: commits (grouped by kind, SHAs joined), pushes
/// (branches deduped), branch merges/rebases, PRs. Each pair renders as
/// verb-dim + value-bold (the SHA/branch/ref/PR#N is the load-bearing part).
/// Commits group by kind so "committed abc, def" reads as one part, not two.
pub(crate) fn git_op_parts(ops: &[GitOp]) -> Vec<(String, String)> {
    use std::collections::HashMap;
    let mut parts: Vec<(String, String)> = Vec::new();
    let mut by_kind: HashMap<CommitKind, Vec<String>> = HashMap::new();
    for op in ops {
        if let Some((sha, kind)) = &op.commit {
            by_kind.entry(*kind).or_default().push(sha.clone());
        }
    }
    for kind in [
        CommitKind::Committed,
        CommitKind::Amended,
        CommitKind::CherryPicked,
    ] {
        if let Some(shas) = by_kind.get(&kind) {
            if shas.is_empty() {
                continue;
            }
            let verb = match kind {
                CommitKind::Committed => "committed",
                CommitKind::Amended => "amended commit",
                CommitKind::CherryPicked => "cherry-picked",
            };
            parts.push((verb.to_string(), shas.join(", ")));
        }
    }
    let mut pushes: Vec<String> = ops.iter().filter_map(|o| o.push.clone()).collect();
    pushes.sort();
    pushes.dedup();
    if !pushes.is_empty() {
        parts.push(("pushed to".into(), pushes.join(", ")));
    }
    for op in ops {
        if let Some((ref_, action)) = &op.branch {
            let verb = match action {
                BranchAction::Merged => "merged",
                BranchAction::Rebased => "rebased onto",
            };
            parts.push((verb.to_string(), ref_.clone()));
        }
    }
    for op in ops {
        if let Some(pr) = &op.pr {
            let verb = match pr.action {
                PrAction::Created => "created",
                PrAction::Edited => "edited",
                PrAction::Merged => "merged",
                PrAction::Commented => "commented on",
                PrAction::Closed => "closed",
                PrAction::Ready => "marked ready",
            };
            parts.push((verb.to_string(), format!("PR #{}", pr.number)));
        }
    }
    parts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_commit_sha() {
        let op = detect_git_operation("git commit -m x", "[main abc1234] x\n 1 file changed");
        assert_eq!(op.commit.as_ref().map(|(s, _)| s.as_str()), Some("abc123"));
    }

    #[test]
    fn test_detect_commit_amend() {
        let op = detect_git_operation("git commit --amend", "[main def5678] x");
        assert_eq!(
            op.commit.as_ref().map(|(_, k)| *k),
            Some(CommitKind::Amended)
        );
    }

    #[test]
    fn test_detect_cherry_pick() {
        let op = detect_git_operation("git cherry-pick abc", "[feature 111aaa] x");
        assert_eq!(
            op.commit.as_ref().map(|(_, k)| *k),
            Some(CommitKind::CherryPicked)
        );
    }

    #[test]
    fn test_detect_push_branch() {
        let op = detect_git_operation(
            "git push",
            "To github.com:o/r.git\n   abc..def  main -> main",
        );
        assert_eq!(op.push.as_deref(), Some("main"));
    }

    #[test]
    fn test_detect_pr_create_url() {
        let op = detect_git_operation("gh pr create", "https://github.com/owner/repo/pull/42");
        let pr = op.pr.expect("pr");
        assert_eq!(pr.number, 42);
        assert_eq!(pr.action, PrAction::Created);
        assert!(pr.url.is_some());
    }

    #[test]
    fn test_detect_pr_merge_text() {
        let op = detect_git_operation("gh pr merge 42", "Merged pull request owner/repo#42");
        let pr = op.pr.expect("pr");
        assert_eq!(pr.number, 42);
        assert_eq!(pr.action, PrAction::Merged);
        assert!(pr.url.is_none());
    }

    #[test]
    fn test_detect_merge_ref() {
        let op = detect_git_operation("git merge feature", "Fast-forward");
        assert_eq!(op.branch.as_ref().map(|(r, _)| r.as_str()), Some("feature"));
        assert_eq!(
            op.branch.as_ref().map(|(_, a)| *a),
            Some(BranchAction::Merged)
        );
    }

    #[test]
    fn test_detect_rebase_ref() {
        let op = detect_git_operation("git rebase main", "Successfully rebased");
        assert_eq!(op.branch.as_ref().map(|(r, _)| r.as_str()), Some("main"));
        assert_eq!(
            op.branch.as_ref().map(|(_, a)| *a),
            Some(BranchAction::Rebased)
        );
    }

    #[test]
    fn test_is_git_op_classifies() {
        assert!(is_git_op_command("git commit -m x"));
        assert!(is_git_op_command("git -c k=v push"));
        assert!(is_git_op_command("gh pr create"));
        assert!(!is_git_op_command("git log"));
        assert!(!is_git_op_command("ls -la"));
    }

    #[test]
    fn test_log_no_false_commit() {
        // A SHA in git log output does not make a non-commit command a commit.
        let op = detect_git_operation("git log", "[main abc1234] old commit");
        assert!(op.commit.is_none());
    }
}
