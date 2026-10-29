//! The record a managed worktree leaves when it is created, and the lease a
//! live session holds while it works in one.
//!
//! A directory being a Git worktree says nothing about who made it: a user's
//! own worktree, another tool's, and a leftover copy all look the same to Git.
//! The record is what makes removal an act on this product's own object, and
//! the lease is what keeps a tree another session is using out of reach.

use std::fs::File;
use std::fs::OpenOptions;
use std::fs::TryLockError;
use std::fs::create_dir_all;
use std::fs::read_to_string;
use std::fs::remove_file;
use std::io::ErrorKind;
use std::path::Path;
use std::path::PathBuf;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use houyicoder_context::write_atomic;
use serde::Deserialize;
use serde::Serialize;

use super::identify::WorktreeTarget;
use super::session::WorktreeError;
use super::session::canonicalize;
use super::session::flatten_slug;
use super::session::validate_worktree_slug;
use super::session::worktree_branch_name;

/// The schema of a creation record. Any change to the record's shape bumps
/// this number. A reader accepts exactly the fields it names, so a record a
/// later version wrote with fields this one does not know is rejected rather
/// than read with those fields dropped: the worktree then answers as
/// refused, instead of being removed under rules its record was not written
/// for. A version that bumps the number decides in the same change how it
/// reads a schema 1 record, and until it does, refusing that record is the
/// answer the code gives.
const RECORD_SCHEMA: u32 = 1;

/// What a managed worktree was created as. The base branch is the ref the
/// merge question is asked against later. The record carries no commit: the
/// head a removal acts on is read from the directory, and a tree adopted after
/// this product's own creation has no earlier head to record.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedWorktree {
    schema: u32,
    slug: String,
    branch: String,
    base_branch: Option<String>,
    created_ms: u64,
}

impl ManagedWorktree {
    /// A record for the worktree at a slug, forked from the base branch the
    /// caller saw the tree start on. An adopted tree passes None: its fork
    /// point was never seen, and naming the branch checked out today would
    /// make the merge question about a branch the tree may not descend from.
    pub fn new(slug: &str, base_branch: Option<String>) -> Self {
        Self {
            schema: RECORD_SCHEMA,
            slug: slug.to_string(),
            branch: worktree_branch_name(slug),
            base_branch,
            created_ms: now_ms(),
        }
    }

    /// The slug this worktree was created under.
    pub fn slug(&self) -> &str {
        &self.slug
    }

    /// The branch the worktree checks out.
    pub fn branch(&self) -> &str {
        &self.branch
    }

    /// The branch the worktree was forked from, when it was known when the
    /// record was written.
    pub fn base_branch(&self) -> Option<&str> {
        self.base_branch.as_deref()
    }

    /// When this record was written, in unix milliseconds. For an adopted
    /// tree that is when it was adopted, not when it was created.
    pub fn created_ms(&self) -> u64 {
        self.created_ms
    }
}

/// The directory holding one creation record and one lease per managed
/// worktree.
///
/// It sits beside the worktrees rather than inside their directory: a reader
/// listing worktrees then sees worktrees and nothing else, and a record can
/// outlive the directory it describes, which is what a removal that has to be
/// finished later needs.
pub(super) fn state_dir(repo_root: &Path) -> PathBuf {
    repo_root.join(".houyicoder").join("worktree-state")
}

/// The record path for a slug.
pub(super) fn record_path(repo_root: &Path, slug: &str) -> PathBuf {
    state_dir(repo_root).join(format!("{}.json", flatten_slug(slug)))
}

/// The lease path for a state key. The key is a slug for a tree this product
/// created and a digest of the resolved directory for any other, which is why
/// it is taken as the name it already is rather than flattened again: a
/// digest holds no separator, so the two forms share one directory without
/// one overwriting the other.
pub(super) fn lease_path(repo_root: &Path, key: &str) -> PathBuf {
    state_dir(repo_root).join(format!("{key}.lease"))
}

/// The directory holding the record of a removal that stopped with its
/// directory still there.
///
/// It is a directory of its own rather than a second suffix in the state
/// directory, because slugs may contain dots: a pending record named
/// slug.cleanup.json would be the creation record of the worktree named
/// slug.cleanup, and removing one would delete the other's file.
pub(super) fn pending_dir(repo_root: &Path) -> PathBuf {
    state_dir(repo_root).join("pending")
}

/// The pending record path for a slug.
pub(super) fn pending_path(repo_root: &Path, slug: &str) -> PathBuf {
    pending_dir(repo_root).join(format!("{}.json", flatten_slug(slug)))
}

/// The state directory, created when it is missing, once it is proven to
/// resolve inside the repository it belongs to. A link standing where the
/// directory should be would move every record this product writes, and every
/// lease file it deletes, outside the tree it was asked to act on.
fn state_dir_ready(repo_root: &Path) -> Result<PathBuf, WorktreeError> {
    let repo = canonicalize(repo_root)?;
    let dir = state_dir(&repo);
    create_dir_all(&dir).map_err(|e| WorktreeError::State {
        path: dir.clone(),
        reason: format!("create the state directory: {e}"),
    })?;
    state_dir_checked(&repo)
}

/// The pending record directory, when it resolves inside the worktree state
/// directory. A directory that is not there yet answers as absent: a reader has
/// nothing to read, and a writer creates it through the creating call.
pub(super) fn pending_dir_checked(repo_root: &Path) -> Result<Option<PathBuf>, WorktreeError> {
    let state = match dunce::canonicalize(state_dir(repo_root)) {
        Ok(state) => state,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(WorktreeError::State {
                path: state_dir(repo_root),
                reason: format!("resolve the worktree state directory: {error}"),
            });
        }
    };
    let dir = pending_dir(repo_root);
    match dunce::canonicalize(&dir) {
        Ok(resolved) if resolved.starts_with(&state) => Ok(Some(dir)),
        Ok(resolved) => Err(WorktreeError::State {
            path: resolved,
            reason: format!(
                "the cleanup record directory resolves outside {}",
                state.display()
            ),
        }),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(WorktreeError::State {
            path: dir,
            reason: format!("resolve the cleanup record directory: {error}"),
        }),
    }
}

/// The pending record directory, created when it is missing, once it is proven
/// to resolve inside the worktree state directory.
pub(super) fn pending_dir_ready(repo_root: &Path) -> Result<PathBuf, WorktreeError> {
    state_dir_ready(repo_root)?;
    let dir = pending_dir(repo_root);
    create_dir_all(&dir).map_err(|e| WorktreeError::State {
        path: dir.clone(),
        reason: format!("create the cleanup record directory: {e}"),
    })?;
    pending_dir_checked(repo_root)?.ok_or_else(|| WorktreeError::State {
        path: dir,
        reason: "the cleanup record directory disappeared after it was created".to_string(),
    })
}

/// The state directory, when it resolves inside the repository. A directory
/// that is not there yet counts as inside: there is nothing to read, and a
/// writer creates it and asks again through the creating call.
fn state_dir_checked(repo_root: &Path) -> Result<PathBuf, WorktreeError> {
    let repo = canonicalize(repo_root)?;
    let dir = state_dir(&repo);
    match dunce::canonicalize(&dir) {
        Ok(resolved) if !resolved.starts_with(&repo) => Err(WorktreeError::State {
            path: resolved,
            reason: format!(
                "the worktree state directory resolves outside {}",
                repo.display()
            ),
        }),
        Ok(_) => Ok(dir),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(dir),
        Err(error) => Err(WorktreeError::State {
            path: dir,
            reason: format!("resolve the worktree state directory: {error}"),
        }),
    }
}

/// Write the creation record for a worktree.
///
/// The record is published by rename, so a reader never sees a half-written
/// one: a crash leaves the temporary file behind and the previous record, if
/// any, intact. Each write names its own temporary file, so two writers in
/// one directory cannot truncate each other's bytes. The bytes reach the disk
/// before the rename, and on Unix the directory entry is synced after it, so
/// a record a resumed removal relies on survives a crash rather than
/// disappearing with the page cache.
pub fn write_managed_worktree(
    repo_root: &Path,
    record: &ManagedWorktree,
) -> Result<(), WorktreeError> {
    // The record names the worktree, so its slug is validated here rather than
    // trusted from the caller: a name this product cannot derive a path from
    // must not become a file in the state directory.
    validate_worktree_slug(&record.slug)?;
    state_dir_ready(repo_root)?;
    let path = record_path(repo_root, &record.slug);
    let body = serde_json::to_string(record).map_err(|e| WorktreeError::State {
        path: path.clone(),
        reason: format!("encode the record: {e}"),
    })?;
    write_state_file(&path, &body)
}

/// Write one file of the worktree state by rename, so a reader never sees a
/// half-written one: a crash leaves the temporary file behind and the previous
/// contents, if any, intact.
pub(super) fn write_state_file(path: &Path, body: &str) -> Result<(), WorktreeError> {
    match write_atomic(path, body.as_bytes()) {
        Ok(()) => Ok(()),
        // The file is on disk; only its survival across a crash is weaker.
        // Reporting it as a failed write would have the caller roll back a
        // tree whose record already exists.
        Err(error) if error.published() => {
            tracing::warn!(path = %path.display(), %error, "state file not flushed");
            Ok(())
        }
        Err(error) => Err(WorktreeError::State {
            path: error.path().to_path_buf(),
            reason: error.to_string(),
        }),
    }
}

/// What the record store holds for a slug.
///
/// The three answers are distinct because a caller that cannot tell an absent
/// record from one it rejected would write a new record over evidence it never
/// read.
#[derive(Clone, Debug)]
pub enum RecordLookup {
    /// No record file exists for the slug.
    Absent,
    /// A record file exists that this version does not accept: unreadable,
    /// unparsable, at another schema, or naming another slug. The reason is
    /// carried so a refusal can say which of them it is.
    Rejected { reason: String },
    /// A record this version reads, naming the slug asked about.
    Read(ManagedWorktree),
}

/// What the record store holds for a slug, so a caller can tell a worktree
/// nothing has recorded from one whose record exists but was refused.
///
/// A slug outside the allowlist answers Rejected: no record path this product
/// would have written can describe it, and a caller must not act on it. A
/// record naming another worktree, or a branch this product does not derive
/// from the slug, is Rejected too: the identity check would refuse the
/// directory later, and one answer is better than two places to disagree.
pub fn lookup_record(repo_root: &Path, slug: &str) -> RecordLookup {
    if validate_worktree_slug(slug).is_err() {
        return RecordLookup::Rejected {
            reason: "the slug is not a worktree name this product writes".to_string(),
        };
    }
    let path = record_path(repo_root, slug);
    if let Err(error) = state_dir_checked(repo_root) {
        return rejected(&state_dir(repo_root), error.to_string());
    }
    match read_record_file(&path) {
        RecordLookup::Read(record) if record.slug != slug => {
            rejected(&path, format!("it names worktree {}", record.slug))
        }
        RecordLookup::Read(record) if record.branch() != worktree_branch_name(slug) => rejected(
            &path,
            format!(
                "it names branch {}, where this product derives {} for that worktree",
                record.branch(),
                worktree_branch_name(slug)
            ),
        ),
        other => other,
    }
}

/// The creation record for a slug, for tests that only need the record.
///
/// A caller deciding what to do with a directory must read RecordLookup
/// instead: this folds an absent record into the same None as a refused one,
/// which is the distinction a removal depends on.
#[cfg(test)]
pub fn read_managed_worktree(repo_root: &Path, slug: &str) -> Option<ManagedWorktree> {
    match lookup_record(repo_root, slug) {
        RecordLookup::Read(record) => Some(record),
        RecordLookup::Absent | RecordLookup::Rejected { .. } => None,
    }
}

/// Delete the record, lease and cleanup record a slug leaves behind, reporting
/// rather than returning a failure. The worktree is already gone by the time
/// this runs, so a record that survives names a tree nothing can resolve;
/// leaving it would grow the state directory with one dead entry per removed
/// worktree.
///
/// The caller holds the slug's lease, but the hold does not outlast the unlink:
/// a process that creates the file again locks a new one. What makes this safe
/// is that the removal has already finished, so a later holder finds no tree
/// and no record to act on.
pub(super) fn clear_managed_state(repo_root: &Path, slug: &str) {
    if let Err(error) = state_dir_checked(repo_root) {
        tracing::warn!("worktree state not cleared for {slug}: {error}");
        return;
    }
    let mut paths = vec![
        record_path(repo_root, slug),
        lease_path(repo_root, &flatten_slug(slug)),
    ];
    // The cleanup record lives one directory deeper, and its directory is
    // proven inside the state directory before a file named there is deleted.
    match pending_dir_checked(repo_root) {
        Ok(Some(_)) => paths.push(pending_path(repo_root, slug)),
        Ok(None) => {}
        Err(error) => tracing::warn!("worktree cleanup record not cleared for {slug}: {error}"),
    }
    for path in paths {
        match remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => tracing::warn!(?path, %error, "worktree state not removed"),
        }
    }
}

/// Read the record file at a path. A file that does not read, does not parse,
/// or names a schema this version does not know answers Rejected: a record
/// from another version is not trusted to share its meaning.
///
/// A refused record is logged with its path and the underlying error, while
/// the caller receives a sentence it can show: the raw input/output or parser
/// text tells an operator what happened but not a user what to do. Answering
/// Rejected leaves the worktree untouched, and without the line an operator
/// would see only a refusal with no way to tell a lost record from a damaged
/// one.
fn read_record_file(path: &Path) -> RecordLookup {
    let text = match read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == ErrorKind::NotFound => return RecordLookup::Absent,
        Err(error) => {
            tracing::warn!(?path, %error, "worktree record could not be read");
            return RecordLookup::Rejected {
                reason: "it could not be read".to_string(),
            };
        }
    };
    let record: ManagedWorktree = match serde_json::from_str(&text) {
        Ok(record) => record,
        Err(error) => {
            tracing::warn!(?path, %error, "worktree record could not be parsed");
            return RecordLookup::Rejected {
                reason: "it could not be parsed".to_string(),
            };
        }
    };
    if record.schema != RECORD_SCHEMA {
        return rejected(
            path,
            format!(
                "it was written at schema {}, and this version reads schema {RECORD_SCHEMA}",
                record.schema
            ),
        );
    }
    RecordLookup::Read(record)
}

/// A refused record, logged with its path so the refusal can be traced to the
/// file that caused it.
fn rejected(path: &Path, reason: String) -> RecordLookup {
    tracing::warn!(?path, %reason, "worktree record rejected");
    RecordLookup::Rejected { reason }
}

/// An exclusive hold on a managed worktree's active-use lease, released when
/// the guard drops.
///
/// The lease is a file lock, not a marker: a process that exits, crashes, or
/// is killed releases it with its last descriptor, so the next holder never
/// has to clean up after one that died. Its bytes carry nothing.
#[derive(Debug)]
pub struct WorktreeLease {
    file: File,
    path: PathBuf,
}

impl WorktreeLease {
    /// Take the lease for a slug, creating the lease file when absent. A slug
    /// outside the allowlist is refused before a path is derived from it.
    pub fn acquire(repo_root: &Path, slug: &str) -> Result<Self, WorktreeError> {
        validate_worktree_slug(slug)?;
        Self::acquire_key(repo_root, &flatten_slug(slug), slug)
    }

    /// Take the lease for a resolved worktree, whatever created it. A linked
    /// tree from elsewhere has no slug, so its state key is what the lease
    /// file is named after; the name it was listed under is what a refusal
    /// shows the user.
    pub(super) fn acquire_for(
        repo_root: &Path,
        target: &WorktreeTarget,
    ) -> Result<Self, WorktreeError> {
        Self::acquire_key(repo_root, &target.state_key(), &target.name())
    }

    /// Take the lease for a state key, creating its file when absent.
    fn acquire_key(repo_root: &Path, key: &str, name: &str) -> Result<Self, WorktreeError> {
        state_dir_ready(repo_root)?;
        let path = lease_path(repo_root, key);
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .map_err(|e| WorktreeError::LeaseUnknown {
                worktree: name.to_string(),
                reason: format!("open {}: {e}", path.display()),
            })?;
        file.try_lock().map_err(|e| match e {
            // Another session holds the lease.
            TryLockError::WouldBlock => WorktreeError::Leased {
                worktree: name.to_string(),
                reason: "another session holds it".to_string(),
            },
            TryLockError::Error(e) => WorktreeError::LeaseUnknown {
                worktree: name.to_string(),
                reason: format!("lock {}: {e}", path.display()),
            },
        })?;
        Ok(Self { file, path })
    }

    /// Give up the hold and remove the lease file, for an act that stopped
    /// without using what it guarded, or one that finished and has nothing
    /// left to keep others out of. No other process can hold the file this
    /// removes, because this guard is still holding it; the lock is released
    /// after the file is gone.
    pub(super) fn discard(self) {
        match remove_file(&self.path) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => {
                tracing::warn!(path = ?self.path, %error, "worktree lease not removed");
            }
        }
    }
}

impl Drop for WorktreeLease {
    fn drop(&mut self) {
        drop(self.file.unlock());
    }
}

/// Whether a live process holds this worktree's lease. A lease file that
/// cannot be opened or locked for a reason other than another holder is an
/// error, not a yes: the caller cannot prove the tree idle, so it must refuse
/// to remove it and say why rather than report a session it never saw.
pub fn worktree_leased(repo_root: &Path, slug: &str) -> Result<bool, WorktreeError> {
    validate_worktree_slug(slug)?;
    leased_key(repo_root, &flatten_slug(slug), slug)
}

/// Whether a live process holds the lease of a resolved worktree, whoever
/// created it. A tree this product never leased reads as not leased, which is
/// what a pane showing a tree from elsewhere needs to know.
pub(super) fn leased_for(repo_root: &Path, target: &WorktreeTarget) -> Result<bool, WorktreeError> {
    leased_key(repo_root, &target.state_key(), &target.name())
}

/// Whether a live process holds the lease under a state key.
fn leased_key(repo_root: &Path, key: &str, name: &str) -> Result<bool, WorktreeError> {
    state_dir_checked(repo_root)?;
    let path = lease_path(repo_root, key);
    let file = match OpenOptions::new().write(true).open(&path) {
        Ok(file) => file,
        // No lease file means the name was never leased, or a removal cleared
        // it. A file that is there may hold no lock: releasing a lease leaves
        // the file behind, so the lock, not the file, is what answers.
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(false),
        Err(e) => {
            return Err(WorktreeError::LeaseUnknown {
                worktree: name.to_string(),
                reason: format!("open {}: {e}", path.display()),
            });
        }
    };
    match file.try_lock() {
        Ok(()) => {
            drop(file.unlock());
            Ok(false)
        }
        Err(TryLockError::WouldBlock) => Ok(true),
        Err(TryLockError::Error(e)) => Err(WorktreeError::LeaseUnknown {
            worktree: name.to_string(),
            reason: format!("lock {}: {e}", path.display()),
        }),
    }
}

/// Milliseconds since the unix epoch, or zero when the clock reads earlier
/// than it.
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests;
