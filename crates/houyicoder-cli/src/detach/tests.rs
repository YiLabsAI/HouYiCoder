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

/// list_sessions reads back a session id we wrote a socket + pidfile for,
/// including the pid.
#[test]
fn test_list_sessions_reads_pid() {
    let base = isolated_base();
    let session = format!("test-list-{}", id());
    let sock = session_socket(&session);
    assert!(
        sock.starts_with(&base),
        "the socket lands under the injected base, not the real home: {sock:?}"
    );
    write(&sock, b"").expect("write test socket");
    write(session_pidfile(&session), "4242\n").expect("write test pid");
    let sessions = list_sessions();
    let found = sessions.iter().find(|s| s.id == session);
    let found = found.expect("list_sessions must find the written id");
    assert_eq!(found.pid, Some(4242), "pid must be read from the pidfile");
    remove_file(&sock).ok();
    remove_file(session_pidfile(&session)).ok();
}

/// list_sessions omits the pid when the pidfile is absent (a predecessor
/// crashed without writing one, or the socket is stale).
#[test]
fn test_list_sessions_pid_optional() {
    let _base = isolated_base();
    let session = format!("test-nopid-{}", id());
    let sock = session_socket(&session);
    write(&sock, b"").expect("write test socket");
    drop(remove_file(session_pidfile(&session)));
    let sessions = list_sessions();
    let found = sessions
        .iter()
        .find(|s| s.id == session)
        .expect("list_sessions must find the written id");
    assert_eq!(found.pid, None, "no pidfile means no pid");
    assert!(!found.live, "no pid means not live");
    remove_file(&sock).ok();
}

/// pid_alive: the current process is alive; a pid certain to not exist
/// (a high sentinel) is not.
#[test]
fn test_pid_alive_self_dead() {
    let self_pid = id() as i32;
    assert!(pid_alive(self_pid), "the current process is alive");
    // A sentinel pid certain to not exist. PIDs are positive; pick one
    // in a range the OS is vanishingly unlikely to have allocated, and
    // accept ESRCH (no such pid) as the not-alive verdict.
    let sentinel = 1_000_000_000;
    assert!(!pid_alive(sentinel), "a non-existent pid is not alive");
}

/// is_session_live: a session whose pidfile points at the current
/// process is live; a session whose pidfile points at a dead pid is not.
#[test]
fn test_session_live_checks_pid() {
    let _base = isolated_base();
    let session = format!("test-live-{}", id());
    let sock = session_socket(&session);
    write(&sock, b"").expect("write test socket");
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
    remove_file(&sock).ok();
    remove_file(session_pidfile(&session)).ok();
}

/// cleanup_stale removes the socket + pidfile for a session whose pid is
/// dead, leaving live sessions alone.
#[test]
fn test_cleanup_stale_reaps_dead() {
    let _base = isolated_base();
    let live_session = format!("test-cleanup-live-{}", id());
    let dead_session = format!("test-cleanup-dead-{}", id());
    write(session_socket(&live_session), b"").expect("write live sock");
    write(session_pidfile(&live_session), id().to_string()).expect("write live pid");
    write(session_socket(&dead_session), b"").expect("write dead sock");
    write(session_pidfile(&dead_session), "1000000000").expect("write dead pid");
    let reaped = cleanup_stale();
    assert!(reaped >= 1, "the dead session must be reaped");
    assert!(
        is_session_live(&live_session),
        "the live session is untouched"
    );
    assert!(
        !session_socket(&dead_session).exists(),
        "the dead session socket is removed"
    );
    assert!(
        !session_pidfile(&dead_session).exists(),
        "the dead session pidfile is removed"
    );
    remove_file(session_socket(&live_session)).ok();
    remove_file(session_pidfile(&live_session)).ok();
}

/// The override names the base the sockets land under: with it set, every
/// socket path starts inside the injected directory, so the suite never
/// reads or reaps the user's real sessions even when a test forgets its
/// own cleanup.
#[test]
fn test_runtime_dir_redirects() {
    let base = isolated_base();
    let session = format!("test-redirect-{}", id());
    let sock = session_socket(&session);
    write(&sock, b"").expect("write test socket");
    assert!(
        sock.starts_with(&base),
        "the socket lands under the injected base, not the real home: {sock:?}"
    );
    remove_file(&sock).ok();
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
