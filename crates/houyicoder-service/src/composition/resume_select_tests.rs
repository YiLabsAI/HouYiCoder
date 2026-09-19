//! Which session --continue selects, and the directory spelling it resolves
//! from. Split from the resume tests on size and subject grounds: this half is
//! about choosing a session out of a store, the other about building a runner
//! out of one.

use super::super::{build_runner_for_resume_sid, latest_session_sid};
use super::*;
use crate::composition::workspace_cwd;

/// Write a session directly on disk: a log with one event plus a descriptor whose
/// cwd is the current workspace, under the given directory name. Returns the
/// directory. Used where the directory name is the point of the test.
fn write_session_at(
    sessions: &std::path::Path,
    name: &str,
    provenance: SessionProvenance,
) -> std::path::PathBuf {
    let dir = sessions.join(name);
    std::fs::create_dir_all(&dir).unwrap();
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
    std::fs::write(
        dir.join("log.jsonl"),
        format!("{}\n", serde_json::to_string(&entry).unwrap()),
    )
    .unwrap();
    let descriptor = SessionDescriptor {
        name: None,
        name_source: NameSource::Auto,
        cwd: workspace_cwd(None),
        model: "m".into(),
        provenance,
        version: "t".into(),
        created_at: 1,
        child_session_ids: Vec::new(),
    };
    std::fs::write(
        dir.join("session.json"),
        serde_json::to_vec(&descriptor).unwrap(),
    )
    .unwrap();
    dir
}

/// A sub-agent session is the newest directory in the workspace, and it is not
/// a continuation: --continue must find nothing rather than land in the child.
#[test]
fn test_latest_skips_child() {
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
        latest_session_sid(&sessions).is_none(),
        "a child is a sidechain of a parent, not something to continue"
    );
    std::fs::remove_dir_all(&sessions).ok();
}

/// A directory named in the legacy id spelling is reachable: the sid prints as
/// a UUID, so a reader that rebuilds the directory from the id finds nothing.
/// The scan carries both the path and the descriptor, so the pick does not depend
/// on the name being the id's display form.
#[test]
fn test_latest_reads_legacy_name() {
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
        latest_session_sid(&sessions),
        Some(sid),
        "the legacy-named session is still the workspace's latest"
    );
    std::fs::remove_dir_all(&sessions).ok();
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
    let resumed = build_runner_for_resume_sid(sid, &sessions, None, None, ResolvedProvider::stub())
        .expect("a legacy-named directory is a session the store can open");
    assert_eq!(resumed.assembled.session, sid);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
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
    std::fs::remove_dir_all(&sessions).ok();
}
