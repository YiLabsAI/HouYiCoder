//! Pure git-worktree mechanics for the worktree isolation feature: slug
//! validation, path and branch derivation, get-or-create, and change
//! counting. Preserves the boundaries (per-segment slug allowlist, flatten
//! to dodge the git directory-vs-file conflict, the -b flag so a name
//! collision fails instead of silently clobbering, fail-closed change
//! counting) and the design logic (fast-resume of an existing worktree,
//! the no-prompt env so a credential prompt cannot hang the session).
//! Host-side
//! git probing (not a sandboxed tool run), so the spawn-chokepoint rule is
//! allowed here as it is for the composition root git slug probe.

use std::path::{Path, PathBuf};
use std::process::Command;

use houyicoder_context::SessionId;

/// Per-segment allowlist: letters, digits, dot, underscore, dash. The
/// allowlist is the single source — no extra chars, no path-traversal
/// segments.
const VALID_SLUG_SEGMENT: &str =
    "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789._-";
const MAX_SLUG_LEN: usize = 64;

/// Errors the worktree mechanics can hit. Slug validation, git failures, and
/// name collisions are distinct so the caller can surface the right message
/// (a collision asks the model to pick a new slug; a git failure is a hard
/// stop; a non-repo tells the user to run from a git work tree).
#[derive(Debug)]
pub enum WorktreeError {
    /// Slug failed validation (bad chars, a dot or dot-dot segment, too
    /// long, empty).
    SlugInvalid(String),
    /// A git command exited non-zero (stderr carried for the message).
    Git { stderr: String },
    /// Branch or worktree path already exists — the -b flag refuses clobber.
    Collision { slug: String },
    /// Not in a git work tree.
    NotARepo,
}

impl std::fmt::Display for WorktreeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SlugInvalid(m) => write!(f, "{m}"),
            Self::Git { stderr } => write!(f, "git failed: {}", stderr.trim()),
            Self::Collision { slug } => write!(
                f,
                "worktree name \"{slug}\" already exists; pick a different name"
            ),
            Self::NotARepo => write!(f, "not in a git work tree"),
        }
    }
}

impl std::error::Error for WorktreeError {}

/// Validate a worktree slug to prevent path traversal and directory escape.
/// Each slash-separated segment must be non-empty, match the allowlist, and
/// not be a single dot or dot-dot. Max 64 chars total. A leading or trailing
/// slash is rejected because splitting yields an empty segment. Nesting
/// (user/feature) is allowed at the slug level but flattened before it
/// reaches the filesystem (see flatten_slug) — nesting on disk is unsafe
/// (the git directory-vs-file conflict, plus a parent worktree remove
/// deletes children with uncommitted work).
pub fn validate_worktree_slug(slug: &str) -> Result<(), WorktreeError> {
    if slug.len() > MAX_SLUG_LEN {
        return Err(WorktreeError::SlugInvalid(format!(
            "worktree name must be {MAX_SLUG_LEN} characters or fewer (got {})",
            slug.len()
        )));
    }
    for segment in slug.split('/') {
        if segment == "." || segment == ".." {
            return Err(WorktreeError::SlugInvalid(format!(
                "worktree name \"{slug}\" must not contain \".\" or \"..\" path segments"
            )));
        }
        if segment.is_empty() || !segment.chars().all(|c| VALID_SLUG_SEGMENT.contains(c)) {
            return Err(WorktreeError::SlugInvalid(format!(
                "worktree name \"{slug}\": each segment must be non-empty and contain only letters, digits, dots, underscores, and dashes"
            )));
        }
    }
    Ok(())
}

/// Flatten nested slugs (user/feature becomes user+feature) for both the
/// branch name and the directory path. Nesting is unsafe in either location:
/// git refs hit a directory-vs-file conflict (the worktree-user file vs the
/// worktree-user/feature directory), and a directory nest means a parent
/// worktree remove deletes children with uncommitted work. The plus sign is
/// valid in git branch names and filesystem paths but NOT in the slug-segment
/// allowlist, so the mapping is injective.
pub fn flatten_slug(slug: &str) -> String {
    slug.replace('/', "+")
}

/// The git branch name for a worktree: the prefix followed by the flattened
/// slug.
pub fn worktree_branch_name(slug: &str) -> String {
    format!("worktree-{}", flatten_slug(slug))
}

/// The worktree directory under the repo root, in the project state dir.
pub fn worktree_path_for(repo_root: &Path, slug: &str) -> PathBuf {
    repo_root
        .join(".houyicoder")
        .join("worktrees")
        .join(flatten_slug(slug))
}

/// The worktree container dir under the repo root.
fn worktrees_dir(repo_root: &Path) -> PathBuf {
    repo_root.join(".houyicoder").join("worktrees")
}

/// A live worktree session identity, captured at enter so exit can restore the
/// cwd and fence and decide keep vs remove.
#[derive(Clone, Debug)]
pub struct WorktreeSession {
    pub original_cwd: PathBuf,
    pub worktree_path: PathBuf,
    pub worktree_name: String,
    pub worktree_branch: Option<String>,
    pub original_branch: Option<String>,
    /// The HEAD commit at worktree creation — the baseline for counting
    /// commits made inside the worktree (None when the baseline could not be
    /// captured, which makes change-counting fail-closed).
    pub original_head_commit: Option<String>,
    pub session_id: SessionId,
}

/// Result of get-or-create: a fresh worktree vs a resumed one (fast resume
/// skips the git fetch and worktree add on the happy path).
#[derive(Clone, Debug)]
pub struct WorktreeCreateResult {
    pub worktree_path: PathBuf,
    pub worktree_branch: String,
    pub head_commit: Option<String>,
    pub existed: bool,
}

/// Env vars that stop git/SSH from opening a credential prompt (which would
/// hang the session — there is no tty to answer it). GIT_TERMINAL_PROMPT=0
/// keeps git off /dev/tty; an empty GIT_ASKPASS disables GUI askpass programs.
const GIT_NO_PROMPT_ENV: [(&str, &str); 2] = [("GIT_TERMINAL_PROMPT", "0"), ("GIT_ASKPASS", "")];

/// Run git with the no-prompt env and a closed stdin, returning stdout or a
/// Git error on non-zero exit. The repo root is the -C target. Host-side git
/// probing (not a sandboxed tool run), so the spawn-chokepoint rule is allowed
/// here as it is for the composition root git slug probe.
#[expect(clippy::disallowed_methods, reason = "infra spawn, not model-driven")]
fn run_git(repo_root: &Path, args: &[&str]) -> Result<String, WorktreeError> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(args)
        .envs(GIT_NO_PROMPT_ENV)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| WorktreeError::Git {
            stderr: format!("spawn git: {e}"),
        })?;
    if !out.status.success() {
        return Err(WorktreeError::Git {
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Canonicalize so the macOS /var to /private/var symlink does not make a
/// seatbelt realpath starts_with check reject legitimate paths.
fn canonicalize(p: &Path) -> Result<PathBuf, WorktreeError> {
    std::fs::canonicalize(p).map_err(|e| WorktreeError::Git {
        stderr: format!("canonicalize {}: {e}", p.display()),
    })
}

/// Create a linked worktree for the slug in the project state dir, or resume
/// it if it already exists. Uses the -b flag (not -B): a name collision is a
/// hard error so the model picks a new slug rather than silently clobbering a
/// branch with real work. Base is HEAD (the origin/default fetch-skip
/// optimization is deferred).
pub fn get_or_create_worktree(
    repo_root: &Path,
    slug: &str,
) -> Result<WorktreeCreateResult, WorktreeError> {
    validate_worktree_slug(slug)?;
    let repo_root = canonicalize(repo_root)?;
    let worktree_path = worktree_path_for(&repo_root, slug);
    let worktree_branch = worktree_branch_name(slug);

    // Fast resume: if the worktree dir exists and HEAD resolves, skip the
    // add. v1 uses rev-parse (the fs-only .git-pointer optimization is
    // deferred).
    if worktree_path.exists()
        && let Ok(head) = run_git(&worktree_path, &["rev-parse", "HEAD"])
    {
        return Ok(WorktreeCreateResult {
            worktree_path,
            worktree_branch,
            head_commit: Some(head),
            existed: true,
        });
    }

    // New worktree. The -b flag refuses an existing branch (collision -> Err,
    // not clobber). The base is HEAD: the agent works off the current commit.
    std::fs::create_dir_all(worktrees_dir(&repo_root)).map_err(|e| WorktreeError::Git {
        stderr: format!("mkdir worktrees dir: {e}"),
    })?;
    run_git(
        &repo_root,
        &[
            "worktree",
            "add",
            "-b",
            &worktree_branch,
            &worktree_path.to_string_lossy(),
            "HEAD",
        ],
    )
    .map_err(|e| match &e {
        // A collision (branch or path exists) surfaces as a distinct error so
        // the caller can ask the model for a new slug.
        WorktreeError::Git { stderr }
            if stderr.contains("already exists") || stderr.contains("already used") =>
        {
            WorktreeError::Collision {
                slug: slug.to_string(),
            }
        }
        _ => e,
    })?;
    let head_commit = run_git(&worktree_path, &["rev-parse", "HEAD"]).ok();
    Ok(WorktreeCreateResult {
        worktree_path,
        worktree_branch,
        head_commit,
        existed: false,
    })
}

/// Count uncommitted files plus commits since the baseline commit inside the
/// worktree. Returns None when the state cannot be reliably determined (git
/// status or rev-list failed, or no baseline commit) — callers that use this
/// as a safety gate must treat None as unknown, assume unsafe (fail-closed).
/// A silent 0/0 would let a remove destroy real work.
pub fn count_worktree_changes(
    worktree_path: &Path,
    original_head_commit: Option<&str>,
) -> Option<ChangeSummary> {
    let status = run_git(worktree_path, &["status", "--porcelain"]).ok()?;
    let changed_files = status.lines().filter(|l| !l.is_empty()).count();
    let baseline = original_head_commit?;
    let rev_list = run_git(
        worktree_path,
        &["rev-list", "--count", &format!("{baseline}..HEAD")],
    )
    .ok()?;
    let commits = rev_list.trim().parse::<usize>().unwrap_or(0);
    Some(ChangeSummary {
        changed_files,
        commits,
    })
}

/// True when the worktree has uncommitted changes or new commits since the
/// head commit. Fail-closed: a git failure returns true so the caller errs
/// toward preserving the worktree.
pub fn has_worktree_changes(worktree_path: &Path, head_commit: &str) -> bool {
    match count_worktree_changes(worktree_path, Some(head_commit)) {
        Some(s) => s.changed_files > 0 || s.commits > 0,
        None => true,
    }
}

#[derive(Clone, Copy, Debug)]
pub struct ChangeSummary {
    pub changed_files: usize,
    pub commits: usize,
}

/// Remove a linked worktree + delete its branch (the exit remove path). Uses
/// --force so a leftover index.lock from a crashed git does not block
/// cleanup; the discard_changes safety gate earlier in the flow already
/// confirmed the user accepts the loss. Sleeps briefly so git has released
/// its locks.
pub fn remove_worktree(repo_root: &Path, slug: &str) -> Result<(), WorktreeError> {
    let repo_root = canonicalize(repo_root)?;
    let path = worktree_path_for(&repo_root, slug);
    let branch = worktree_branch_name(slug);
    drop(run_git(
        &repo_root,
        &["worktree", "remove", "--force", &path.to_string_lossy()],
    ));
    // Best-effort lock-release wait; a git that just removed the worktree may
    // hold the branch ref lock for a few ms.
    std::thread::sleep(std::time::Duration::from_millis(100));
    run_git(&repo_root, &["branch", "-D", &branch])?;
    Ok(())
}

/// The current branch name of the repo (the branch a worktree session would
/// branch off + return to). None when HEAD is detached or git fails.
pub fn current_branch(repo_root: &Path) -> Option<String> {
    run_git(repo_root, &["rev-parse", "--abbrev-ref", "HEAD"])
        .ok()
        .filter(|s| !s.is_empty() && s != "HEAD")
}

/// Resolve a ref to its commit SHA (e.g. refs/heads/main). None on failure.
pub fn rev_parse(repo_root: &Path, ref_name: &str) -> Option<String> {
    run_git(repo_root, &["rev-parse", ref_name]).ok()
}

#[cfg(test)]
#[expect(clippy::disallowed_methods, reason = "infra spawn, not model-driven")]
mod tests {
    use super::*;

    #[test]
    fn test_validates_legal_slugs() {
        assert!(validate_worktree_slug("feat-x").is_ok());
        assert!(validate_worktree_slug("user/feature").is_ok());
        assert!(validate_worktree_slug("a.b_c-d").is_ok());
    }

    #[test]
    fn test_rejects_bad_slugs() {
        assert!(validate_worktree_slug("").is_err(), "empty");
        assert!(validate_worktree_slug(".").is_err(), "dot");
        assert!(validate_worktree_slug("..").is_err(), "dotdot");
        assert!(validate_worktree_slug("a/../b").is_err(), "traversal");
        assert!(validate_worktree_slug("/leading").is_err(), "leading slash");
        assert!(
            validate_worktree_slug("trailing/").is_err(),
            "trailing slash"
        );
        assert!(validate_worktree_slug("a//b").is_err(), "empty segment");
        assert!(validate_worktree_slug("bad space").is_err(), "space");
        assert!(validate_worktree_slug("bad:colon").is_err(), "colon");
        assert!(validate_worktree_slug(&"x".repeat(65)).is_err(), "too long");
    }

    #[test]
    fn test_flatten_replaces_slash() {
        assert_eq!(flatten_slug("user/feature"), "user+feature");
        assert_eq!(flatten_slug("plain"), "plain");
        assert_eq!(flatten_slug("a/b/c"), "a+b+c");
    }

    #[test]
    fn test_branch_name_uses_slug() {
        assert_eq!(
            worktree_branch_name("user/feature"),
            "worktree-user+feature"
        );
        let repo = Path::new("/repo");
        assert_eq!(
            worktree_path_for(repo, "user/feature"),
            Path::new("/repo/.houyicoder/worktrees/user+feature")
        );
    }

    /// Smoke test against a real throwaway worktree of the host repo: create
    /// then content visible then resume same path then cleanup. Skipped when
    /// not run from a git work tree.
    #[test]
    fn test_get_create_round_trips() {
        let cwd = std::env::current_dir().expect("cwd");
        if !cwd.join(".git").exists() {
            return;
        }
        let slug = "smoke-round-trip";
        drop(cleanup_worktree(&cwd, slug));
        let r = get_or_create_worktree(&cwd, slug).expect("create");
        assert!(!r.existed, "first call creates");
        assert!(
            r.worktree_path.join("Cargo.toml").exists(),
            "worktree sees repo content"
        );
        assert_eq!(r.worktree_branch, "worktree-smoke-round-trip");
        let r2 = get_or_create_worktree(&cwd, slug).expect("resume");
        assert!(r2.existed, "second call resumes");
        assert_eq!(r.worktree_path, r2.worktree_path);
        drop(cleanup_worktree(&cwd, slug));
        assert!(
            !r.worktree_path.exists(),
            "worktree dir removed after cleanup"
        );
    }

    /// A pre-existing same-named branch makes the -b add collide (the
    /// worktree dir does not yet exist, so the fast-resume path is skipped).
    #[test]
    fn test_collision_on_existing_branch() {
        let cwd = std::env::current_dir().expect("cwd");
        if !cwd.join(".git").exists() {
            return;
        }
        let slug = "smoke-collision";
        drop(cleanup_worktree(&cwd, slug));
        let branch = worktree_branch_name(slug);
        drop(
            Command::new("git")
                .arg("-C")
                .arg(&cwd)
                .args(["branch", &branch])
                .status(),
        );
        let err = get_or_create_worktree(&cwd, slug).expect_err("collision");
        assert!(
            matches!(err, WorktreeError::Collision { .. }),
            "expected Collision, got {err:?}"
        );
        drop(
            Command::new("git")
                .arg("-C")
                .arg(&cwd)
                .args(["branch", "-D", &branch])
                .status(),
        );
    }

    /// Fail-closed: count_worktree_changes returns None when there is no
    /// baseline commit (cannot prove clean, assume unsafe).
    #[test]
    fn test_count_changes_without_baseline() {
        assert!(count_worktree_changes(Path::new("/nonexistent"), None).is_none());
    }

    fn cleanup_worktree(repo_root: &Path, slug: &str) -> Result<(), WorktreeError> {
        let path = worktree_path_for(repo_root, slug);
        let branch = worktree_branch_name(slug);
        drop(run_git(
            repo_root,
            &["worktree", "remove", "--force", &path.to_string_lossy()],
        ));
        drop(run_git(repo_root, &["branch", "-D", &branch]));
        Ok(())
    }
}
