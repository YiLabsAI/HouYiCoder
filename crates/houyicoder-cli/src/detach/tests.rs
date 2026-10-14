use super::*;
use std::fs::remove_file;
use std::fs::write;

/// list_sessions reads back a session id we wrote a socket + pidfile for,
/// including the pid. Uses the real conventional dir, isolated to the
/// user's own sessions.
#[test]
fn test_list_sessions_reads_pid() {
    let id = format!("test-list-{}", std::process::id());
    let sock = session_socket(&id);
    write(&sock, b"").expect("write test socket");
    write(session_pidfile(&id), "4242\n").expect("write test pid");
    let sessions = list_sessions();
    let found = sessions.iter().find(|s| s.id == id);
    let found = found.expect("list_sessions must find the written id");
    assert_eq!(found.pid, Some(4242), "pid must be read from the pidfile");
    remove_file(&sock).ok();
    remove_file(session_pidfile(&id)).ok();
}

/// list_sessions omits the pid when the pidfile is absent (a predecessor
/// crashed without writing one, or the socket is stale).
#[test]
fn test_list_sessions_pid_optional() {
    let id = format!("test-nopid-{}", std::process::id());
    let sock = session_socket(&id);
    write(&sock, b"").expect("write test socket");
    drop(remove_file(session_pidfile(&id)));
    let sessions = list_sessions();
    let found = sessions
        .iter()
        .find(|s| s.id == id)
        .expect("list_sessions must find the written id");
    assert_eq!(found.pid, None, "no pidfile means no pid");
    assert!(!found.live, "no pid means not live");
    remove_file(&sock).ok();
}

/// pid_alive: the current process is alive; a pid certain to not exist
/// (a high sentinel) is not.
#[test]
fn test_pid_alive_self_dead() {
    let self_pid = std::process::id() as i32;
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
    let id = format!("test-live-{}", std::process::id());
    let sock = session_socket(&id);
    write(&sock, b"").expect("write test socket");
    // Live: the pidfile points at this test process.
    write(session_pidfile(&id), std::process::id().to_string()).expect("write pid");
    assert!(
        is_session_live(&id),
        "a session held by the current pid is live"
    );
    // Dead: point the pidfile at the sentinel.
    write(session_pidfile(&id), "1000000000").expect("write dead pid");
    assert!(
        !is_session_live(&id),
        "a dead pid means the session is not live"
    );
    remove_file(&sock).ok();
    remove_file(session_pidfile(&id)).ok();
}

/// cleanup_stale removes the socket + pidfile for a session whose pid is
/// dead, leaving live sessions alone.
#[test]
fn test_cleanup_stale_reaps_dead() {
    let live_id = format!("test-cleanup-live-{}", std::process::id());
    let dead_id = format!("test-cleanup-dead-{}", std::process::id());
    write(session_socket(&live_id), b"").expect("write live sock");
    write(session_pidfile(&live_id), std::process::id().to_string()).expect("write live pid");
    write(session_socket(&dead_id), b"").expect("write dead sock");
    write(session_pidfile(&dead_id), "1000000000").expect("write dead pid");
    let reaped = cleanup_stale();
    assert!(reaped >= 1, "the dead session must be reaped");
    assert!(is_session_live(&live_id), "the live session is untouched");
    assert!(
        !session_socket(&dead_id).exists(),
        "the dead session socket is removed"
    );
    assert!(
        !session_pidfile(&dead_id).exists(),
        "the dead session pidfile is removed"
    );
    remove_file(session_socket(&live_id)).ok();
    remove_file(session_pidfile(&live_id)).ok();
}
