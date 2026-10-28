//! What directory a managed worktree path actually is: the checks that decide
//! whether the thing on disk is the worktree this product created, read from
//! the directory itself rather than from the slug that named it.
//!
//! Resuming a tree and previewing its removal ask the same question, and must
//! answer it the same way: a resume that accepted a directory the preview
//! refuses would hand a session a tree nothing can remove.

use std::fs::Metadata;
use std::fs::metadata;
use std::fs::symlink_metadata;
use std::io::ErrorKind;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;

use super::session::WorktreeError;
use super::session::canonicalize;
use super::session::flatten_slug;
use super::session::run_git;
use super::session::run_git_raw;
use super::session::worktree_path_for;
use super::session::worktrees_dir;

/// The identity a removal or a resume acts on, read from the directory itself
/// rather than from the slug that named it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorktreeIdentity {
    /// The directory, resolved through every link on its path.
    pub path: PathBuf,
    /// The Git directory the worktree shares with the repository it belongs
    /// to.
    pub common_dir: PathBuf,
    /// The branch the worktree is checked out on.
    pub branch: String,
    /// The commit the worktree has checked out.
    pub head: String,
    /// Whether Git records the worktree as locked, which refuses removal until
    /// the lock is cleared or a second force is given.
    pub locked: bool,
}

/// The directory a slug names, checked against the managed root, the
/// repository's Git common directory, the branch the caller expects there, and
/// Git's own worktree record.
///
/// The expected branch is the branch the caller's account of the tree names:
/// the creation record when one is readable, and the branch the slug derives
/// when the tree has no record yet. A directory failing any check is refused
/// rather than described, because a removal or a resume acting on it would act
/// on something other than the object the caller named.
pub(super) fn resolve_identity(
    repo_root: &Path,
    slug: &str,
    expected_branch: &str,
) -> Result<WorktreeIdentity, WorktreeError> {
    let repo_root = canonicalize(repo_root)?;
    let expected = worktree_path_for(&repo_root, slug);
    let root = canonicalize(&worktrees_dir(&repo_root))?;
    // The managed root is anchored to the repository, not only to itself: a
    // link left where the directory belongs would otherwise move the root, and
    // every tree made under it, outside the repository while every check still
    // agreed the tree sat under its own root.
    if !root.starts_with(&repo_root) {
        return Err(WorktreeError::Identity {
            path: root,
            reason: format!(
                "the managed directory resolves outside {}",
                repo_root.display()
            ),
        });
    }
    let path = dunce::canonicalize(&expected).map_err(|e| WorktreeError::Identity {
        path: expected.clone(),
        reason: format!("it could not be resolved: {e}"),
    })?;
    let named = flatten_slug(slug);
    let named_on_disk = path.file_name().and_then(|name| name.to_str());
    if path.parent() != Some(root.as_path()) || named_on_disk != Some(named.as_str()) {
        return Err(WorktreeError::Identity {
            path: path.clone(),
            reason: format!("it resolves outside {}", root.display()),
        });
    }
    if !metadata(&path).is_ok_and(|meta| meta.is_dir()) {
        return Err(WorktreeError::Identity {
            path,
            reason: "the managed path is not a directory".to_string(),
        });
    }
    let not_a_worktree = |error: WorktreeError| WorktreeError::Identity {
        path: path.clone(),
        reason: format!("it is not a Git worktree: {error}"),
    };
    let head = run_git(&path, &["rev-parse", "HEAD"]).map_err(not_a_worktree)?;
    let branch = run_git(&path, &["rev-parse", "--abbrev-ref", "HEAD"]).map_err(not_a_worktree)?;
    if branch != expected_branch {
        return Err(WorktreeError::Identity {
            path,
            reason: format!("it is checked out on {branch}, not {expected_branch}"),
        });
    }
    let common_dir = common_dir_of(&path)?;
    if common_dir != common_dir_of(&repo_root)? {
        return Err(WorktreeError::Identity {
            path,
            reason: format!("it belongs to the repository at {}", common_dir.display()),
        });
    }
    let locked = listed_worktree(&repo_root, &path)?.locked;
    Ok(WorktreeIdentity {
        path,
        common_dir,
        branch,
        head,
        locked,
    })
}

/// The Git common directory of the repository a path belongs to, resolved to
/// an absolute path. Git prints it relative to the directory it was asked from
/// when that directory is the repository root.
pub(super) fn common_dir_of(path: &Path) -> Result<PathBuf, WorktreeError> {
    let printed = run_git(path, &["rev-parse", "--git-common-dir"])?;
    let base = Path::new(&printed);
    let joined = if base.is_absolute() {
        base.to_path_buf()
    } else {
        path.join(base)
    };
    canonicalize(&joined)
}

/// One entry of Git's own worktree listing.
struct ListedEntry {
    path: PathBuf,
    locked: bool,
}

/// What Git's own worktree record says about a directory.
struct ListedWorktree {
    locked: bool,
}

/// Ask Git's own worktree record about a directory. A directory the record
/// does not name is not the worktree this repository registered, whatever it
/// presents on disk, so it is refused rather than described.
///
/// The listing is read as NUL-separated records so a path holding a newline
/// stays one field. A Git too old to know the flag fails the call, which
/// refuses the caller rather than reading the listing as empty and refusing
/// every registered tree for the wrong reason.
fn listed_worktree(repo_root: &Path, path: &Path) -> Result<ListedWorktree, WorktreeError> {
    match listed_entry(repo_root, path)? {
        Some(entry) => Ok(ListedWorktree {
            locked: entry.locked,
        }),
        None => Err(WorktreeError::Identity {
            path: path.to_path_buf(),
            reason: "Git does not list it as a worktree of this repository".to_string(),
        }),
    }
}

/// Whether Git's own worktree record names a directory. A path that does not
/// resolve is not listed: there is nothing for the record to name.
///
/// A removal asks this to tell a tree Git still registered, which it takes
/// apart the ordinary way, from a leftover Git has already detached, which only
/// the record of that leftover may finish.
pub(super) fn listed_in_git(repo_root: &Path, path: &Path) -> Result<bool, WorktreeError> {
    let Ok(resolved) = canonicalize(path) else {
        return Ok(false);
    };
    Ok(listed_entry(repo_root, &resolved)?.is_some())
}

/// Git's entry for a resolved directory, when its listing carries one. An
/// entry whose own path does not resolve names nothing this caller can compare,
/// so it is skipped rather than treated as a match or as a failure.
fn listed_entry(repo_root: &Path, resolved: &Path) -> Result<Option<ListedEntry>, WorktreeError> {
    for entry in worktree_listing(repo_root)? {
        if canonicalize(&entry.path).is_ok_and(|listed| listed == resolved) {
            return Ok(Some(entry));
        }
    }
    Ok(None)
}

/// Git's own worktree listing, read as NUL-separated records.
fn worktree_listing(repo_root: &Path) -> Result<Vec<ListedEntry>, WorktreeError> {
    let raw = run_git_raw(repo_root, &["worktree", "list", "--porcelain", "-z"])?;
    Ok(parse_worktree_listing(&raw))
}

/// Split a NUL-separated worktree listing into its entries. Each entry starts
/// with its path and may carry a lock line; the other lines describe the
/// commit it holds and the branch it is on, which the identity check already
/// reads from the directory itself.
fn parse_worktree_listing(raw: &[u8]) -> Vec<ListedEntry> {
    let mut entries: Vec<ListedEntry> = Vec::new();
    for field in raw.split(|byte| *byte == 0) {
        let text = String::from_utf8_lossy(field);
        if let Some(path) = text.strip_prefix("worktree ") {
            entries.push(ListedEntry {
                path: PathBuf::from(path),
                locked: false,
            });
        } else if (text == "locked" || text.starts_with("locked "))
            && let Some(entry) = entries.last_mut()
        {
            entry.locked = true;
        }
    }
    entries
}

/// The directory a caller verified, so a path that now names a different
/// directory is refused rather than deleted.
///
/// Unix exposes the numbers its file systems name a directory by, and they are
/// carried into the record of a removal that stopped so a later attempt deletes
/// only the directory that removal checked. Windows exposes them only through an
/// unstable part of the standard library, so the stamp compares nothing there
/// and a check passes; on that platform the record and identity checks around
/// it are the ones that stand.
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub(super) struct TargetStamp {
    device: u64,
    inode: u64,
}

impl TargetStamp {
    /// The numbers the file system gives a directory, or None when there is no
    /// directory at the path to take them from.
    pub(super) fn read(path: &Path) -> Result<Option<Self>, WorktreeError> {
        match symlink_metadata(path) {
            Ok(meta) => {
                let (device, inode) = stamp_of(&meta);
                Ok(Some(Self { device, inode }))
            }
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
            Err(error) => Err(WorktreeError::State {
                path: path.to_path_buf(),
                reason: format!("read the directory: {error}"),
            }),
        }
    }

    /// Answer whether the path still names the directory these numbers came
    /// from, or the reason it no longer does. The caller decides what a
    /// mismatch means, because what it refuses depends on how far the removal
    /// had already gone.
    pub(super) fn verify(self, path: &Path) -> Result<(), String> {
        let now = Self::read(path).map_err(|error| error.to_string())?;
        match now {
            Some(now) if now.device == self.device && now.inode == self.inode => Ok(()),
            _ => Err("it was replaced or removed after it was checked".to_string()),
        }
    }
}

/// The numbers that name a directory on its file system. Zero on a platform
/// that does not expose them, which makes every directory compare equal.
#[cfg(unix)]
fn stamp_of(meta: &Metadata) -> (u64, u64) {
    (meta.dev(), meta.ino())
}

/// See the Unix form.
#[cfg(not(unix))]
fn stamp_of(_meta: &Metadata) -> (u64, u64) {
    (0, 0)
}

#[cfg(test)]
mod tests;
