use super::*;
use houyicoder_context::session_class::{DESCRIPTOR_FILE, LOG_FILE};
use houyicoder_context::{
    EventId, SessionDescriptor, SessionEvent, SessionLogEntry, SessionProvenance,
};
use houyicoder_service::lifecycle::prune::{PruneAction, PruneEntry, PruneReason};
use std::path::PathBuf;

fn temp_root() -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let p = temp_dir().join(format!("housekeeping-{n}-{}", id()));
    let _r = remove_dir_all(&p);
    create_dir_all(&p).unwrap();
    p
}

/// A one-entry plan removing a session directory. with_lock controls
/// whether the directory already carries a session.lock: a session no
/// live process has ever opened does not have one, which is the case the
/// delete-time acquire has to create.
fn plan_with_session(dir: &Path, with_lock: bool) -> PrunePlan {
    create_dir_all(dir).unwrap();
    write(dir.join("log.jsonl"), b"[]").unwrap();
    if with_lock {
        write(dir.join("session.lock"), b"").unwrap();
    }
    PrunePlan {
        entries: vec![PruneEntry {
            path: dir.to_path_buf(),
            kind: PruneKind::Session,
            reason: PruneReason::Ttl,
            last_active: 0,
            action: PruneAction::RemoveDir,
        }],
        kept: 0,
    }
}

/// A session nobody holds is deleted and reported as removed. It starts
/// without a session.lock, so this also covers the acquire creating one
/// on a directory that has never been opened by a live process.
#[test]
fn test_apply_removes_free_session() {
    let root = temp_root();
    let sid_dir = root.join("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa");
    let plan = plan_with_session(&sid_dir, false);
    let (report, skipped) = apply_prune_locked(&plan);
    assert_eq!(report.removed, 1, "free session removed");
    assert_eq!(skipped, 0, "nothing skipped");
    assert!(!sid_dir.exists(), "the directory is gone");
    let _r = remove_dir_all(&root);
}

/// A session whose lock a live process holds survives, and the skip is
/// counted rather than dropped. Fail-closed: protect, do not delete.
#[test]
fn test_apply_spares_held_session() {
    let root = temp_root();
    let sid_dir = root.join("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb");
    let plan = plan_with_session(&sid_dir, true);
    // Stand in for a live process holding the session open.
    let _external = Flock::lock(
        OpenOptions::new()
            .write(true)
            .open(sid_dir.join("session.lock"))
            .unwrap(),
        FlockArg::LockExclusiveNonblock,
    )
    .unwrap();
    let (report, skipped) = apply_prune_locked(&plan);
    assert_eq!(report.removed, 0, "a held session is not deleted");
    assert_eq!(skipped, 1, "the skip is counted, not silently dropped");
    assert!(sid_dir.exists(), "the held session survives");
    let _r = remove_dir_all(&root);
}

/// The property the whole lock exists for: while the prune holds a
/// session, a resume of that same session cannot start. Driven through
/// the real SessionLock the resume path uses, not an imitation of it, so
/// the two sides are proven to contend rather than assumed to.
///
/// The session starts with no session.lock, which is the ordinary state
/// for an old session in a plan -- the file only appears once a process
/// has opened the session. An acquire that skipped an absent lock file
/// left this case entirely unprotected: the resume created its own lock,
/// succeeded, and the delete landed on a live session.
#[test]
fn test_delete_lock_blocks_resume() {
    let root = temp_root();
    let sid_str = "cccccccc-cccc-cccc-cccc-cccccccccccc";
    let sid_dir = root.join(sid_str);
    create_dir_all(&sid_dir).unwrap();
    write(sid_dir.join("log.jsonl"), b"[]").unwrap();
    assert!(
        !sid_dir.join("session.lock").exists(),
        "the fixture must start with no lock file for this to mean anything"
    );
    let guard = acquire_delete_lock(&sid_dir).expect("prune acquires the delete lock");
    let resumed = SessionLock::acquire(sid_str, &root);
    assert!(
        resumed.is_err(),
        "a resume started the session the prune is about to delete"
    );
    drop(guard);
    // Once the prune releases, a resume is free to take the session again.
    assert!(
        SessionLock::acquire(sid_str, &root).is_ok(),
        "the lock must not outlive the delete it guards"
    );
    let _r = remove_dir_all(&root);
}

/// The sweep calls the descriptor repair over the store it sweeps: a
/// log-only child holds its descriptor when the sweep returns. The pass
/// itself is tested where it lives; this covers the call site, which no
/// unit test of the pass can see.
#[test]
fn test_sweep_repairs_child_descriptor() {
    let home = temp_root();
    let sessions = temp_root();
    let sid = SessionId::new();
    let dir = sessions.join(sid.to_string());
    create_dir_all(&dir).unwrap();
    let head = SessionLogEntry {
        id: EventId::new(),
        session: sid,
        ts: 1_700_000_042_123,
        prev_hash: None,
        event: SessionEvent::ChildDelegated {
            parent_session_id: "parent-1".to_string(),
            subagent_type: "explore".to_string(),
        },
    };
    let line = serde_json::to_string(&head).unwrap();
    write(dir.join(LOG_FILE), format!("{line}\n")).unwrap();

    run_once(
        &home,
        &sessions,
        &home.join("shell-snapshots"),
        None,
        &SessionId::new(),
    );

    let body = read(dir.join(DESCRIPTOR_FILE))
        .expect("the sweep wrote the descriptor the child's log implies");
    let descriptor: SessionDescriptor = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        descriptor.provenance,
        SessionProvenance::SpawnedBy {
            parent_session_id: "parent-1".to_string(),
            subagent_type: "explore".to_string(),
            task_id: sid.to_string(),
        },
        "the descriptor names the delegation the log opens with"
    );
    assert_eq!(
        descriptor.created_at, 1_700_000_042,
        "created at the second the delegation was recorded"
    );
    let _r = remove_dir_all(&home);
    let _r = remove_dir_all(&sessions);
}
