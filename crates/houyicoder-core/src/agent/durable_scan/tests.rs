//! Tests for this module.

use super::*;
use houyicoder_context::{EventId, SessionEvent, SessionId, SessionLogEntry};

fn write_session(root: &Path, retry: u32) {
    let dir = root.join(SessionId::new().to_string());
    create_dir_all(&dir).expect("mkdir");
    let ev = SessionLogEntry {
        id: EventId::new(),
        session: SessionId::new(),
        ts: 0,
        prev_hash: None,
        event: SessionEvent::RewardObservation {
            redundant: 0,
            retry_after_error: retry,
        },
    };
    let line = serde_json::to_string(&ev).expect("serialize");
    write(dir.join("log.jsonl"), format!("{line}\n")).expect("write");
}

/// Two sessions with blind retries sum across sessions.
#[test]
fn test_sums_retry_across_sessions() {
    let tmp = temp_dir().join(format!("durable-scan-{}", id()));
    let _cleanup = remove_dir_all(&tmp);
    write_session(&tmp, 2);
    write_session(&tmp, 3);
    assert_eq!(scan_cross_session_retry(&tmp, None), 5);
    let _cleanup = remove_dir_all(&tmp);
}

/// A directory that is not a session (no session id for a name) holds no
/// trajectory to scan, whatever it carries.
#[test]
fn test_ignores_non_session_dir() {
    let tmp = temp_dir().join(format!("durable-scan-stray-{}", id()));
    let _cleanup = remove_dir_all(&tmp);
    let dir = tmp.join("notes");
    create_dir_all(&dir).expect("mkdir");
    let ev = SessionLogEntry {
        id: EventId::new(),
        session: SessionId::new(),
        ts: 0,
        prev_hash: None,
        event: SessionEvent::RewardObservation {
            redundant: 0,
            retry_after_error: 7,
        },
    };
    let line = serde_json::to_string(&ev).expect("serialize");
    write(dir.join("log.jsonl"), format!("{line}\n")).expect("write");
    assert_eq!(scan_cross_session_retry(&tmp, None), 0);
    let _cleanup = remove_dir_all(&tmp);
}

/// The current session is excluded even when its directory is named in the
/// legacy id format. The skip is parsed and compared as an id, so the
/// session the caller already counted in memory is not read back off disk
/// and added again; a string compare against the caller's display form
/// would miss the directory and double-count it.
#[test]
fn test_skips_current_legacy_dir() {
    let tmp = temp_dir().join(format!("durable-scan-legacy-{}", id()));
    let _cleanup = remove_dir_all(&tmp);
    let current = SessionId::new();
    let dir = tmp.join(current.ulid_name());
    create_dir_all(&dir).expect("mkdir");
    let ev = SessionLogEntry {
        id: EventId::new(),
        session: current,
        ts: 0,
        prev_hash: None,
        event: SessionEvent::RewardObservation {
            redundant: 0,
            retry_after_error: 5,
        },
    };
    let line = serde_json::to_string(&ev).expect("serialize");
    write(dir.join("log.jsonl"), format!("{line}\n")).expect("write");

    let skip = current.to_string();
    assert_eq!(
        scan_cross_session_retry(&tmp, Some(&skip)),
        0,
        "the current session's directory is named in the other spelling"
    );
    let _cleanup = remove_dir_all(&tmp);
}

/// A session with no RewardObservation events contributes 0.
#[test]
fn test_session_without_observations() {
    let tmp = temp_dir().join(format!("durable-scan-empty-{}", id()));
    let _cleanup = remove_dir_all(&tmp);
    let dir = tmp.join(SessionId::new().to_string());
    create_dir_all(&dir).expect("mkdir");
    write(dir.join("log.jsonl"), "").expect("write");
    assert_eq!(scan_cross_session_retry(&tmp, None), 0);
    let _cleanup = remove_dir_all(&tmp);
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
    let tmp = temp_dir().join(format!("durable-scan-corrupt-{}", id()));
    let _cleanup = remove_dir_all(&tmp);
    let dir = tmp.join(SessionId::new().to_string());
    create_dir_all(&dir).expect("mkdir");
    let valid = {
        let ev = SessionLogEntry {
            id: EventId::new(),
            session: SessionId::new(),
            ts: 0,
            prev_hash: None,
            event: SessionEvent::RewardObservation {
                redundant: 0,
                retry_after_error: 2,
            },
        };
        serde_json::to_string(&ev).expect("serialize")
    };
    let content = format!("garbage line\n{valid}\nmore garbage\n");
    write(dir.join("log.jsonl"), &content).expect("write");
    assert_eq!(scan_cross_session_retry(&tmp, None), 2);
    let _cleanup = remove_dir_all(&tmp);
}
