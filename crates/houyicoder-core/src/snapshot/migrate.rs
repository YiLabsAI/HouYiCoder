//! Opening a store at a new root: moving a previous location's snapshots in,
//! then seeding the id counter past every surviving snapshot. The move runs
//! under both stores' namespace locks, so it cannot land between another
//! publisher's name check and its rename, and it cannot take a snapshot a
//! live session still holds.

use std::fs::read_dir;
use std::fs::remove_dir;
use std::fs::remove_file;
use std::fs::rename;
use std::path::Path;

use super::lease::LeaseProbe;
use super::lease::StoreLock;
use super::lease::claim;
use super::lease::is_legacy_sentinel;
use super::lease::mark_legacy_sentinel;
use super::lease::sentinel_path;
use super::staging::complete_marker;

/// Move a previous store's snapshots into the current one, then drop the old
/// directory if it empties out. The caller holds the previous store's
/// namespace lock and the destination's, so no other process publishes into
/// either while the move runs.
///
/// A snapshot that cannot be moved stays where it is, which means that undo
/// entry is no longer reachable: the store only scans its own root. Every such
/// failure is reported rather than swallowed, and the leftover directory stays
/// on disk for a manual recovery. The complete marker and the lease move
/// before the directory, so a directory under the destination name keeps its
/// verified state and its protection. A source without a marker gets its
/// sentinel tagged before the rename, so the moved snapshot is reported as
/// unverified rather than refused.
///
/// The old namespace lock file is never unlinked. Removing it would let a
/// process blocked on the old inode and a process that opens a fresh file
/// both believe they hold the lock; a leftover directory holding an empty
/// lock file is the safe state.
pub(in crate::snapshot) fn migrate_store(
    legacy: &Path,
    store_root: &Path,
    legacy_lock: StoreLock,
    destination_lock: StoreLock,
) {
    let entries = match read_dir(legacy) {
        Ok(entries) => entries,
        Err(error) => {
            tracing::warn!(
                "previous snapshot store {} cannot be listed: {error}",
                legacy.display()
            );
            return;
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !path.is_dir() || !name.starts_with("snap-") {
            continue;
        }
        move_snapshot(legacy, store_root, &path, &name);
    }
    drop(legacy_lock);
    if let Err(error) = remove_dir(legacy) {
        tracing::debug!(
            "previous snapshot store {} remains: {error}",
            legacy.display()
        );
    }
    drop(destination_lock);
}

/// Move one snapshot into the destination store. A name the destination
/// already fills, a source another process holds, a source that carries a
/// sentinel without a marker, and a destination sentinel another process
/// holds all leave the snapshot where it is.
fn move_snapshot(legacy: &Path, store_root: &Path, path: &Path, name: &str) {
    let dst = store_root.join(name);
    if dst.exists() {
        tracing::warn!(
            "snapshot store move: {} already exists; {} left in place",
            dst.display(),
            path.display()
        );
        return;
    }
    let source_sentinel = sentinel_path(legacy, name);
    if !complete_marker(legacy, name).exists() {
        if source_sentinel.exists() {
            if !is_legacy_sentinel(&source_sentinel) {
                tracing::warn!(
                    "snapshot store move: {} has a lease but no complete marker; \
                     left in place",
                    path.display()
                );
                return;
            }
        } else if let Err(error) = mark_legacy_sentinel(&source_sentinel) {
            tracing::warn!(
                "snapshot store move: {} cannot be tagged as a pre-marker snapshot: {error}",
                path.display()
            );
            return;
        }
    }
    // A snapshot another process still holds stays where it is.
    let _source_lease = match claim(&source_sentinel) {
        LeaseProbe::Claimed(lease) => lease,
        LeaseProbe::Busy => {
            tracing::warn!(
                "snapshot store move: {} is held by a live process; left in place",
                path.display()
            );
            return;
        }
        LeaseProbe::Unreadable => {
            tracing::warn!(
                "snapshot store move: {} cannot be read; left in place",
                path.display()
            );
            return;
        }
    };
    // The destination sentinel is probed, then released. Replacing an open
    // file does not work on every platform, and both namespace locks are
    // held, so no cooperative process can take the name between the probe
    // and the rename. A prune that removed this directory and still holds
    // its sentinel is what the probe is for: it turns the overlap into a
    // skip.
    match claim(&sentinel_path(store_root, name)) {
        LeaseProbe::Claimed(lease) => drop(lease),
        LeaseProbe::Busy => {
            tracing::warn!(
                "snapshot store move: {} is being removed elsewhere; left in place",
                path.display()
            );
            return;
        }
        LeaseProbe::Unreadable => {
            tracing::warn!(
                "snapshot store move: the destination lease for {} cannot be read; \
                 left in place",
                path.display()
            );
            return;
        }
    }
    // A marker without its directory is residue of a move that stopped
    // before the rename. Clearing it keeps an unmarked source from
    // inheriting a verified state; a sentinel left at the destination is
    // replaced by the incoming rename instead of unlinked.
    let stale_marker = complete_marker(store_root, name);
    if stale_marker.exists()
        && let Err(error) = remove_file(&stale_marker)
    {
        tracing::warn!(
            "snapshot sidecar at {} cannot be cleared: {error}",
            stale_marker.display()
        );
        return;
    }
    if !move_sidecars(legacy, store_root, name) {
        return;
    }
    if let Err(error) = rename(path, &dst) {
        tracing::warn!("snapshot store move failed for {}: {error}", path.display());
    }
}

/// Move a snapshot's complete marker and lease beside the snapshot at the
/// destination, stopping at the first failure so the directory never moves
/// without a sidecar that did move.
fn move_sidecars(legacy: &Path, store_root: &Path, name: &str) -> bool {
    for (from, to) in [
        (
            complete_marker(legacy, name),
            complete_marker(store_root, name),
        ),
        (sentinel_path(legacy, name), sentinel_path(store_root, name)),
    ] {
        if !from.exists() {
            continue;
        }
        if let Err(error) = rename(&from, &to) {
            tracing::warn!(
                "snapshot sidecar move failed for {}: {error}",
                from.display()
            );
            return false;
        }
    }
    true
}

/// Scan the store directory for the highest snap-N id so a fresh process
/// starts its counter past existing snapshots (avoids collision).
pub(in crate::snapshot) fn scan_max_snapshot_id(store_root: &Path) -> u64 {
    let mut max = 0;
    if let Ok(entries) = read_dir(store_root) {
        for entry in entries.flatten() {
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false)
                && let Some(name) = entry.file_name().to_str()
                && let Some(num) = name.strip_prefix("snap-")
                && let Ok(n) = num.parse::<u64>()
            {
                max = max.max(n);
            }
        }
    }
    max
}
