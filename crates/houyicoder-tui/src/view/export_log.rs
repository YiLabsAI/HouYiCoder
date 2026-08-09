//! The export seam: the TUI owns the contract (ExportLog trait +
//! ExportPayload), the composition root injects an impl backed by the runner's
//! SessionLog that projects the durable event stream into a self-contained
//! JSON document. Matches the trajectory-data + disk-search seams: the TUI
//! never touches the log file or the event types — it gets a serialized
//! payload + a suggested filename and writes it to disk.
//!
//! /export is TUI-local (no server round-trip): it serializes the session's
//! durable trajectory + tool stats + usage + checkpoints + errors to a
//! single JSON file. This is the self-evolution data source — a
//! machine-readable record of what happened, distinct from the
//! human-readable .txt export (different use, not a superiority claim).

use std::io;
use std::path::Path;

/// The serialized export document + a suggested default filename. The bridge
/// builds both from the durable event stream (started_at + first-prompt slug
/// drive the filename); the command handler honors an explicit path argument
/// or falls back to the suggestion.
pub struct ExportPayload {
    /// Suggested filename (no directory): timestamp + first-prompt slug + .json.
    pub filename: String,
    /// Pretty-printed JSON document. Serialized at the bridge so the TUI stays
    /// free of the export data shape (the bridge owns the projection).
    pub json: String,
}

/// Project the durable session log into an export document. None in stub /
/// unwired modes (the command then reports "no session log wired" instead of
/// writing an empty file).
pub trait ExportLog: Send + Sync {
    fn export(&self) -> ExportPayload;
}

/// Write bytes to a path atomically with 0o600 file permissions (the
/// local .jsonl 0o600 convention — parity, not a regression). Atomicity:
/// write to a sibling temp file then rename, so a crash mid-write never leaves
/// a half-written export. The 0o600 permission isolates the export to the
/// owner — the durable log carries real tool I/O + reasoning that may contain
/// secrets (redact-on-write is a separate, deferred boundary; the file
/// permission is the floor).
pub fn write_atomic_0600(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let tmp = dir.join(format!(
        ".{}.tmp",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("export")
    ));
    // Create the temp file with 0o600 AT OPEN (not after write) so the
    // secret payload is never briefly world-readable between create and a
    // later set_permissions call. Matches the local file pattern
    // (OpenOptions::mode at create). std::fs::set_permissions after the fact
    // leaves a window where the temp holds real tool I/O + reasoning.
    {
        use std::io::Write;
        let mut f = {
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                std::fs::OpenOptions::new()
                    .create(true)
                    .write(true)
                    .truncate(true)
                    .mode(0o600)
                    .open(&tmp)?
            }
            #[cfg(not(unix))]
            {
                std::fs::File::create(&tmp)?
            }
        };
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_write_round_trips_bytes() {
        let dir = std::env::temp_dir();
        let path = dir.join("houyi_export_test_roundtrip.json");
        std::fs::remove_file(&path).ok();
        let payload = b"{\"hello\":\"world\"}";
        write_atomic_0600(&path, payload).expect("write");
        let read = std::fs::read(&path).expect("read");
        assert_eq!(read, payload);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path)
                .expect("metadata")
                .permissions()
                .mode();
            assert_eq!(
                mode & 0o777,
                0o600,
                "export file must be 0o600 (owner-only)"
            );
        }
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_write_atomic_replaces_existing() {
        // A second write to the same path replaces the first content; the
        // temp-then-rename path must not leave the old bytes visible.
        let dir = std::env::temp_dir();
        let path = dir.join("houyi_export_test_replace.json");
        std::fs::remove_file(&path).ok();
        write_atomic_0600(&path, b"first").expect("write 1");
        write_atomic_0600(&path, b"second").expect("write 2");
        let read = std::fs::read(&path).expect("read");
        assert_eq!(read, b"second");
        std::fs::remove_file(&path).ok();
    }
}
