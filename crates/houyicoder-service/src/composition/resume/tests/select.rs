//! Which session --continue selects, and the directory spelling it resolves
//! from. Split from the resume tests on size and subject grounds: this half is
//! about choosing a session out of a store, the other about building a runner
//! out of one.

use super::super::{
    RECENT_WINDOW, build_runner_for_resume_sid, resume_candidates, resume_candidates_all,
};
use super::*;
use crate::composition::workspace_cwd;
use std::fs::FileTimes;
use std::fs::OpenOptions;
use std::fs::create_dir_all;
use std::fs::remove_dir_all;
use std::fs::write;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::time::SystemTime;
use tokio::runtime::Builder;

/// Write a session directly on disk: a log with one event plus a descriptor whose
/// cwd is the current workspace, under the given directory name. Returns the
/// directory. Used where the directory name is the point of the test.
fn write_session_at(sessions: &Path, name: &str, provenance: SessionProvenance) -> PathBuf {
    write_session_in(sessions, name, provenance, &workspace_cwd(None))
}

/// The same, recorded in a caller-chosen workspace.
fn write_session_in(
    sessions: &Path,
    name: &str,
    provenance: SessionProvenance,
    cwd: &str,
) -> PathBuf {
    let dir = sessions.join(name);
    create_dir_all(&dir).unwrap();
    let sid = SessionId::from_display_string(name).expect("directory name is a session id");
    let entry = SessionLogEntry {
        id: EventId::new(),
        session: sid,
        ts: 0,
        prev_hash: None,
        event: SessionEvent::UserInput {
            text: "continue-me".into(),
        },
    };
    write(
        dir.join("log.jsonl"),
        format!("{}\n", serde_json::to_string(&entry).unwrap()),
    )
    .unwrap();
    let descriptor = SessionDescriptor {
        name: None,
        name_source: NameSource::Auto,
        cwd: cwd.to_string(),
        model: "m".into(),
        provenance,
        version: "t".into(),
        created_at: 1,
        child_session_ids: Vec::new(),
        first_prompt: None,
        last_prompt: None,
        prompt_coverage: Default::default(),
    };
    write(
        dir.join("session.json"),
        serde_json::to_vec(&descriptor).unwrap(),
    )
    .unwrap();
    dir
}

/// A sub-agent session is the newest directory in the workspace, and it is not
/// a continuation: --continue must find nothing rather than land in the child.
#[test]
fn test_candidates_skip_child() {
    let sessions = temp_root();
    write_session_at(
        &sessions,
        &SessionId::new().to_string(),
        SessionProvenance::SpawnedBy {
            parent_session_id: "parent".into(),
            subagent_type: "explore".into(),
            task_id: "task".into(),
        },
    );
    assert!(
        resume_candidates(&sessions).is_empty(),
        "a child is a sidechain of a parent, not something to continue"
    );
    remove_dir_all(&sessions).ok();
}

/// A directory named in the legacy id spelling is reachable: the sid prints as
/// a UUID, so a reader that rebuilds the directory from the id finds nothing.
/// The scan carries both the path and the descriptor, so the pick does not depend
/// on the name being the id's display form.
#[test]
fn test_candidates_read_legacy() {
    const LEGACY: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
    let sessions = temp_root();
    let dir = write_session_at(&sessions, LEGACY, SessionProvenance::Fresh);
    let sid = SessionId::from_display_string(LEGACY).unwrap();
    assert_ne!(
        dir,
        sessions.join(sid.to_string()),
        "the fixture is only meaningful while the two spellings differ"
    );
    assert_eq!(
        resume_candidates(&sessions),
        vec![sid],
        "the legacy-named session is still the workspace's latest"
    );
    remove_dir_all(&sessions).ok();
}

/// Candidates arrive newest first, so --continue takes the most recent free
/// session and falls back through the list in activity order.
#[test]
fn test_candidates_newest_first() {
    let sessions = temp_root();
    let older = SessionId::new();
    let newer = SessionId::new();
    let older_dir = write_session_at(&sessions, &older.to_string(), SessionProvenance::Fresh);
    let newer_dir = write_session_at(&sessions, &newer.to_string(), SessionProvenance::Fresh);
    age_log(&older_dir.join("log.jsonl"), 100);
    age_log(&newer_dir.join("log.jsonl"), 10);
    assert_eq!(
        resume_candidates(&sessions),
        vec![newer, older],
        "the newest activity is first"
    );
    remove_dir_all(&sessions).ok();
}

/// The listing reads only the most recently active sessions before falling
/// back to a full scan. A workspace crowded out of that window must still
/// continue in activity order, so the fallback ranks its matches the same
/// way the window does.
#[test]
fn test_fallback_keeps_order() {
    let sessions = temp_root();
    let older = SessionId::new();
    let newer = SessionId::new();
    let older_dir = write_session_at(&sessions, &older.to_string(), SessionProvenance::Fresh);
    let newer_dir = write_session_at(&sessions, &newer.to_string(), SessionProvenance::Fresh);
    // Fill the window with newer sessions recorded in another workspace, so
    // the two above are reachable only through the fallback scan.
    for _ in 0..RECENT_WINDOW {
        let sid = SessionId::new();
        write_session_in(
            &sessions,
            &sid.to_string(),
            SessionProvenance::Fresh,
            "/elsewhere/repo",
        );
    }
    age_log(&older_dir.join("log.jsonl"), 100);
    age_log(&newer_dir.join("log.jsonl"), 10);
    assert_eq!(
        resume_candidates(&sessions),
        vec![newer, older],
        "the fallback keeps the newest activity first"
    );
    remove_dir_all(&sessions).ok();
}

/// The full scan reaches a workspace session the bounded listing stops short
/// of, in the same activity order. --continue falls back to it when every
/// session the bounded listing returned is held, so a free session outside
/// the window still continues instead of the command refusing to start.
#[test]
fn test_all_candidates_span_window() {
    let sessions = temp_root();
    let older = SessionId::new();
    let newer = SessionId::new();
    let older_dir = write_session_at(&sessions, &older.to_string(), SessionProvenance::Fresh);
    let newer_dir = write_session_at(&sessions, &newer.to_string(), SessionProvenance::Fresh);
    // Fill the window with sessions from another workspace, then order the
    // whole store so the bounded listing holds the newer session of this
    // workspace only.
    let mut fillers = Vec::new();
    for _ in 0..RECENT_WINDOW {
        let sid = SessionId::new();
        fillers.push(write_session_in(
            &sessions,
            &sid.to_string(),
            SessionProvenance::Fresh,
            "/elsewhere/repo",
        ));
    }
    for dir in &fillers {
        age_log(&dir.join("log.jsonl"), 100);
    }
    age_log(&older_dir.join("log.jsonl"), 200);
    age_log(&newer_dir.join("log.jsonl"), 10);
    assert_eq!(
        resume_candidates(&sessions),
        vec![newer],
        "the bounded listing holds only the session inside its window"
    );
    assert_eq!(
        resume_candidates_all(&sessions),
        vec![newer, older],
        "the full scan reaches past the window in the same order"
    );
    remove_dir_all(&sessions).ok();
}

/// Move a log's modification time into the past, so activity order is visible
/// at the whole-second granularity the listing compares.
fn age_log(path: &Path, secs_ago: u64) {
    let t = SystemTime::now() - Duration::from_secs(secs_ago);
    // Opened for write: windows stamps times through a handle carrying the
    // write-attributes right.
    OpenOptions::new()
        .write(true)
        .open(path)
        .expect("open for set_times")
        .set_times(FileTimes::new().set_modified(t))
        .expect("set mtime");
}

/// Continuing the session above opens it: the store resolves the id to the
/// directory the ulid spelling names, so a session that was picked is also
/// readable. Rebuilding the path from the id alone would fail the precheck
/// and turn a listed session into an error.
#[test]
fn test_resume_reads_legacy_dir() {
    const LEGACY: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
    let sessions = temp_root();
    write_session_at(&sessions, LEGACY, SessionProvenance::Fresh);
    let sid = SessionId::from_display_string(LEGACY).unwrap();
    let resumed =
        build_runner_for_resume_sid(sid, &sessions, None, None, ProviderResolution::stub())
            .expect("a legacy-named directory is a session the store can open");
    assert_eq!(resumed.assembled.session, sid);
    let rt = Builder::new_current_thread().enable_all().build().unwrap();
    let events = rt.block_on(async {
        resumed
            .assembled
            .runner
            .store()
            .replay(sid)
            .await
            .expect("the legacy directory's log replays")
    });
    assert_eq!(events.len(), 1, "the log written there is the one read");
    remove_dir_all(&sessions).ok();
}
