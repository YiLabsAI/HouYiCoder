//! Snapshot store cleanup: expiry, size cap, and lease-protected removal.
//! A candidate is re-checked and lease-claimed under the store namespace
//! lock, then deleted while this process holds the lease, so a snapshot a
//! live session owns is never removed. Sizes are measured before the lock,
//! which keeps the critical section to name checks and lease probes.

use std::fs::metadata;
use std::fs::read_dir;
use std::fs::remove_dir_all;
use std::io::ErrorKind;
use std::path::Path;
use std::path::PathBuf;
use std::time::SystemTime;

use walkdir::WalkDir;

use super::lease;
use super::lease::LeaseProbe;
use super::lease::SnapshotLease;
use super::lease::StoreLock;
use super::lease::is_legacy_sentinel;
use super::lease::mark_legacy_sentinel;
use super::lease::remove_sidecar;
use super::staging::complete_marker;
use super::staging::is_complete;
use super::staging::sweep_orphan_sidecars;
use super::staging::sweep_orphan_staging;

/// Remove expired snapshots and enforce the total size cap. Snapshot paths
/// listed in protected, and snapshots whose lease cannot be claimed, are
/// kept. Returns the number of directories actually removed.
pub(in crate::snapshot) fn enforce_retention(
    store_root: &Path,
    ttl_secs: u64,
    max_size: u64,
    protected: &[PathBuf],
) -> usize {
    let now = SystemTime::now();
    let mut expired: Vec<(PathBuf, SystemTime)> = Vec::new();
    let mut sized: Vec<(PathBuf, SystemTime, Option<u64>)> = Vec::new();
    let entries = match read_dir(store_root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return 0,
        Err(error) => {
            tracing::warn!(
                "snapshot store {} cannot be listed: {error}",
                store_root.display()
            );
            return 0;
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let is_snapshot = name
            .strip_prefix("snap-")
            .and_then(|rest| rest.parse::<u64>().ok())
            .is_some();
        if !is_snapshot || !path.is_dir() {
            continue;
        }
        if protected
            .iter()
            .any(|kept| path.starts_with(kept) || kept.starts_with(&path))
        {
            continue;
        }
        let mtime = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .unwrap_or(now);
        let age = now.duration_since(mtime).unwrap_or_default();
        if age.as_secs() > ttl_secs {
            expired.push((path, mtime));
        } else {
            let size = dir_size(&path);
            if size.is_none() {
                tracing::debug!("snapshot {} size unknown; kept", path.display());
            }
            sized.push((path, mtime, size));
        }
    }
    let Ok(lock) = StoreLock::acquire(store_root) else {
        tracing::warn!("snapshot store lock unavailable; prune skipped this round");
        return 0;
    };
    let orphans = sweep_orphan_staging(store_root);
    sweep_orphan_sidecars(store_root);
    let mut removals: Vec<(PathBuf, SnapshotLease)> = Vec::new();
    for (path, seen) in expired {
        if let Some(lease) = claim_for_delete(&path, seen) {
            removals.push((path, lease));
        }
    }
    sized.sort_by_key(|(_, mtime, _)| *mtime);
    let mut total: u64 = sized.iter().filter_map(|(_, _, size)| *size).sum();
    for (path, seen, size) in sized {
        if total <= max_size {
            break;
        }
        // A snapshot whose size could not be measured is never selected, so
        // the cap can stay above its target but no unmeasured tree is deleted
        // on a guess.
        let Some(size) = size else {
            continue;
        };
        if let Some(lease) = claim_for_delete(&path, seen) {
            removals.push((path, lease));
            total -= size;
        }
    }
    drop(lock);
    for orphan in orphans {
        orphan.remove();
    }
    remove_claimed(store_root, removals)
}

/// Claim a snapshot for deletion. A snapshot another process holds, one
/// whose lease cannot be read, one that disappeared or was replaced while
/// the namespace lock was being acquired, and one whose marker is missing
/// beside a sentinel this process cannot tell apart from a failed publish are
/// all kept.
///
/// A directory with neither a marker nor a sentinel predates both. Its
/// sentinel is tagged before the claim, so a process that dies between the
/// claim and the removal leaves a state re-link accepts as unverified rather
/// than an empty sentinel it refuses.
pub(in crate::snapshot) fn claim_for_delete(
    path: &Path,
    seen: SystemTime,
) -> Option<SnapshotLease> {
    if !path.is_dir() {
        return None;
    }
    let current = metadata(path).and_then(|meta| meta.modified()).ok()?;
    if current != seen {
        tracing::debug!(
            "snapshot {} changed while prune waited; kept",
            path.display()
        );
        return None;
    }
    let name = path.file_name()?.to_str()?;
    let sentinel = lease::sentinel_path(path.parent()?, name);
    if !is_complete(path) {
        if sentinel.exists() {
            if !is_legacy_sentinel(&sentinel) {
                tracing::warn!(
                    "snapshot {} has a lease but no complete marker; kept",
                    path.display()
                );
                return None;
            }
        } else if let Err(error) = mark_legacy_sentinel(&sentinel) {
            tracing::warn!(
                "snapshot {} cannot be tagged as a pre-marker snapshot: {error}",
                path.display()
            );
            return None;
        }
    }
    match lease::claim(&sentinel) {
        LeaseProbe::Claimed(lease) => Some(lease),
        LeaseProbe::Busy => {
            tracing::debug!("snapshot {} is held; kept", path.display());
            None
        }
        LeaseProbe::Unreadable => {
            tracing::warn!("snapshot {} lease unreadable; kept", path.display());
            None
        }
    }
}

/// A snapshot directory this process deleted, with the sidecars and lease
/// that still have to go.
pub(in crate::snapshot) struct RemovedSnapshot {
    /// The directory that was deleted, kept to tell whether the name came
    /// back before the sidecars are removed.
    pub(in crate::snapshot) directory: PathBuf,
    pub(in crate::snapshot) sentinel: PathBuf,
    pub(in crate::snapshot) marker: PathBuf,
    pub(in crate::snapshot) lease: SnapshotLease,
}

/// Delete the claimed directories while their leases are held, then remove
/// their sentinels and markers under the namespace lock.
fn remove_claimed(store_root: &Path, removals: Vec<(PathBuf, SnapshotLease)>) -> usize {
    let mut removed = 0;
    let mut sidecars: Vec<RemovedSnapshot> = Vec::new();
    for (path, lease) in removals {
        match remove_dir_all(&path) {
            Ok(()) => {
                removed += 1;
                if let Some(name) = path.file_name().and_then(|name| name.to_str()) {
                    let sentinel = lease::sentinel_path(store_root, name);
                    let marker = complete_marker(store_root, name);
                    sidecars.push(RemovedSnapshot {
                        directory: path.clone(),
                        sentinel,
                        marker,
                        lease,
                    });
                }
            }
            Err(error) => {
                tracing::warn!("snapshot {} cannot be removed: {error}", path.display());
            }
        }
    }
    cleanup_sidecars(store_root, sidecars);
    removed
}

/// Remove the sidecars of deleted snapshots under the namespace lock. A
/// directory that came back under the same name belongs to a newer snapshot,
/// whose sentinel and marker must survive, so both unlinks are skipped when
/// it is present again.
pub(in crate::snapshot) fn cleanup_sidecars(
    store_root: &Path,
    sidecars: Vec<RemovedSnapshot>,
) -> usize {
    if sidecars.is_empty() {
        return 0;
    }
    let lock = match StoreLock::acquire(store_root) {
        Ok(lock) => lock,
        Err(error) => {
            tracing::warn!("snapshot lease cleanup skipped: {error}");
            return sidecars.len();
        }
    };
    let mut kept = 0;
    for held in sidecars {
        if held.directory.exists() {
            tracing::debug!(
                "snapshot {} was published again; its sidecars stay",
                held.directory.display()
            );
            kept += 1;
            drop(held.lease);
            continue;
        }
        // The unlink precedes the lease drop, which is safe here because the
        // namespace lock keeps every cooperating process from opening the
        // name between the two.
        remove_sidecar(&held.sentinel);
        remove_sidecar(&held.marker);
        drop(held.lease);
    }
    drop(lock);
    kept
}

/// Bytes a snapshot directory occupies, or nothing when any part of it
/// cannot be measured. Partial sizes are not reported as real ones: an
/// unreadable subtree would otherwise pass as zero and invite a deletion
/// decision on a figure that was never computed.
fn dir_size(path: &Path) -> Option<u64> {
    let mut total: u64 = 0;
    for entry in WalkDir::new(path).min_depth(1) {
        let entry = entry.ok()?;
        if entry.file_type().is_file() {
            total += entry.metadata().ok()?.len();
        }
    }
    Some(total)
}
