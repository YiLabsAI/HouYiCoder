//! Pure git-worktree mechanics: slug validation, path and branch derivation,
//! get-or-create, change counting, the creation record a new tree leaves, and
//! the removal that takes one apart: its checks, and what a removal that
//! stopped leaves for the next attempt.
//! Boundaries kept: a per-segment slug allowlist and flattening to dodge the
//! git directory-vs-file conflict, the -b flag so a name collision fails
//! instead of silently clobbering, and fail-closed change counting. Fast
//! resume of an existing worktree stays, and git runs without a prompt so no
//! credential prompt can hang the session. Host-side git probing is not a
//! sandboxed tool run, so the spawn-chokepoint rule is allowed here.

use std::path::{Path, PathBuf};
use std::process::Command;

use houyicoder_context::SessionId;

use super::changes::read_changes;
use super::identify::resolve_identity;
use super::record::ManagedWorktree;
use super::record::RecordLookup;
use super::record::lookup_record;
use super::record::write_managed_worktree;
#[cfg(test)]
use std::env::current_dir;
use std::error::Error;
use std::fmt::Display;
use std::fmt::Formatter;
use std::fs::create_dir_all;
use std::process::Output;
use std::process::Stdio;

mod removal;

pub use removal::remove_clean_worktree;
pub use removal::remove_previewed_worktree;
pub use removal::remove_worktree;

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
    /// The slug names no worktree this product recorded, so removal has no
    /// object it can act on and must leave the directory alone.
    Unmanaged { slug: String },
    /// The directory is not one this product created under its managed
    /// worktrees directory, so no slug of this product names it and nothing
    /// here may act on it.
    UnmanagedPath { path: PathBuf },
    /// The slug has a creation record this version refuses, so the directory
    /// cannot be established as the tree that record describes and nothing may
    /// act on it.
    RecordRejected { slug: String, reason: String },
    /// The directory is not the worktree its record describes: it resolves
    /// outside the managed root, belongs to another repository, is checked out
    /// on a branch neither the record nor the slug names, or is missing from
    /// Git's own worktree record.
    Identity { path: PathBuf, reason: String },
    /// Git records the worktree as locked, which refuses both removal and,
    /// while it stands, the session that would work in it.
    Locked { path: PathBuf, reason: String },
    /// A file or directory operation on the worktree state failed: the record
    /// could not be stored or read, or a path could not be resolved. Distinct
    /// from a Git failure so the message names the actor that failed.
    State { path: PathBuf, reason: String },
    /// A live process holds the worktree's active-use lease.
    Leased { slug: String, reason: String },
    /// The worktree's active-use lease could not be read, so whether a
    /// process holds it is unknown.
    LeaseUnknown { slug: String, reason: String },
    /// The directory was removed but its branch was not, so the act finished
    /// half done. The branch and the record are kept, and this answer names
    /// what survived rather than reporting the deleted directory as a failure
    /// that changed nothing.
    RemovalIncomplete {
        path: PathBuf,
        branch: String,
        reason: String,
    },
    /// A removal asked for as part of ending a session failed, and the session
    /// ended regardless. The fence and the working directory are already back
    /// at the repository root, so the answer has to carry both facts: what
    /// stopped the removal, and that the isolation is over.
    RemovalFailed { path: PathBuf, reason: String },
    /// Git detached the worktree's registration but the directory survived, or
    /// a recorded leftover could not be finished. The directory and the record
    /// naming it are kept, so running the same removal again finishes that one
    /// target instead of leaving a directory nothing accounts for.
    CleanupPending { path: PathBuf, reason: String },
    /// The worktree no longer matches the state its removal was previewed
    /// from, so nothing was removed. The user was shown a different tree than
    /// the one the removal would have acted on, and a new preview is what
    /// shows the difference.
    Stale { slug: String },
}

impl Display for WorktreeError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SlugInvalid(m) => write!(f, "{m}"),
            Self::Git { stderr } => write!(f, "git failed: {}", stderr.trim()),
            Self::Collision { slug } => write!(
                f,
                "worktree name \"{slug}\" already exists; pick a different name"
            ),
            Self::NotARepo => write!(f, "not in a git work tree"),
            Self::Unmanaged { slug } => write!(
                f,
                "worktree \"{slug}\" carries no creation record; it is left untouched"
            ),
            Self::UnmanagedPath { path } => write!(
                f,
                "{} is not a worktree this product created under its state directory, so it cannot be previewed, entered, or removed from here",
                path.display()
            ),
            Self::RecordRejected { slug, reason } => write!(
                f,
                "worktree \"{slug}\" has a creation record this version cannot use ({reason}); it is left untouched"
            ),
            Self::Identity { path, reason } => write!(
                f,
                "{} is not the worktree this slug names: {reason}",
                path.display()
            ),
            Self::Locked { path, reason } => write!(
                f,
                "{} is locked: {reason}; the lock has to be released before the worktree can be used or removed",
                path.display()
            ),
            Self::State { path, reason } => write!(
                f,
                "worktree state at {} could not be used: {reason}",
                path.display()
            ),
            Self::Leased { slug, reason } => {
                write!(f, "worktree \"{slug}\" is in use: {reason}")
            }
            Self::LeaseUnknown { slug, reason } => {
                write!(f, "worktree \"{slug}\" in-use state unknown: {reason}")
            }
            Self::RemovalIncomplete {
                path,
                branch,
                reason,
            } => write!(
                f,
                "{} was removed, but branch {branch} could not be deleted ({reason}); the branch and its record are kept",
                path.display()
            ),
            Self::RemovalFailed { path, reason } => write!(
                f,
                "{} was not removed: {reason}. The worktree session ended anyway, so the repository root is the working directory again and the worktree is still on disk",
                path.display()
            ),
            Self::CleanupPending { path, reason } => write!(
                f,
                "{} is no longer registered as a worktree, but its directory could not be removed: {reason}. The directory and the record naming it are kept, and a later removal of this worktree finishes it",
                path.display()
            ),
            Self::Stale { slug } => write!(
                f,
                "worktree \"{slug}\" changed since its removal was previewed; nothing was removed, and a new preview shows what it holds now"
            ),
        }
    }
}

impl Error for WorktreeError {}

/// Validate a worktree slug to prevent path traversal and directory escape.
/// Each slash-separated segment must be non-empty, match the allowlist, and
/// not be a single dot or dot-dot. Max 64 chars total. A leading or trailing
/// slash is rejected because splitting yields an empty segment. Nesting
/// (user/feature) is allowed at the slug level but flattened before it
/// reaches the filesystem (see flatten_slug) — nesting on disk is unsafe
/// (the git directory-vs-file conflict, plus a parent worktree remove
/// deletes children with uncommitted work).
pub fn validate_worktree_slug(slug: &str) -> Result<(), WorktreeError> {
    let length = slug.chars().count();
    if length > MAX_SLUG_LEN {
        return Err(WorktreeError::SlugInvalid(format!(
            "worktree name must be {MAX_SLUG_LEN} characters or fewer (got {length})"
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
pub(super) fn worktrees_dir(repo_root: &Path) -> PathBuf {
    repo_root.join(".houyicoder").join("worktrees")
}

/// The slug of the managed worktree a directory names.
///
/// A directory this product created sits directly under the managed worktrees
/// directory and carries the flattened form of its slug. The plus sign is not
/// allowed inside a slug segment, so flattening is invertible and the slug is
/// recovered from the directory name. Every other directory answers
/// UnmanagedPath: that refusal is what keeps a worktree Git lists but this
/// product never created from being entered, previewed, or removed from here.
pub fn managed_slug_for_path(repo_root: &Path, path: &Path) -> Result<String, WorktreeError> {
    let unmanaged = || WorktreeError::UnmanagedPath {
        path: path.to_path_buf(),
    };
    let managed_root = canonicalize(&worktrees_dir(repo_root)).map_err(|_| unmanaged())?;
    let dir = canonicalize(path).map_err(|_| unmanaged())?;
    let name = dir
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(unmanaged)?
        .to_string();
    let slug = name.replace('+', "/");
    validate_worktree_slug(&slug).map_err(|_| unmanaged())?;
    if dir != managed_root.join(&name) {
        return Err(unmanaged());
    }
    Ok(slug)
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

/// Run git with the no-prompt env and a closed stdin. The repo root is the -C
/// target. Host-side git probing (not a sandboxed tool run), so the
/// spawn-chokepoint rule is allowed here as it is for the composition root git
/// slug probe.
#[expect(clippy::disallowed_methods, reason = "infra spawn, not model-driven")]
fn run_git_output(repo_root: &Path, args: &[&str]) -> Result<Output, WorktreeError> {
    Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(args)
        .envs(GIT_NO_PROMPT_ENV)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| WorktreeError::Git {
            stderr: format!("spawn git: {e}"),
        })
}

/// Run git and return its stdout bytes, or a Git error on non-zero exit.
/// Bytes rather than text because a NUL-separated listing has no line to read
/// and lossy decoding would rewrite the very paths a caller compares.
pub(super) fn run_git_raw(repo_root: &Path, args: &[&str]) -> Result<Vec<u8>, WorktreeError> {
    let out = run_git_output(repo_root, args)?;
    if !out.status.success() {
        return Err(WorktreeError::Git {
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        });
    }
    Ok(out.stdout)
}

/// Run git and return its trimmed stdout as text.
pub(super) fn run_git(repo_root: &Path, args: &[&str]) -> Result<String, WorktreeError> {
    let raw = run_git_raw(repo_root, args)?;
    Ok(String::from_utf8_lossy(&raw).trim().to_string())
}

/// Run git for a command whose answer is its exit status rather than its
/// output. None when git could not be started or was killed by a signal.
pub(super) fn run_git_code(repo_root: &Path, args: &[&str]) -> Option<i32> {
    run_git_output(repo_root, args).ok()?.status.code()
}

/// Canonicalize so the macOS /var to /private/var symlink does not make a
/// seatbelt realpath starts_with check reject legitimate paths. dunce keeps
/// Windows from returning a \\?\ path, which git cannot consume.
pub(super) fn canonicalize(p: &Path) -> Result<PathBuf, WorktreeError> {
    dunce::canonicalize(p).map_err(|e| WorktreeError::State {
        path: p.to_path_buf(),
        reason: format!("resolve the path: {e}"),
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

    // Fast resume: an existing tree whose HEAD resolves skips the add, after
    // the identity checks that prove it is this slug's worktree.
    if worktree_path.exists()
        && let Ok(head) = run_git(&worktree_path, &["rev-parse", "HEAD"])
    {
        resume_existing(&repo_root, slug)?;
        return Ok(WorktreeCreateResult {
            worktree_path,
            worktree_branch,
            head_commit: Some(head),
            existed: true,
        });
    }

    // New worktree. The -b flag refuses an existing branch (collision -> Err,
    // not clobber). The base is HEAD: the agent works off the current commit.
    // The base branch is read before the add, because the merge question a
    // removal asks later needs the branch this tree was forked from.
    let base_branch = current_branch(&repo_root);
    create_dir_all(worktrees_dir(&repo_root)).map_err(|e| WorktreeError::State {
        path: worktrees_dir(&repo_root),
        reason: format!("create the managed directory: {e}"),
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
    // Creation is one act: the directory and the record that makes it this
    // product's object. A record that cannot be written rolls the directory
    // back, because a tree left without one is a residue no removal will
    // accept.
    let record = ManagedWorktree::new(slug, base_branch);
    if let Err(error) = write_managed_worktree(&repo_root, &record) {
        return Err(match remove_fresh_worktree(&repo_root, slug) {
            Ok(()) => error,
            Err(rollback) => WorktreeError::State {
                path: worktree_path_for(&repo_root, slug),
                reason: format!(
                    "{error}; rolling the unrecorded worktree back also failed: {rollback}"
                ),
            },
        });
    }
    Ok(WorktreeCreateResult {
        worktree_path,
        worktree_branch,
        head_commit,
        existed: false,
    })
}

/// Prove that the directory already at a slug's managed path is that slug's
/// worktree before a session resumes in it, recording a tree none was
/// recorded for.
///
/// A resume that accepted a directory the removal preview refuses would put a
/// session to work in a tree nothing can remove, so the two ask the same
/// question. A record this version cannot read is refused rather than
/// overwritten: it is the only account of what the tree is. An unrecorded
/// tree is recorded with no base branch, because its fork point was never
/// seen, and naming the branch checked out today would make the merge
/// question about a branch the tree may not descend from.
fn resume_existing(repo_root: &Path, slug: &str) -> Result<(), WorktreeError> {
    let record = lookup_record(repo_root, slug);
    let expected_branch = match &record {
        RecordLookup::Read(record) => record.branch().to_string(),
        RecordLookup::Rejected { reason } => {
            return Err(WorktreeError::RecordRejected {
                slug: slug.to_string(),
                reason: reason.clone(),
            });
        }
        RecordLookup::Absent => worktree_branch_name(slug),
    };
    let identity = resolve_identity(repo_root, slug, &expected_branch)?;
    // A locked tree cannot be removed, so a session started in one would work
    // in a tree it can never leave cleanly.
    if identity.locked {
        return Err(WorktreeError::Locked {
            path: identity.path,
            reason: "Git records a lock on it".to_string(),
        });
    }
    if matches!(record, RecordLookup::Absent) {
        write_managed_worktree(repo_root, &ManagedWorktree::new(slug, None))?;
    }
    Ok(())
}

/// Count uncommitted files plus commits since the baseline commit inside the
/// worktree. Returns None when the state cannot be reliably determined (git
/// status or rev-list failed, or no baseline commit) — callers that use this
/// as a safety gate must treat None as unknown, assume unsafe (fail-closed).
/// A silent 0/0 would let a remove destroy real work.
///
/// The file count covers the classes that hold work a removal must not
/// discard: tracked edits, untracked paths, and ignored entries no known build
/// directory explains, since nothing can tell what those files are. A known
/// build directory stays out: it is regenerable output, and counting it would
/// mark every tree a build ever ran in as holding work.
pub fn count_worktree_changes(
    worktree_path: &Path,
    original_head_commit: Option<&str>,
) -> Option<ChangeSummary> {
    let changes = read_changes(worktree_path).ok()?;
    let changed_files = changes.tracked_dirty + changes.untracked + changes.ignored_unknown;
    let baseline = original_head_commit?;
    let rev_list = run_git(
        worktree_path,
        &["rev-list", "--count", &format!("{baseline}..HEAD")],
    )
    .ok()?;
    // A count that does not parse is not zero: this number is the gate's only
    // evidence of commits, so an unreadable answer is unknown, not clean.
    let commits = rev_list.trim().parse::<usize>().ok()?;
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

/// Undo a creation whose record could not be published.
///
/// The lease is not taken: the tree was made by this call, and the caller may
/// already hold the slug's lease, so taking it here would refuse the rollback
/// of its own tree. The state directory is left alone for the same reason:
/// deleting a lease file another process holds the lock on lets a third
/// process create a fresh one and lock it as well.
fn remove_fresh_worktree(repo_root: &Path, slug: &str) -> Result<(), WorktreeError> {
    let path = worktree_path_for(repo_root, slug);
    run_git(repo_root, &["worktree", "remove", &path.to_string_lossy()])?;
    run_git(repo_root, &["branch", "-d", &worktree_branch_name(slug)])?;
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
mod tests;
