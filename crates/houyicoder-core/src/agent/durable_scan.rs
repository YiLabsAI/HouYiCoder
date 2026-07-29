//! Cross-session reward scanning: reads the durable session log (previous
//! sessions' log.jsonl files on disk) for RewardObservation events so the
//! dream can learn from failures that span sessions, not just the current
//! one.
//!
//! The in-memory observability layer (SharedObservability) is per-process
//! and lost on exit. The RewardObservation event is appended to the durable
//! trajectory (append.rs) so a later dream in a later session can scan the
//! cross-session retry_after_error trend. This module is the reader for
//! that scan.
//!
//! Best-effort: a corrupt or unreadable log line is skipped, not fatal. The
//! scan caps at MAX_SESSIONS (most recent by mtime) so a long-running
//! project with hundreds of sessions does not scan forever.

use std::path::Path;

use houyicoder_context::{TurnEvent, TurnEventKind};

/// Maximum sessions to scan, most recent first. A long-running project may
/// have hundreds of sessions on disk; scanning all of them on every
/// FinalOutput would be wasteful. The most recent 20 sessions capture the
/// recent retry trend.
const MAX_SESSIONS: usize = 20;

/// Scan the durable session log root for RewardObservation events across
/// previous sessions. Returns the cumulative retry_after_error count.
///
/// The skip param is the current session's id (as a directory name) — the
/// scan excludes it so the caller can ADD the result to the in-memory
/// snapshot (which already holds the current session's count) without
/// double-counting. None when the caller has no session id (tests, forked).
///
/// Sessions are sorted by directory mtime (most recent first) and capped at
/// MAX_SESSIONS. Each session's log.jsonl is read line by line; each line is
/// deserialized as a TurnEvent. RewardObservation events carry the
/// retry_after_error count for that batch.
///
/// Best-effort: unreadable logs, corrupt lines, and missing directories
/// return 0, not an error. The scan is an amplifier, not a gate — a failed
/// scan just means no cross-session data, which falls back to the in-memory
/// snapshot.
pub(crate) fn scan_cross_session_retry(root: &Path, skip: Option<&str>) -> u32 {
    let Ok(sessions) = std::fs::read_dir(root) else {
        return 0;
    };
    let mut dirs: Vec<(std::time::SystemTime, std::path::PathBuf)> = sessions
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            let meta = e.metadata().ok()?;
            // Skip non-directory entries (e.g. .cas block stores that
            // sit alongside session dirs and steal a slot in the
            // most-recent-20 window).
            if !meta.is_dir() {
                return None;
            }
            let mtime = meta.modified().ok()?;
            Some((mtime, p))
        })
        .collect();
    dirs.sort_by_key(|b| std::cmp::Reverse(b.0));
    let mut total = 0u32;
    for (_, dir) in dirs.iter().take(MAX_SESSIONS) {
        // Skip the current session — its count lives in the in-memory
        // snapshot, so including it here + adding there double-counts.
        if let Some(skip) = skip
            && dir
                .file_name()
                .map(|n| n == std::ffi::OsStr::new(skip))
                .unwrap_or(false)
        {
            continue;
        }
        let log = dir.join("log.jsonl");
        let Ok(content) = std::fs::read_to_string(&log) else {
            continue;
        };
        for line in content.lines() {
            // Cheap string filter before serde: only RewardObservation
            // lines carry the retry count; skip the rest without the
            // cost of full deserialization. A long session log is MBs
            // of user/assistant/tool events — most lines are not
            // reward observations.
            if !line.contains("RewardObservation") {
                continue;
            }
            let Ok(ev) = serde_json::from_str::<TurnEvent>(line) else {
                continue;
            };
            if let TurnEventKind::RewardObservation {
                retry_after_error, ..
            } = ev.kind
            {
                total = total.saturating_add(retry_after_error);
            }
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;
    use houyicoder_context::{EventId, SessionId, TurnEvent, TurnEventKind};

    fn write_session(root: &Path, sid: &str, retry: u32) {
        let dir = root.join(sid);
        std::fs::create_dir_all(&dir).expect("mkdir");
        let ev = TurnEvent {
            id: EventId::new(),
            session: SessionId::new(),
            ts: 0,
            prev_hash: None,
            kind: TurnEventKind::RewardObservation {
                redundant: 0,
                retry_after_error: retry,
            },
        };
        let line = serde_json::to_string(&ev).expect("serialize");
        std::fs::write(dir.join("log.jsonl"), format!("{line}\n")).expect("write");
    }

    /// Two sessions with blind retries sum across sessions.
    #[test]
    fn test_sums_retry_across_sessions() {
        let tmp = std::env::temp_dir().join(format!("durable-scan-{}", std::process::id()));
        let _cleanup = std::fs::remove_dir_all(&tmp);
        write_session(&tmp, "s1", 2);
        write_session(&tmp, "s2", 3);
        assert_eq!(scan_cross_session_retry(&tmp, None), 5);
        let _cleanup = std::fs::remove_dir_all(&tmp);
    }

    /// A session with no RewardObservation events contributes 0.
    #[test]
    fn test_session_without_observations() {
        let tmp = std::env::temp_dir().join(format!("durable-scan-empty-{}", std::process::id()));
        let _cleanup = std::fs::remove_dir_all(&tmp);
        let dir = tmp.join("s1");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("log.jsonl"), "").expect("write");
        assert_eq!(scan_cross_session_retry(&tmp, None), 0);
        let _cleanup = std::fs::remove_dir_all(&tmp);
    }

    /// A missing root returns 0, not an error.
    #[test]
    fn test_missing_root_returns_zero() {
        assert_eq!(
            scan_cross_session_retry(Path::new("/nonexistent/path"), None),
            0
        );
    }

    /// A corrupt line is skipped, not fatal.
    #[test]
    fn test_corrupt_line_skipped() {
        let tmp = std::env::temp_dir().join(format!("durable-scan-corrupt-{}", std::process::id()));
        let _cleanup = std::fs::remove_dir_all(&tmp);
        let dir = tmp.join("s1");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let valid = {
            let ev = TurnEvent {
                id: EventId::new(),
                session: SessionId::new(),
                ts: 0,
                prev_hash: None,
                kind: TurnEventKind::RewardObservation {
                    redundant: 0,
                    retry_after_error: 2,
                },
            };
            serde_json::to_string(&ev).expect("serialize")
        };
        let content = format!("garbage line\n{valid}\nmore garbage\n");
        std::fs::write(dir.join("log.jsonl"), &content).expect("write");
        assert_eq!(scan_cross_session_retry(&tmp, None), 2);
        let _cleanup = std::fs::remove_dir_all(&tmp);
    }
}
