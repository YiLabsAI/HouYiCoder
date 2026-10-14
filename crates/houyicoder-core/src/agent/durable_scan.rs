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

use houyicoder_context::session_class::{LOG_FILE, SessionClass, SessionEntry, scan_sessions};
use houyicoder_context::{SessionEvent, SessionId, SessionLogEntry};
use std::cmp::Reverse;
#[cfg(test)]
use std::env::temp_dir;
#[cfg(test)]
use std::fs::create_dir_all;
use std::fs::read_to_string;
#[cfg(test)]
use std::fs::remove_dir_all;
#[cfg(test)]
use std::fs::write;
#[cfg(test)]
use std::process::id;

/// Maximum sessions to scan, most recent first. A long-running project may
/// have hundreds of sessions on disk; scanning all of them on every
/// FinalOutput would be wasteful. The most recent 20 sessions capture the
/// recent retry trend.
const MAX_SESSIONS: usize = 20;

/// Scan the durable session log root for RewardObservation events across
/// previous sessions. Returns the cumulative retry_after_error count.
///
/// The skip param is the current session's id in its display form; the scan
/// parses it and compares ids, so a directory named in either spelling is
/// excluded. It is excluded so the caller can ADD the result to the in-memory
/// snapshot (which already holds the current session's count) without
/// double-counting. None when the caller has no session id (tests, forked).
///
/// The sessions are the ones the store scan classifies as holding a log,
/// sorted by last-active (most recent first) and capped at MAX_SESSIONS.
/// Each session's log.jsonl is read line by line; each line is deserialized
/// as a SessionLogEntry. RewardObservation events carry the retry_after_error
/// count for that batch.
///
/// Best-effort: unreadable logs, corrupt lines, and missing directories
/// return 0, not an error. The scan is an amplifier, not a gate — a failed
/// scan just means no cross-session data, which falls back to the in-memory
/// snapshot.
pub(crate) fn scan_cross_session_retry(root: &Path, skip: Option<&str>) -> u32 {
    let mut sessions: Vec<SessionEntry> = scan_sessions(root)
        .into_iter()
        // A shell carries no log, so there is nothing to read; every other
        // class holds a durable trajectory worth scanning, including a
        // sub-agent session (its failures are the project's failures).
        .filter(|session| session.class != SessionClass::Shell)
        .collect();
    sessions.sort_by_key(|session| Reverse(session.last_active));
    // The skip arrives as a string (the caller's display form); comparing
    // parsed ids keeps it matching a directory named in either the current
    // or the legacy id format.
    let skip = skip.and_then(SessionId::from_display_string);
    let mut total = 0u32;
    for session in sessions.iter().take(MAX_SESSIONS) {
        // Skip the current session — its count lives in the in-memory
        // snapshot, so including it here + adding there double-counts.
        if skip == Some(session.sid) {
            continue;
        }
        let log = session.path.join(LOG_FILE);
        let Ok(content) = read_to_string(&log) else {
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
            let Ok(ev) = serde_json::from_str::<SessionLogEntry>(line) else {
                continue;
            };
            if let SessionEvent::RewardObservation {
                retry_after_error, ..
            } = ev.event
            {
                total = total.saturating_add(retry_after_error);
            }
        }
    }
    total
}

#[cfg(test)]
mod tests;
