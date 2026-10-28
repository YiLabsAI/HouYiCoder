//! The record a removal leaves when Git detached its registration but the
//! directory survived, and the guarded directory removal that finishes it.
//!
//! A leftover is the directory such a removal left behind, and it is not an
//! ordinary directory to delete: once Git has forgotten the worktree, nothing
//! on disk still says the directory is this product's object. The record
//! carries the numbers the file system gave that directory, so a later attempt
//! deletes a directory only while those numbers still match.

use std::fs::Metadata;
use std::fs::read_dir;
use std::fs::read_to_string;
use std::fs::remove_dir;
use std::fs::remove_file;
use std::fs::symlink_metadata;
use std::io::ErrorKind;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
#[cfg(windows)]
use std::os::windows::fs::MetadataExt;
use std::path::Path;
use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;

use super::identify::TargetStamp;
use super::record::pending_dir_checked;
use super::record::pending_dir_ready;
use super::record::pending_path;
use super::record::write_state_file;
use super::session::WorktreeError;
use super::session::validate_worktree_slug;

/// The attribute Windows sets on an entry that points at something else, such
/// as a junction or a mount point. Such an entry presents as a directory while
/// its contents live somewhere else, so the walk must not enter it.
#[cfg(windows)]
const REPARSE_POINT: u32 = 0x400;

/// The schema of a cleanup record. A record from a later version is refused
/// rather than read with fields dropped, because the field this version would
/// drop could be the one naming the target.
const CLEANUP_SCHEMA: u32 = 1;

/// What a removal that stopped between its two halves leaves behind: the
/// registration is gone and the directory is not, and this record names the
/// one directory a later attempt may finish.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PendingCleanup {
    schema: u32,
    slug: String,
    /// The directory as it resolved when the removal found it still there.
    path: PathBuf,
    /// The numbers the directory had then, so a later attempt deletes only the
    /// directory this record was written for. None when there was no directory
    /// to read them from, which leaves nothing for a later attempt to prove.
    stamp: Option<TargetStamp>,
}

impl PendingCleanup {
    /// A record for the leftover a removal just found at a resolved path.
    pub(super) fn new(slug: &str, path: &Path, stamp: Option<TargetStamp>) -> Self {
        Self {
            schema: CLEANUP_SCHEMA,
            slug: slug.to_string(),
            path: path.to_path_buf(),
            stamp,
        }
    }

    /// The slug the leftover belongs to.
    pub(super) fn slug(&self) -> &str {
        &self.slug
    }

    /// The directory the record names.
    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    /// The numbers the directory had when the record was written.
    pub(super) fn stamp(&self) -> Option<TargetStamp> {
        self.stamp
    }
}

/// Store the record of a leftover, so a later attempt can finish it after a
/// crash, a reboot, or a directory a running process would not let go.
pub(super) fn write_pending(
    repo_root: &Path,
    pending: &PendingCleanup,
) -> Result<(), WorktreeError> {
    validate_worktree_slug(&pending.slug)?;
    // The directory is proven before the record is written: a link standing
    // where it belongs would otherwise move the record outside the state
    // directory, and a later removal would delete a file named there.
    pending_dir_ready(repo_root)?;
    let path = pending_path(repo_root, &pending.slug);
    let body = serde_json::to_string(pending).map_err(|e| WorktreeError::State {
        path: path.clone(),
        reason: format!("encode the cleanup record: {e}"),
    })?;
    write_state_file(&path, &body)
}

/// The leftover a slug's removal stopped on, when one is recorded.
///
/// A record that cannot be read, parsed, or is from another version answers as
/// an error rather than as absence: absence would let the removal fall through
/// to the ordinary path and report a failure that hides a leftover nobody can
/// then finish.
pub(super) fn lookup_pending(
    repo_root: &Path,
    slug: &str,
) -> Result<Option<PendingCleanup>, WorktreeError> {
    validate_worktree_slug(slug)?;
    if pending_dir_checked(repo_root)?.is_none() {
        return Ok(None);
    }
    let path = pending_path(repo_root, slug);
    let text = match read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(WorktreeError::State {
                path,
                reason: format!("read the cleanup record: {error}"),
            });
        }
    };
    let pending: PendingCleanup =
        serde_json::from_str(&text).map_err(|e| WorktreeError::State {
            path: path.clone(),
            reason: format!("parse the cleanup record: {e}"),
        })?;
    if pending.schema != CLEANUP_SCHEMA {
        return Err(WorktreeError::State {
            path,
            reason: format!(
                "the cleanup record was written at schema {}, and this version reads schema {CLEANUP_SCHEMA}",
                pending.schema
            ),
        });
    }
    Ok(Some(pending))
}

/// Drop a cleanup record that no longer describes anything, so a finished
/// removal does not leave one behind. A file that is already gone is not a
/// failure.
pub(super) fn clear_pending(repo_root: &Path, slug: &str) {
    match pending_dir_checked(repo_root) {
        Ok(Some(_)) => {}
        Ok(None) => return,
        Err(error) => {
            tracing::warn!(%error, "worktree cleanup record not removed");
            return;
        }
    }
    let path = pending_path(repo_root, slug);
    match remove_file(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => tracing::warn!(?path, %error, "worktree cleanup record not removed"),
    }
}

/// Remove a directory and everything under it, stopping at anything that could
/// reach past it.
///
/// The walk refuses rather than guesses: a directory on another file system is
/// a mount this call has no business crossing, and a path that stops being a
/// directory on the way is a replacement it cannot account for. A link is the
/// one entry that is removed rather than refused, and it is removed by its own
/// name: the file system drops the link, and whatever it names is untouched.
///
/// What it cannot see, it does not claim to: every check reads a path and acts
/// on that path again, so a directory replaced by a link between the check and
/// the listing is followed, and a mount sharing the root's file system reports
/// the same file system number as the root. Both need a walk that holds each
/// directory open and descends relative to that handle, which this call does
/// not do; the stamp and the lease bound who can reach the window at all.
///
/// The stamp the caller read before its own checks is compared here, at the
/// step that deletes, so a directory replaced in between is refused rather than
/// deleted.
pub(super) fn remove_tree(root: &Path, stamp: Option<TargetStamp>) -> Result<(), String> {
    let meta =
        symlink_metadata(root).map_err(|e| format!("{} could not be read: {e}", root.display()))?;
    if !is_plain_dir(&meta) {
        return Err(format!("{} is not a directory", root.display()));
    }
    if let Some(stamp) = stamp {
        stamp.verify(root)?;
    }
    let device = device_of(&meta);
    let mut pending: Vec<PathBuf> = vec![root.to_path_buf()];
    let mut directories: Vec<PathBuf> = Vec::new();
    while let Some(dir) = pending.pop() {
        let meta = symlink_metadata(&dir)
            .map_err(|e| format!("{} could not be read: {e}", dir.display()))?;
        if !is_plain_dir(&meta) {
            return Err(format!("{} is no longer a directory", dir.display()));
        }
        if device_of(&meta) != device {
            return Err(format!(
                "{} is on another file system, so it is not part of this directory",
                dir.display()
            ));
        }
        let entries =
            read_dir(&dir).map_err(|e| format!("{} could not be listed: {e}", dir.display()))?;
        for entry in entries {
            let entry = entry.map_err(|e| format!("{} could not be listed: {e}", dir.display()))?;
            let child = entry.path();
            let meta = symlink_metadata(&child)
                .map_err(|e| format!("{} could not be read: {e}", child.display()))?;
            if is_plain_dir(&meta) {
                pending.push(child);
            } else {
                remove_link(&child)?;
            }
        }
        directories.push(dir);
    }
    // A directory goes only once the entries it holds are gone, so the walk
    // records each one and removes them innermost first.
    for dir in directories.iter().rev() {
        remove_dir(dir).map_err(|e| format!("{} could not be removed: {e}", dir.display()))?;
    }
    Ok(())
}

/// Whether the walk may enter a directory. A link is not one, and neither is a
/// Windows reparse point: a junction presents as a directory while its contents
/// live somewhere else, so entering it would delete what it points at.
#[cfg(unix)]
fn is_plain_dir(meta: &Metadata) -> bool {
    meta.is_dir()
}

/// See the Unix form.
#[cfg(windows)]
fn is_plain_dir(meta: &Metadata) -> bool {
    meta.is_dir() && meta.file_attributes() & REPARSE_POINT == 0
}

/// See the Unix form.
#[cfg(not(any(unix, windows)))]
fn is_plain_dir(meta: &Metadata) -> bool {
    meta.is_dir()
}

/// The file system a directory sits on, so a mount inside the tree is
/// recognised rather than walked into. Only Unix exposes the number; elsewhere
/// the walk relies on the directory checks alone.
#[cfg(unix)]
fn device_of(meta: &Metadata) -> u64 {
    meta.dev()
}

/// See the Unix form.
#[cfg(not(unix))]
fn device_of(_meta: &Metadata) -> u64 {
    0
}

/// Remove one entry that is not a directory. A link is unlinked by its own
/// name, so the target is never touched; on Windows a directory link is
/// removed with the call that takes directories.
#[cfg(unix)]
fn remove_link(path: &Path) -> Result<(), String> {
    remove_file(path).map_err(|e| format!("{} could not be removed: {e}", path.display()))
}

/// See the Unix form.
#[cfg(not(unix))]
fn remove_link(path: &Path) -> Result<(), String> {
    remove_file(path)
        .or_else(|_| remove_dir(path))
        .map_err(|e| format!("{} could not be removed: {e}", path.display()))
}

#[cfg(test)]
mod tests;
