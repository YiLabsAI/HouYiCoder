//! Merge-preserving, atomic, CAS-guarded settings writes.
//!
//! Reads the settings file as a serde_json::Value (preserving all unknown
//! keys), runs a caller-supplied mutator to edit only the target keys, and
//! writes back via a pid- and timestamp-suffixed temp file + rename.
//! Concurrent writers are serialised by an exclusive file lock on a
//! sibling .lock file (handle-based: a crash frees it, a leftover file
//! does not block), with a content-hash token as a second guard against
//! a writer that does not take the lock (e.g. a user hand-editing the
//! file).

/// Error from a merge-preserving settings write. Separate from ConfigError
/// (provider resolution) so a settings failure never masquerades as an auth
/// problem. Callers surface these to the user, not the provider path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingsWriteError {
    /// I/O failure reading or writing the settings file.
    Io(String),
    /// The file exists but is not valid JSON; it is not overwritten.
    CorruptJson(String),
    /// Concurrent writers kept winning; max_retries hit without a clean
    /// rename. The file is left in the last winner's state.
    CasRetriesExhausted,
}

/// Merge-preserving, atomic, CAS-guarded settings write. Reads the file as
/// a JSON Value (preserving all unknown keys — serde's default is passthrough,
/// so unrecognised fields round-trip unchanged), runs the mutator to edit
/// only the target keys, and writes back via a pid- and timestamp-suffixed
/// temp file + rename.
///
/// A corrupt JSON file (syntax error) is never overwritten — the error is
/// returned so a broken file is not silently clobbered. A missing file is
/// treated as an empty object.
///
/// Concurrent writers are reconciled by a content-hash token taken from the
/// initial read and re-checked against a fresh read before rename: if another
/// writer landed in between, the hash differs and the mutator re-runs against
/// the latest content, bounded by max_retries. A content hash (rather than
/// mtime+size) avoids same-second, same-size collisions — a settings write
/// often leaves size unchanged (a boolean flip). The mutator must be pure
/// (only mutates the Value, no external side effects) because CAS retry
/// re-invokes it — hence Fn, not FnOnce.
///
/// Concurrency: an exclusive file lock on a sibling .lock file serialises
/// all update_settings callers, so the content-hash CAS below never races
/// another in-process writer. The CAS stays as a second guard against a
/// change made outside update_settings (a user hand-editing the file
/// mid-write) — the lock does not fence that out, the hash does.
pub fn update_settings(
    path: &std::path::Path,
    mutator: impl Fn(&mut serde_json::Value),
    max_retries: usize,
) -> Result<(), SettingsWriteError> {
    let parent = path
        .parent()
        .ok_or_else(|| SettingsWriteError::Io("settings path has no parent".into()))?;
    std::fs::create_dir_all(parent).map_err(|e| SettingsWriteError::Io(e.to_string()))?;
    let lock_path = parent.join(format!(
        "{}.lock",
        path.file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("settings")
    ));
    let _lock = acquire_settings_lock(&lock_path)?;
    let temp = parent.join(format!(
        ".{}.{}.{:?}.{}.tmp",
        path.file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("settings"),
        std::process::id(),
        std::thread::current().id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    ));
    for _ in 0..=max_retries {
        let mut value = match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str::<serde_json::Value>(&text)
                .map_err(|e| SettingsWriteError::CorruptJson(e.to_string()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                serde_json::Value::Object(serde_json::Map::new())
            }
            Err(e) => return Err(SettingsWriteError::Io(e.to_string())),
        };
        let token = content_token(&std::fs::read_to_string(path).unwrap_or_default());
        mutator(&mut value);
        let json = serde_json::to_string_pretty(&value)
            .map_err(|e| SettingsWriteError::Io(e.to_string()))?;
        {
            use std::io::Write;
            let mut opts = std::fs::OpenOptions::new();
            opts.write(true).create(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }
            let mut f = match opts.open(&temp) {
                Ok(f) => f,
                Err(_) => {
                    drop(std::fs::remove_file(&temp));
                    return Err(SettingsWriteError::Io("failed to open temp file".into()));
                }
            };
            if f.write_all(json.as_bytes()).is_err() || f.sync_all().is_err() {
                drop(std::fs::remove_file(&temp));
                return Err(SettingsWriteError::Io("failed to write temp file".into()));
            }
        }
        let current_token = content_token(&std::fs::read_to_string(path).unwrap_or_default());
        if current_token == token {
            // Under the lock no in-process caller races us, so the token
            // + verify guard only against an OUT-OF-BAND change (a hand-edit
            // made without taking the lock): the token detects a change
            // since our read, this verify detects one after our rename.
            // The verify cannot protect an earlier caller we might have
            // overwritten — that caller already returned Ok with no chance
            // to retry — which is exactly why inter-caller exclusion lives
            // in the lock, not in this CAS loop.
            if std::fs::rename(&temp, path).is_err() {
                drop(std::fs::remove_file(&temp));
                return Err(SettingsWriteError::Io("failed to rename temp".into()));
            }
            // Best-effort fsync of the parent directory so the rename
            // survives a crash.
            if let Ok(dir) = std::fs::File::open(parent) {
                drop(dir.sync_all());
            }
            // Verify: did we win the race? If another thread renamed after
            // us, its content (without our mutation) would be on disk. We
            // compare the file content to what we wrote.
            let verify = std::fs::read_to_string(path)
                .map(|c| c == json)
                .unwrap_or(false);
            if verify {
                return Ok(());
            }
            // Lost the race: retry against the new content.
            continue;
        }
        // Another writer won the race; retry against the latest content.
        drop(std::fs::remove_file(&temp));
    }
    Err(SettingsWriteError::CasRetriesExhausted)
}

/// Content-hash fingerprint for change detection. A content hash catches
/// same-second, same-size rewrites that an mtime+size token would miss (a
/// boolean flip leaves the file size unchanged). Uses the std default hasher
/// — not cryptographic, only a collision-free token for the CAS loop within
/// a process; the same bytes always hash to the same value.
fn content_token(text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

fn acquire_settings_lock(
    lock_path: &std::path::Path,
) -> Result<SettingsLockGuard, SettingsWriteError> {
    // std::fs::File::lock is an exclusive file lock: flock on unix,
    // LockFileEx on windows — same primitive the previous unix-only flock
    // used, now covering both. Opened .write(true) (windows refuses to
    // lock an append-only handle) and .truncate(false) explicitly: the
    // file is a zero-byte sentinel, so truncating is pointless and on
    // windows can fail while another handle holds the exclusive lock. The
    // guard holds the File; dropping it closes the handle and releases
    // the lock.
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(lock_path)
        .map_err(|e| SettingsWriteError::Io(format!("open lock {lock_path:?}: {e}")))?;
    file.lock()
        .map_err(|e| SettingsWriteError::Io(format!("lock {lock_path:?}: {e}")))?;
    Ok(SettingsLockGuard { _file: file })
}

struct SettingsLockGuard {
    _file: std::fs::File,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A mutator that, on its first call, writes a different file to the path
    /// to simulate a concurrent writer racing between our CAS check and rename.
    /// This forces the verify check to see content != our json, triggering the
    /// retry path.
    #[test]
    fn test_cas_retries_toctou_race() {
        let dir =
            std::env::temp_dir().join(format!("cas-toctou-{}-{}", std::process::id(), line!()));
        drop(std::fs::remove_dir_all(&dir));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        std::fs::write(&path, r#"{"v":0}"#).unwrap();
        let first = std::sync::atomic::AtomicBool::new(true);
        let path_for_mutator = path.clone();
        let result = update_settings(
            &path,
            |v| {
                if first.swap(false, std::sync::atomic::Ordering::SeqCst) {
                    // Simulate a concurrent writer: modify the file so our CAS
                    // token is stale, forcing the verify to see different
                    // content and retry.
                    std::fs::write(&path_for_mutator, r#"{"v":99}"#).unwrap();
                }
                v["v"] = 1.into();
            },
            5,
        );
        assert!(result.is_ok(), "CAS retry should succeed: {result:?}");
        let after: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(after["v"], 1, "our mutation landed after retry");
        drop(std::fs::remove_dir_all(&dir));
    }
}
