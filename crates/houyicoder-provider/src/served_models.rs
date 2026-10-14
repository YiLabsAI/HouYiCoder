//! The startup /v1/models fetch + cache write. The provider asks the
//! endpoint once which model ids it actually serves; the result lands in the
//! config crate's served-models cache so catalog validation can flag a stale
//! or typo'd id. Fire-and-forget at startup, all-fault-tolerant
//! (failure degrades — the caller debug-logs, the existing cache is
//! kept, nothing surfaces to the user). Existence only: the
//! OpenAI-compatible /v1/models returns ids, not capability fields.

use std::collections::HashSet;

use houyicoder_config::ServedModels;
use serde_json::Value;
use std::fs::File;
use std::fs::OpenOptions;
use std::fs::create_dir_all;
use std::fs::remove_file;
use std::fs::rename;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

/// Parse the OpenAI-compatible /v1/models response body into the served id
/// list. Deduplicates (keep first occurrence), drops blank ids, and sorts
/// longest-id-first so the substring match in config prefers the most
/// specific served id. Pure so it is unit-testable without a network.
pub fn parse_response(body: &Value) -> Vec<String> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut ids: Vec<String> = Vec::new();
    if let Some(data) = body.get("data").and_then(Value::as_array) {
        for entry in data {
            if let Some(id) = entry.get("id").and_then(Value::as_str) {
                let trimmed = id.trim();
                if !trimmed.is_empty() && seen.insert(trimmed.to_lowercase()) {
                    ids.push(trimmed.to_string());
                }
            }
        }
    }
    sort_longest_first(ids)
}

/// Longest-id-first (secondary: case-insensitive lexical) so a more specific
/// served id wins the substring match in config::exists_in.
fn sort_longest_first(mut ids: Vec<String>) -> Vec<String> {
    ids.sort_by(|a, b| {
        b.len()
            .cmp(&a.len())
            .then_with(|| a.to_lowercase().cmp(&b.to_lowercase()))
    });
    ids
}

/// Write the cache atomically (temp + rename, 0o600) so a crash mid-write
/// leaves either the old cache or the new one, never a torn file.
/// Best-effort: a write failure is logged and swallowed — the cache is
/// advisory, the process still runs without it.
pub(crate) fn write_cache(path: &Path, ids: &[String]) {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    if let Err(e) = create_dir_all(dir) {
        tracing::warn!("served-models cache: create_dir_all failed: {e}");
        return;
    }
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let payload = ServedModels {
        ids: ids.to_vec(),
        timestamp,
    };
    let Ok(json) = serde_json::to_string(&payload) else {
        return;
    };
    let tmp = path.with_extension("json.tmp");
    match open_cache_file(&tmp) {
        Ok(mut f) => {
            if let Err(e) = f.write_all(json.as_bytes()) {
                tracing::warn!("served-models cache: write failed: {e}");
                drop(remove_file(&tmp));
                return;
            }
            drop(f);
            if let Err(e) = rename(&tmp, path) {
                tracing::warn!("served-models cache: rename failed: {e}");
                drop(remove_file(&tmp));
            }
        }
        Err(e) => tracing::warn!("served-models cache: open tmp failed: {e}"),
    }
}

/// Open the cache temp file with restrictive permissions on Unix (0o600).
/// On non-Unix platforms the default permissions are used.
#[cfg(unix)]
fn open_cache_file(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn open_cache_file(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
}

#[cfg(test)]
mod tests;
