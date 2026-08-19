//! The /worktrees pane data, split from wiring.rs so that file stays under
//! the file-size gate. parse_worktrees() runs git worktree list --porcelain
//! against the project root and returns structured rows the pane renders.
//! The tests exercise it against throwaway repos.

#![allow(clippy::disallowed_methods)]

use std::process::Command;

/// One linked worktree row. path is the working tree, head is the short HEAD
/// sha, branch is the ref (or a bare marker), is_current flags the worktree
/// whose path matches the project working directory.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WorktreeEntry {
    pub path: String,
    pub head: String,
    pub branch: String,
    pub is_current: bool,
}

/// Run git worktree list --porcelain against the project root and parse the
/// blocks into structured rows. Each block is a path line, a HEAD line, then
/// a branch / detached / bare marker, separated by blank lines. is_current is
/// set on the entry whose path matches the project working directory (porcelain
/// output carries no current marker, so the comparison is the source of truth).
/// Returns an empty vec when no root is set or git fails, so the pane renders
/// a fallback message rather than crashing.
pub fn parse_worktrees(repo_root: Option<&str>, working_dir: &str) -> Vec<WorktreeEntry> {
    let Some(root) = repo_root else {
        return Vec::new();
    };
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .arg("worktree")
        .arg("list")
        .arg("--porcelain")
        .stdin(std::process::Stdio::null())
        .output();
    let o = match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
        _ => return Vec::new(),
    };
    let mut entries = Vec::new();
    let mut cur = WorktreeEntry::default();
    let mut have = false;
    for line in o.lines() {
        if line.is_empty() {
            if have {
                entries.push(std::mem::take(&mut cur));
                have = false;
            }
            continue;
        }
        if let Some(path) = line.strip_prefix("worktree ") {
            if have {
                entries.push(std::mem::take(&mut cur));
            }
            cur.path = path.to_string();
            cur.head.clear();
            cur.branch.clear();
            cur.is_current = false;
            have = true;
        } else if let Some(head) = line.strip_prefix("HEAD ") {
            cur.head = head[..head.len().min(7)].to_string();
        } else if line == "detached" || line == "bare" {
            cur.branch = line.to_string();
        } else if let Some(br) = line.strip_prefix("branch ") {
            cur.branch = br.strip_prefix("refs/heads/").unwrap_or(br).to_string();
        }
    }
    if have {
        entries.push(cur);
    }
    // Mark current by path match against the project working directory. Git
    // emits canonicalized paths (on macOS /var symlinks to /private/var, and
    // git resolves it), while the working dir may be the unresolved form, so
    // the comparison canonicalizes both sides before falling back to a direct
    // compare. A miss leaves no row flagged, which is correct when the cwd is
    // not a linked worktree.
    let wd_canon = std::fs::canonicalize(working_dir).ok();
    for e in &mut entries {
        let e_canon = std::fs::canonicalize(&e.path).ok();
        let matches = match (e_canon, &wd_canon) {
            (Some(a), Some(b)) => a == *b,
            _ => e.path == working_dir,
        };
        if matches {
            e.is_current = true;
        }
    }
    entries
}

#[cfg(test)]
mod worktree_display_tests {
    use super::parse_worktrees;
    use std::process::Command;

    fn make_repo(tag: u64) -> String {
        let dir =
            std::env::temp_dir().join(format!("houyi-wt-display-{}-{tag}", std::process::id()));
        drop(std::fs::remove_dir_all(&dir));
        std::fs::create_dir_all(&dir).expect("mkdir");
        drop(
            Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(["init", "-q", "--initial-branch=main"])
                .status(),
        );
        drop(
            Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(["config", "user.email", "t@x"])
                .status(),
        );
        drop(
            Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(["config", "user.name", "t"])
                .status(),
        );
        drop(
            Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(["commit", "--allow-empty", "-m", "init", "-q"])
                .status(),
        );
        dir.to_string_lossy().into_owned()
    }

    /// parse_worktrees returns the main worktree row for a fresh repo, with
    /// the branch name stripped of its refs/heads/ prefix, the head sha
    /// shortened to seven characters, and is_current set when the working dir
    /// matches (git canonicalizes the path, so the compare is canonical).
    #[test]
    fn test_parse_returns_main_row() {
        let repo = make_repo(2);
        let entries = parse_worktrees(Some(&repo), &repo);
        assert!(!entries.is_empty(), "at least the main worktree");
        let main = &entries[0];
        assert_eq!(
            std::fs::canonicalize(&main.path).ok(),
            std::fs::canonicalize(&repo).ok(),
            "main worktree path canonicalizes to the repo root"
        );
        assert_eq!(main.branch, "main", "branch stripped of refs/heads/");
        assert_eq!(main.head.len(), 7, "head sha shortened to 7 chars");
        assert!(main.is_current, "main flagged current when cwd matches");
        std::fs::remove_dir_all(&repo).ok();
    }

    /// parse_worktrees marks a detached-HEAD worktree's branch as
    /// "detached" (git porcelain emits the bare word, no parens). Pinned
    /// because an earlier match on "(detached)" never hit, leaving the
    /// branch empty and the pane rendering an empty bracket.
    #[test]
    fn test_parse_marks_detached_branch() {
        use std::process::Command;
        let repo = make_repo(3);
        let wt = std::env::temp_dir().join(format!("houyi-wt-detached-{}-3", std::process::id()));
        drop(std::fs::remove_dir_all(&wt));
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(["worktree", "add", "--detach", &wt.to_string_lossy(), "-q"])
                .status()
                .map(|s| s.success())
                .unwrap_or(false),
            "git worktree add --detach"
        );
        let entries = parse_worktrees(Some(&repo), &repo);
        let wt_canon = std::fs::canonicalize(&wt).ok();
        let detached = entries
            .iter()
            .find(|e| std::fs::canonicalize(&e.path).ok() == wt_canon);
        let detached = detached.expect("detached worktree listed");
        assert_eq!(detached.branch, "detached", "detached HEAD marked");
        std::fs::remove_dir_all(&repo).ok();
        std::fs::remove_dir_all(&wt).ok();
    }

    /// parse_worktrees returns an empty vec when no root is given, so the pane
    /// renders the fallback message rather than indexing into nothing.
    #[test]
    fn test_parse_empty_no_root() {
        assert!(parse_worktrees(None, "/tmp").is_empty());
    }
}
