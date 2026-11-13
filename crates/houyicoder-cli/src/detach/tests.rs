use super::*;
use std::env::var_os;
use std::fs::remove_file;
use std::fs::write;
use std::process::id;

/// The runner injects the runtime dir variable pointing at a per-run temp
/// directory, so these tests write and reap inside that directory and the
/// user's real home is never the directory a cleanup pass scans. The
/// workspace denies unsafe, so the injection happens outside the process;
/// this helper only verifies the injection is present, so a test run
/// without the runner fails loudly instead of silently touching the home.
fn isolated_base() -> PathBuf {
    let base = var_os(ENV_HOUYICODER_RUNTIME_DIR)
        .expect("the runner injects the runtime dir before the tests run");
    PathBuf::from(base)
}

/// The listing reads back a session id we recorded an endpoint for, including
/// the pid.
#[test]
fn test_list_sessions_reads_pid() {
    let base = isolated_base();
    let session = format!("test-list-{}", id());
    let record = endpoint_file(&session);
    assert!(
        record.starts_with(&base),
        "the record lands under the injected base, not the real home: {record:?}"
    );
    write(&record, "staged-endpoint").expect("write the endpoint record");
    write(session_pidfile(&session), "4242\n").expect("write test pid");
    let sessions = list_sessions();
    let found = sessions.iter().find(|s| s.id == session);
    let found = found.expect("list_sessions must find the recorded id");
    assert_eq!(found.pid, Some(4242), "pid must be read from the pidfile");
    remove_file(&record).ok();
    remove_file(session_pidfile(&session)).ok();
}

/// The listing omits the pid when the pidfile is absent (a predecessor
/// crashed without writing one, or the record is stale).
#[test]
fn test_list_sessions_pid_optional() {
    let _base = isolated_base();
    let session = format!("test-nopid-{}", id());
    let record = endpoint_file(&session);
    write(&record, "staged-endpoint").expect("write the endpoint record");
    drop(remove_file(session_pidfile(&session)));
    let sessions = list_sessions();
    let found = sessions
        .iter()
        .find(|s| s.id == session)
        .expect("list_sessions must find the recorded id");
    assert_eq!(found.pid, None, "no pidfile means no pid");
    assert!(!found.live, "no pid means not live");
    remove_file(&record).ok();
}

/// A session whose pidfile points at the current process is live; a session
/// whose pidfile points at a dead pid is not.
#[test]
fn test_session_live_checks_pid() {
    let _base = isolated_base();
    let session = format!("test-live-{}", id());
    let record = endpoint_file(&session);
    write(&record, "staged-endpoint").expect("write the endpoint record");
    // Live: the pidfile points at this test process.
    write(session_pidfile(&session), id().to_string()).expect("write pid");
    assert!(
        is_session_live(&session),
        "a session held by the current pid is live"
    );
    // Dead: point the pidfile at the sentinel.
    write(session_pidfile(&session), "1000000000").expect("write dead pid");
    assert!(
        !is_session_live(&session),
        "a dead pid means the session is not live"
    );
    remove_file(&record).ok();
    remove_file(session_pidfile(&session)).ok();
}

/// cleanup_stale removes the endpoint, its record, and the pidfile for a
/// session whose pid is dead, leaving live sessions alone.
#[test]
fn test_cleanup_stale_reaps_dead() {
    let base = isolated_base();
    let live_session = format!("test-cleanup-live-{}", id());
    let dead_session = format!("test-cleanup-dead-{}", id());
    let dead_endpoint = base.join(format!("{dead_session}.sock"));
    #[cfg(unix)]
    write(&dead_endpoint, b"").expect("stage the dead endpoint file");
    write(endpoint_file(&live_session), "staged-endpoint").expect("write live record");
    write(session_pidfile(&live_session), id().to_string()).expect("write live pid");
    write(
        endpoint_file(&dead_session),
        dead_endpoint.to_string_lossy().as_bytes(),
    )
    .expect("write dead record");
    write(session_pidfile(&dead_session), "1000000000").expect("write dead pid");
    let reaped = cleanup_stale();
    assert!(reaped >= 1, "the dead session must be reaped");
    assert!(
        is_session_live(&live_session),
        "the live session is untouched"
    );
    assert!(
        !endpoint_file(&dead_session).exists(),
        "the dead session record is removed"
    );
    assert!(
        !session_pidfile(&dead_session).exists(),
        "the dead session pidfile is removed"
    );
    #[cfg(unix)]
    assert!(
        !dead_endpoint.exists(),
        "the dead session endpoint file is removed"
    );
    remove_file(endpoint_file(&live_session)).ok();
    remove_file(session_pidfile(&live_session)).ok();
}

/// The override names the base the records land under: with it set, every
/// endpoint record starts inside the injected directory, so the suite never
/// reads or reaps the user's real sessions even when a test forgets its own
/// cleanup.
#[test]
fn test_runtime_dir_redirects() {
    let base = isolated_base();
    let session = format!("test-redirect-{}", id());
    let record = endpoint_file(&session);
    write(&record, "staged-endpoint").expect("write the endpoint record");
    assert!(
        record.starts_with(&base),
        "the record lands under the injected base, not the real home: {record:?}"
    );
    remove_file(&record).ok();
}

/// The home fallback joins the dotted configuration directory, the prefix
/// every other home-level path uses. The undotted spelling this replaced
/// never held a session, so the home fallback must not name it; the
/// runtime fallback keeps the undotted join because a runtime dir is not
/// a home.
#[test]
fn test_home_fallback_is_dotted() {
    let source = include_str!("../detach.rs");
    assert!(
        source.contains(".join(\".houyicoder\")"),
        "the home fallback spells the dotted configuration directory"
    );
    assert!(
        !source.contains("or_else(temp_dir)\n            .join(\"houyicoder\")"),
        "the home fallback keeps no undotted join: {source:?}"
    );
}
