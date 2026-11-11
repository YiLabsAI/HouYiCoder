//! Private build directories for snapshots. A snapshot is copied into a
//! unique staging directory and becomes visible only when it is renamed to
//! its final name, with the complete marker written first. The build holds a
//! lease on the staging directory, so a concurrent prune leaves it alone; a
//! build that dies releases the lease with its process.

use std::env::var;
use std::fs::create_dir;
use std::fs::metadata;
use std::fs::read_dir;
use std::fs::remove_dir_all;
use std::fs::rename;
use std::fs::write;
use std::io::ErrorKind;
use std::path::Path;
use std::path::PathBuf;
use std::process::id;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;

use super::lease;
use super::lease::LeaseProbe;
use super::lease::SnapshotLease;
use super::lease::StoreLock;
use super::lease::remove_sidecar;

/// The prefix of a staging directory name.
const STAGING_PREFIX: &str = ".staging-";
/// The suffix of every lease sentinel file.
pub(in crate::snapshot) const LEASE_SUFFIX: &str = ".lease";
/// The suffix of the marker file that records a fully published snapshot.
const COMPLETE_SUFFIX: &str = ".complete";
/// How long an unclaimed staging tree survives before reclaim.
const STAGING_ORPHAN_TTL_SECS: u64 = 3600;
/// A two-process test names a directory here to stop a build at a fixed
/// point. Debug builds honor it; production never sets it.
const BUILD_BARRIER_ENV: &str = "HOUYICODER_SNAPSHOT_TEST_BARRIER";
/// A stopped barrier must not outlive the test that set it.
const BUILD_BARRIER_DEADLINE: Duration = Duration::from_secs(60);

/// Distinguishes two staging builds in one process.
static STAGING_SEQ: AtomicU64 = AtomicU64::new(0);

/// A staging tree under construction, with the lease that keeps prune away.
pub(in crate::snapshot) struct Staged {
    dir: PathBuf,
    lease_path: PathBuf,
    lease: SnapshotLease,
}

impl Staged {
    /// The directory the walk fills.
    pub(in crate::snapshot) fn path(&self) -> &Path {
        &self.dir
    }

    /// Remove a staging tree whose build failed. The tree is deleted while
    /// its lease is still held, so a sweep cannot claim the name under the
    /// removal; only the sentinel unlink needs the namespace lock.
    pub(in crate::snapshot) fn discard(self) {
        let Staged {
            dir,
            lease_path,
            lease,
        } = self;
        discard_staging(&dir, &lease_path, lease);
    }
}

/// The marker that records a fully published snapshot. The marker is a file
/// beside the snapshot directory rather than an entry inside it, because the
/// walk copies workspace content verbatim and a workspace may hold a file of
/// the same name. It is written under the namespace lock before the rename,
/// so a directory under a snapshot name from this version always has one.
pub(in crate::snapshot) fn complete_marker(store_root: &Path, snap_name: &str) -> PathBuf {
    store_root.join(format!(".{snap_name}{COMPLETE_SUFFIX}"))
}

/// Whether a snapshot directory carries its complete marker. A snapshot
/// without one is usually one that predates the marker; re-link and restore
/// still accept it, but it is never reported as verified, because a marker
/// lost after publication cannot be told apart from that.
pub(in crate::snapshot) fn is_complete(snap_path: &Path) -> bool {
    let Some(name) = snap_path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let Some(store_root) = snap_path.parent() else {
        return false;
    };
    complete_marker(store_root, name).is_file()
}

/// Create a private staging directory and claim its lease under the store
/// namespace lock, so a prune can never observe a half-created build.
pub(in crate::snapshot) fn start_build(store_root: &Path) -> std::io::Result<Staged> {
    let _lock = StoreLock::acquire(store_root)?;
    loop {
        let seq = STAGING_SEQ.fetch_add(1, Ordering::Relaxed);
        let name = format!("{STAGING_PREFIX}{}-{seq}", id());
        let lease_path = store_root.join(format!("{name}{LEASE_SUFFIX}"));
        let lease = match lease::claim_exclusive(&lease_path) {
            Ok(lease) => lease,
            // A sentinel left by a build that reused this name keeps it
            // busy; the next sequence number is free.
            Err(error) if error.kind() == ErrorKind::WouldBlock => continue,
            Err(error) => return Err(error),
        };
        let dir = store_root.join(&name);
        match create_dir(&dir) {
            Ok(()) => {
                return Ok(Staged {
                    dir,
                    lease_path,
                    lease,
                });
            }
            // A tree left by a dead build is reclaimed by the orphan sweep;
            // the next sequence number is free.
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                drop(lease);
                remove_sidecar(&lease_path);
            }
            Err(error) => {
                drop(lease);
                remove_sidecar(&lease_path);
                return Err(error);
            }
        }
    }
}

/// Write the complete marker, then publish the staged tree under its final
/// name. The namespace lock covers the id allocation, the marker write, the
/// lease claim, and the rename, so no cooperative publisher can fill the
/// name in between and a killed build never leaves a tree under a snapshot
/// name.
pub(in crate::snapshot) fn publish(
    staged: Staged,
    counter: &AtomicU64,
    store_root: &Path,
) -> std::io::Result<(PathBuf, SnapshotLease)> {
    let lock = match StoreLock::acquire(store_root) {
        Ok(lock) => lock,
        Err(error) => {
            staged.discard();
            return Err(error);
        }
    };
    loop {
        let id = counter.fetch_add(1, Ordering::Relaxed);
        let name = format!("snap-{id}");
        let final_dir = store_root.join(&name);
        if final_dir.exists() {
            continue;
        }
        let sentinel = lease::sentinel_path(store_root, &name);
        let snapshot = match lease::claim_exclusive(&sentinel) {
            Ok(snapshot) => snapshot,
            // A name whose lease is still held is being removed or was
            // abandoned by another process; take the next id.
            Err(error) if error.kind() == ErrorKind::WouldBlock => continue,
            // The whole staging tree is removed after the namespace lock is
            // released, so a failed publish does not stall every publisher
            // for the length of that removal.
            Err(error) => {
                drop(lock);
                staged.discard();
                return Err(error);
            }
        };
        let marker = complete_marker(store_root, &name);
        if let Err(error) = write(&marker, b"") {
            remove_sidecar(&marker);
            drop(snapshot);
            remove_sidecar(&sentinel);
            drop(lock);
            staged.discard();
            return Err(error);
        }
        pause_at_build_barrier("publishing");
        let Staged {
            dir,
            lease_path,
            lease,
        } = staged;
        if let Err(error) = rename(&dir, &final_dir) {
            remove_sidecar(&marker);
            drop(snapshot);
            remove_sidecar(&sentinel);
            drop(lock);
            discard_staging(&dir, &lease_path, lease);
            return Err(error);
        }
        drop(lease);
        remove_sidecar(&lease_path);
        drop(lock);
        return Ok((final_dir, snapshot));
    }
}

/// A staging tree whose build died, claimed so the caller can remove it
/// after releasing the namespace lock.
pub(in crate::snapshot) struct StagingOrphan {
    dir: PathBuf,
    lease_path: PathBuf,
    lease: SnapshotLease,
}

impl StagingOrphan {
    /// Remove the tree while its lease is held, then release the lease and
    /// unlink its sentinel under the namespace lock, the same order a failed
    /// build uses.
    pub(in crate::snapshot) fn remove(self) {
        let StagingOrphan {
            dir,
            lease_path,
            lease,
        } = self;
        discard_staging(&dir, &lease_path, lease);
    }
}

/// Delete a staging tree that will never be published, then release its
/// lease and unlink its sentinel under the namespace lock. The tree goes
/// first, while the lease still keeps a sweep off the name; the lock is
/// taken only for the unlink, so the removal of a whole workspace copy does
/// not stall the store.
fn discard_staging(dir: &Path, lease_path: &Path, lease: SnapshotLease) {
    remove_tree(dir);
    match lease_path.parent().map(StoreLock::acquire).transpose() {
        Ok(_lock) => {
            drop(lease);
            remove_sidecar(lease_path);
        }
        Err(error) => {
            drop(lease);
            tracing::debug!(
                "snapshot staging lease {} is left to the reclaim sweep: {error}",
                lease_path.display()
            );
        }
    }
}

/// Claim staging trees left by a process that died during a build. A tree is
/// claimed only after the safe period and only once its lease is free, so a
/// live build is never taken. The caller removes the claimed trees after it
/// releases the namespace lock, because deleting a whole workspace copy under
/// the lock would stall every publisher for the length of the removal.
pub(in crate::snapshot) fn sweep_orphan_staging(store_root: &Path) -> Vec<StagingOrphan> {
    let now = SystemTime::now();
    let mut orphans = Vec::new();
    let entries = match read_dir(store_root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return orphans,
        Err(error) => {
            tracing::warn!(
                "snapshot store {} cannot be listed: {error}",
                store_root.display()
            );
            return orphans;
        }
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with(STAGING_PREFIX) || name.ends_with(LEASE_SUFFIX) {
            continue;
        }
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let lease_path = store_root.join(format!("{name}{LEASE_SUFFIX}"));
        let age = age_secs(&lease_path, now)
            .or_else(|| age_secs(&path, now))
            .unwrap_or(0);
        if age <= STAGING_ORPHAN_TTL_SECS {
            continue;
        }
        if let LeaseProbe::Claimed(lease) = lease::claim(&lease_path) {
            orphans.push(StagingOrphan {
                dir: path,
                lease_path,
                lease,
            });
        }
    }
    orphans
}

/// Reclaim lease and marker files whose subject never appeared, which a
/// publish that died between claiming its name and renaming leaves behind.
/// A sidecar is unlinked only once its lease can be claimed and only after
/// the safe period. The unlink runs before the claimed lease is dropped,
/// which is safe because the caller holds the namespace lock throughout.
pub(in crate::snapshot) fn sweep_orphan_sidecars(store_root: &Path) {
    let now = SystemTime::now();
    let entries = match read_dir(store_root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return,
        Err(error) => {
            tracing::warn!(
                "snapshot store {} cannot be listed: {error}",
                store_root.display()
            );
            return;
        }
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(names) = sidecar_name(&name) else {
            continue;
        };
        if store_root.join(names.subject).exists() {
            continue;
        }
        let path = entry.path();
        if age_secs(&path, now).is_none_or(|age| age <= STAGING_ORPHAN_TTL_SECS) {
            continue;
        }
        let lease_path = store_root.join(format!("{}{LEASE_SUFFIX}", names.stem));
        let LeaseProbe::Claimed(_lease) = lease::claim(&lease_path) else {
            continue;
        };
        remove_sidecar(&path);
        if !name.ends_with(LEASE_SUFFIX) {
            remove_sidecar(&lease_path);
        }
    }
}

/// The two names a sidecar file yields: the stem of its own name, used to
/// address its lease, and the directory name it guards.
struct SidecarName<'a> {
    stem: &'a str,
    subject: &'a str,
}

/// Split a sidecar file name, or report that the file is not a sidecar. A
/// staging tree keeps the leading dot of its name; a snapshot directory
/// drops the dot its sentinel carries.
fn sidecar_name(name: &str) -> Option<SidecarName<'_>> {
    let stem = name
        .strip_suffix(LEASE_SUFFIX)
        .or_else(|| name.strip_suffix(COMPLETE_SUFFIX))?;
    if stem.starts_with(STAGING_PREFIX) {
        return Some(SidecarName {
            stem,
            subject: stem,
        });
    }
    let subject = stem.strip_prefix('.')?;
    Some(SidecarName { stem, subject })
}

/// Test hook: stop the build at a fixed point while a two-process test
/// prunes and re-links around it. Debug builds honor the directory named by
/// the barrier variable; production never sets it.
pub(in crate::snapshot) fn pause_at_build_barrier(stage: &str) {
    if !cfg!(debug_assertions) {
        return;
    }
    let Ok(dir) = var(BUILD_BARRIER_ENV) else {
        return;
    };
    let dir = PathBuf::from(dir);
    if write(dir.join(stage), b"").is_err() {
        return;
    }
    let release = dir.join(format!("{stage}.release"));
    let deadline = Instant::now() + BUILD_BARRIER_DEADLINE;
    while !release.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(1));
    }
}

/// Remove a staging tree. A tree that cannot be removed is left for the
/// orphan sweep.
fn remove_tree(dir: &Path) {
    if let Err(error) = remove_dir_all(dir) {
        tracing::debug!("snapshot staging {} remains: {error}", dir.display());
    }
}

fn age_secs(path: &Path, now: SystemTime) -> Option<u64> {
    let modified = metadata(path).and_then(|meta| meta.modified()).ok()?;
    Some(
        now.duration_since(modified)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    )
}
