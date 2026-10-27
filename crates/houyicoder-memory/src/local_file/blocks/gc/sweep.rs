//! The sweep half of a reclaim pass: walk the store one shard at a time and
//! remove what the mark did not name.
//!
//! Every removal is bounded by the write lease, so a writer whose block landed
//! during the pass is not freed before the event naming it can be appended. A
//! shard is recorded as finished before the budget is checked, so a pass that
//! stops part way resumes after it instead of starting over.

use std::fs;
use std::io::ErrorKind;
use std::path::Path;
use std::path::PathBuf;
use std::time::Instant;
use std::time::SystemTime;

use super::super::BLOCK_SUFFIX;
use super::super::SHARD_CHARS;
use super::super::TEMP_SUFFIX;
use super::MarkRun;
use super::ReclaimError;
use super::ReclaimPolicy;
use super::ReclaimReport;
use super::unreadable;
use crate::local_file::LocalFileBackend;

impl LocalFileBackend {
    /// Walk the store one shard at a time, removing what the mark did not
    /// name. Both sides are sorted by address, so one index over the bucket
    /// answers every block in the shard.
    ///
    /// The cursor records a finished shard at a widening interval and at the
    /// budget stop. A shard the record does not reach is walked again by the
    /// next pass and finds nothing to free, so a late record costs a re-walk
    /// and never a missed removal.
    pub(super) fn sweep(
        &self,
        run: &mut MarkRun,
        policy: &ReclaimPolicy,
        started: Instant,
    ) -> Result<ReclaimReport, ReclaimError> {
        let refs = run.refs.clone();
        let mut finished = 0usize;
        for shard in self.shard_dirs()? {
            let Some(name) = shard.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if run.swept.as_deref().is_some_and(|swept| name <= swept) {
                continue;
            }
            let mut named = read_bucket(&refs.join(name), run.buckets.contains_key(name))?;
            named.sort();
            named.dedup();
            let mut blocks = Vec::new();
            let entries = fs::read_dir(&shard).map_err(|e| unreadable(&shard, e))?;
            for entry in entries {
                let entry = entry.map_err(|e| unreadable(&shard, e))?;
                let Ok(file) = entry.file_name().into_string() else {
                    continue;
                };
                blocks.push(file);
            }
            blocks.sort();
            let mut named_at = 0;
            for file in blocks {
                if let Some(address) = file.strip_suffix(BLOCK_SUFFIX) {
                    while named_at < named.len() && named[named_at].as_str() < address {
                        named_at += 1;
                    }
                    run.report.blocks += 1;
                    if named.get(named_at).is_none_or(|entry| entry != address) {
                        remove_if_stale(&shard.join(&file), policy, &mut run.report);
                    }
                } else if file.ends_with(TEMP_SUFFIX) {
                    // A write that never reached its rename: no reference can
                    // name a temp file, so it needs only the same age rule a
                    // live writer's temp file relies on.
                    remove_if_stale(&shard.join(&file), policy, &mut run.report);
                }
            }
            finished += 1;
            let spent = started.elapsed() >= policy.budget;
            if finished.is_power_of_two() || spent {
                run.record_swept(name)?;
            }
            // The budget is checked after a shard, never before one: a shard
            // whose references were read and whose files were walked is done,
            // and a pass that stopped before touching a shard would never make
            // progress at all under a small budget.
            if spent {
                return Ok(run.report);
            }
        }
        run.report.complete = true;
        Ok(run.report)
    }

    /// The shard directories under the store, in address order.
    fn shard_dirs(&self) -> Result<Vec<PathBuf>, ReclaimError> {
        let dir = self.cas_dir();
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(unreadable(&dir, e)),
        };
        let mut shards = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| unreadable(&dir, e))?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let hex = |c: char| c.is_ascii_hexdigit() && !c.is_ascii_uppercase();
            if name.len() != SHARD_CHARS || !name.chars().all(hex) {
                continue;
            }
            // A link is not a shard. Following one would let a name in the
            // store root point the walk at a directory elsewhere, and every
            // block-shaped file there would be removed by this pass. A shard
            // that cannot be typed is not an empty shard either: working past
            // it would leave its blocks behind while reporting a finished
            // pass, so the walk stops as unreadable.
            let path = entry.path();
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => shards.push(path),
                Ok(_) => {}
                Err(e) if e.kind() == ErrorKind::NotFound => {}
                Err(e) => return Err(unreadable(&path, e)),
            }
        }
        shards.sort();
        Ok(shards)
    }
}

/// The addresses one bucket holds, one per line. A bucket this pass recorded
/// and now cannot find is an error rather than an empty set: the sweep would
/// free every block whose address it held.
pub(super) fn read_bucket(path: &Path, expected: bool) -> Result<Vec<String>, ReclaimError> {
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(e) if e.kind() == ErrorKind::NotFound && !expected => return Ok(Vec::new()),
        Err(e) => return Err(super::scratch(path, e)),
    };
    Ok(content
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect())
}

/// Remove one block, unless it is young enough that a writer may still be
/// about to name it. An age that cannot be read keeps the block.
///
/// The modification time is read again just before the removal, and a changed
/// one keeps the block: a writer that re-stored these bytes in between
/// refreshed the time and is about to append the event naming them. Two
/// windows remain. The second read still precedes the unlink by a few
/// instructions, and a filesystem whose timestamps are coarser than the
/// interval between the two reads reports the same time for a re-store. The
/// lease covers a writer whose append lands inside it; one that lands later
/// than the lease is outside anything this pass can observe.
fn remove_if_stale(path: &Path, policy: &ReclaimPolicy, report: &mut ReclaimReport) {
    let Ok(modified) = fs::metadata(path).and_then(|meta| meta.modified()) else {
        report.deferred += 1;
        return;
    };
    let age = SystemTime::now()
        .duration_since(modified)
        .unwrap_or_default();
    if age < policy.write_lease {
        report.leased += 1;
        return;
    }
    let unchanged = fs::metadata(path)
        .and_then(|meta| meta.modified())
        .is_ok_and(|now| now == modified);
    if !unchanged {
        report.deferred += 1;
        return;
    }
    match fs::remove_file(path) {
        Ok(()) => report.removed += 1,
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(_) => report.deferred += 1,
    }
}
