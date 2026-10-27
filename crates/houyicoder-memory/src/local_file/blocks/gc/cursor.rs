//! Where an unfinished reclaim pass stopped, and when that record may be
//! trusted: the source list it belongs to, the size and time each filed log
//! held, the bytes each bucket holds, and the last shard the sweep finished.
//! Only a log is skipped on a resume, since appending keeps every address
//! already filed; a manifest is rewritten whole and is read again every pass.
//! A log that grew sends the mark back to the log before it, and any other
//! change, or a record that cannot be trusted, sends it to the start.

use std::collections::BTreeMap;
use std::fs;
use std::fs::OpenOptions;
use std::io::ErrorKind;
use std::io::Write;
use std::path::Path;

use serde::Deserialize;
use serde::Serialize;

use super::ReclaimError;
use super::ReferenceSource;
use super::scratch;
use super::source_state;
use crate::sha256_hex;

/// The on-disk shape of the cursor. A cursor written by another version of
/// this pass is not trusted to share its meaning; the mark is redone.
pub(super) const CURSOR_SCHEMA: u32 = 3;

/// Where an unfinished pass stopped: the last log filed, the source list it
/// belongs to, what each filed log held when it was read, how many bytes each
/// bucket holds, and the last shard the sweep finished.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Cursor {
    pub(super) schema: u32,
    pub(super) done: String,
    pub(super) fingerprint: String,
    /// What each filed log held when the mark read it, keyed by source key.
    pub(super) filed: BTreeMap<String, SourceState>,
    pub(super) buckets: BTreeMap<String, u64>,
    /// The last shard directory the sweep finished, so a pass that spent its
    /// budget while sweeping continues at the next one.
    pub(super) swept: Option<String>,
}

/// The size and modification time of a source at the moment the mark read it.
#[derive(Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct SourceState {
    pub(super) modified_ms: u64,
    pub(super) len: u64,
}

impl Cursor {
    /// Write the cursor, so a later pass can continue where this one stopped.
    /// Callers write it only after the buckets reached the disk, so a cursor
    /// that survives a crash names references the store still holds.
    pub(super) fn write_to(&self, path: &Path) -> Result<(), ReclaimError> {
        let body = serde_json::to_string(self).map_err(|e| scratch(path, e))?;
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(path)
            .map_err(|e| scratch(path, e))?;
        file.write_all(body.as_bytes())
            .map_err(|e| scratch(path, e))?;
        file.sync_all().map_err(|e| scratch(path, e))
    }
}

/// The cursor a resumed pass may trust, if any.
///
/// A filed log must still hold the size and modification time the mark
/// recorded, and one the pass cannot stat is no better: two unknown states
/// are not two equal ones. A log that grew is the only recoverable change,
/// since its earlier addresses stay valid: the mark goes back to the log
/// before it and reads from there. A log that shrank may have dropped a
/// reference the bucket still holds, so the mark starts over, as it does for
/// missing bucket bytes. A manifest is never trusted here; the mark reads
/// every one again.
pub(super) fn resumable_cursor(
    cursor: &Path,
    refs: &Path,
    sources: &[ReferenceSource],
    fingerprint: &str,
) -> Option<Cursor> {
    let text = match fs::read_to_string(cursor) {
        Ok(text) => text,
        Err(error) if error.kind() == ErrorKind::NotFound => return None,
        Err(error) => {
            tracing::debug!(?error, "reclaim cursor unreadable; the mark starts over");
            return None;
        }
    };
    let held: Cursor = match serde_json::from_str(&text) {
        Ok(held) => held,
        Err(error) => {
            tracing::debug!(?error, "reclaim cursor unreadable; the mark starts over");
            return None;
        }
    };
    if held.schema != CURSOR_SCHEMA || held.fingerprint != fingerprint {
        return None;
    }
    let filed: Vec<&ReferenceSource> = sources
        .iter()
        .filter(|source| source.is_append_only() && source.key() <= held.done.as_str())
        .collect();
    if held.filed.len() != filed.len()
        || !filed
            .iter()
            .all(|source| held.filed.contains_key(source.key()))
    {
        return None;
    }
    let backed = held.buckets.iter().all(|(name, recorded)| {
        fs::metadata(refs.join(name)).is_ok_and(|meta| meta.is_file() && meta.len() >= *recorded)
    });
    if !backed {
        return None;
    }
    let mut moved = None;
    for source in &filed {
        let now = source_state(source.path());
        let same = match (now.as_ref(), held.filed.get(source.key())) {
            (Some(now), Some(seen)) => now == seen,
            _ => false,
        };
        if same {
            continue;
        }
        let now = now?;
        moved = Some((source, now));
        break;
    }
    let Some((moved, now)) = moved else {
        return Some(held);
    };
    // A log only gains references, so the addresses the earlier mark filed
    // from it stay valid, and reading it again keeps a store with one active
    // session from re-reading every log on each pass.
    let grew = held
        .filed
        .get(moved.key())
        .is_some_and(|seen| now.len > seen.len);
    if !grew {
        return None;
    }
    let mut resumed = held;
    resumed.done = sources
        .iter()
        .filter(|source| source.is_append_only())
        .map(ReferenceSource::key)
        .take_while(|key| *key < moved.key())
        .last()
        .unwrap_or_default()
        .to_string();
    resumed.filed.retain(|key, _| key.as_str() < moved.key());
    Some(resumed)
}

/// One digest over the source list, so a cursor left by a pass over another
/// list is never trusted for this one: a source added since, as an imported
/// session carrying older references, changes the digest and the mark starts
/// over rather than skipping a source it never read.
pub(super) fn source_fingerprint(sources: &[ReferenceSource]) -> String {
    let mut joined = String::new();
    for source in sources {
        joined.push_str(source.key());
        joined.push('\n');
    }
    sha256_hex(joined.as_bytes()).0
}
