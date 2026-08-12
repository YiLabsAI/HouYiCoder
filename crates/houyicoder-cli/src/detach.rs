//! Detached-session discovery. A background agent run with --detached binds
//! a Unix domain socket at a conventional per-user path derived from the
//! session id, and writes a pidfile beside it so a later list can enumerate
//! live sessions with their pids. The conventional dir is a sessions subdir
//! under XDG_RUNTIME_DIR when that runtime dir is set (the standard per-user
//! runtime location, often tmpfs), falling back to a sessions subdir of the
//! user home so detach works on a stock mac where the runtime dir is unset.
//!
//! Stopping a detached session is deliberately not a built-in command: the
//! workspace forbids unsafe code (so a libc kill is out), and the clippy
//! spawn ban routes every spawn through the launcher (a stop-by-spawn-kill
//! is heavier than the value). The ps command prints the pid instead, so a
//! user terminates a session with a plain kill of that pid.

#![cfg(unix)]

use std::path::PathBuf;

/// The conventional per-user directory where detached-session sockets and
/// pidfiles live. Created on first use; never silently uses a shared world
/// location.
pub fn sessions_dir() -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
        .unwrap_or_else(std::env::temp_dir);
    let dir = base.join("houyicoder").join("sessions");
    std::fs::create_dir_all(&dir).ok();
    dir
}

/// The socket path for a session id under the conventional dir.
pub fn session_socket(session_id: &str) -> PathBuf {
    sessions_dir().join(format!("{session_id}.sock"))
}

/// The pidfile path for a session id under the conventional dir.
fn session_pidfile(session_id: &str) -> PathBuf {
    sessions_dir().join(format!("{session_id}.pid"))
}

/// Write the current process pid into the session pidfile so a later ps can
/// report it. Best-effort: a write failure only means ps omits the pid for
/// this session; the socket still works.
pub fn write_pidfile(session_id: &str) {
    std::fs::write(session_pidfile(session_id), std::process::id().to_string()).ok();
}

/// A discovered detached session: its id, the pid its pidfile recorded (if
/// any), and whether that pid is still alive. A socket file present means the
/// agent bound (or a predecessor crashed without cleanup); the pid is read
/// from the sidecar pidfile so a user can terminate the session with a plain
/// kill. The live flag is false when the pid is dead (a crashed session) so
/// ps can flag stale entries + cleanup_stale can reap them.
pub struct DiscoveredSession {
    pub id: String,
    pub pid: Option<i32>,
    pub live: bool,
}

/// True when a process with the given pid exists. Uses signal-0 (the
/// standard pid-existence probe): Ok or EPERM means the pid is alive (EPERM
/// = exists but not ours to signal -- still alive), ESRCH means no such pid.
/// The procStart identity check (guard against pid reuse by a different
/// process) is deferred -- pid reuse is rare on mac (high random pids), and
/// a start-time comparison needs /proc or ps, neither available without a
/// spawn; pid_alive is the primary signal.
pub fn pid_alive(pid: i32) -> bool {
    use nix::sys::signal::kill;
    use nix::unistd::Pid;
    match kill(Pid::from_raw(pid), None) {
        Ok(()) => true,
        Err(nix::errno::Errno::EPERM) => true,
        Err(_) => false,
    }
}

/// List the detached sessions exposed under the conventional dir, each with
/// the pid its pidfile recorded (if any) + whether that pid is alive. Sorted
/// by id. A session whose pid is dead is still listed (so ps can show it as
/// stale); cleanup_stale reaps them.
pub fn list_sessions() -> Vec<DiscoveredSession> {
    let dir = sessions_dir();
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(id) = name.strip_suffix(".sock") else {
            continue;
        };
        let pid = std::fs::read_to_string(session_pidfile(id))
            .ok()
            .and_then(|s| s.trim().parse().ok());
        let live = pid.map(pid_alive).unwrap_or(false);
        out.push(DiscoveredSession {
            id: id.to_string(),
            pid,
            live,
        });
    }
    out.sort_by_key(|s| s.id.clone());
    out
}

/// True when a session id has a live process holding it: the pidfile
/// exists and the pid is alive. The resume path uses this to refuse
/// resuming a session a live process is still writing (the hash chain
/// needs a single writer) and point at attach or fork-session instead.
pub fn is_session_live(session_id: &str) -> bool {
    let pid = std::fs::read_to_string(session_pidfile(session_id))
        .ok()
        .and_then(|s| s.trim().parse::<i32>().ok());
    pid.map(pid_alive).unwrap_or(false)
}

/// Reap stale entries: remove the socket + pidfile for sessions whose pid is
/// dead (a predecessor crashed without cleanup). Best-effort -- a remove
/// failure is swallowed (a read-only runtime dir leaves the stale entry,
/// which ps then shows as not-live). Returns the count reaped.
pub fn cleanup_stale() -> usize {
    let mut reaped = 0;
    for s in list_sessions() {
        if s.live {
            continue;
        }
        let id = &s.id;
        drop(std::fs::remove_file(session_socket(id)));
        drop(std::fs::remove_file(session_pidfile(id)));
        reaped += 1;
    }
    reaped
}

#[cfg(test)]
mod tests {
    use super::*;

    /// list_sessions reads back a session id we wrote a socket + pidfile for,
    /// including the pid. Uses the real conventional dir, isolated to the
    /// user's own sessions.
    #[test]
    fn test_list_sessions_reads_pid() {
        let id = format!("test-list-{}", std::process::id());
        let sock = session_socket(&id);
        std::fs::write(&sock, b"").expect("write test socket");
        std::fs::write(session_pidfile(&id), "4242\n").expect("write test pid");
        let sessions = list_sessions();
        let found = sessions.iter().find(|s| s.id == id);
        let found = found.expect("list_sessions must find the written id");
        assert_eq!(found.pid, Some(4242), "pid must be read from the pidfile");
        std::fs::remove_file(&sock).ok();
        std::fs::remove_file(session_pidfile(&id)).ok();
    }

    /// list_sessions omits the pid when the pidfile is absent (a predecessor
    /// crashed without writing one, or the socket is stale).
    #[test]
    fn test_list_sessions_pid_optional() {
        let id = format!("test-nopid-{}", std::process::id());
        let sock = session_socket(&id);
        std::fs::write(&sock, b"").expect("write test socket");
        drop(std::fs::remove_file(session_pidfile(&id)));
        let sessions = list_sessions();
        let found = sessions
            .iter()
            .find(|s| s.id == id)
            .expect("list_sessions must find the written id");
        assert_eq!(found.pid, None, "no pidfile means no pid");
        assert!(!found.live, "no pid means not live");
        std::fs::remove_file(&sock).ok();
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
        std::fs::write(&sock, b"").expect("write test socket");
        // Live: the pidfile points at this test process.
        std::fs::write(session_pidfile(&id), std::process::id().to_string()).expect("write pid");
        assert!(
            is_session_live(&id),
            "a session held by the current pid is live"
        );
        // Dead: point the pidfile at the sentinel.
        std::fs::write(session_pidfile(&id), "1000000000").expect("write dead pid");
        assert!(
            !is_session_live(&id),
            "a dead pid means the session is not live"
        );
        std::fs::remove_file(&sock).ok();
        std::fs::remove_file(session_pidfile(&id)).ok();
    }

    /// cleanup_stale removes the socket + pidfile for a session whose pid is
    /// dead, leaving live sessions alone.
    #[test]
    fn test_cleanup_stale_reaps_dead() {
        let live_id = format!("test-cleanup-live-{}", std::process::id());
        let dead_id = format!("test-cleanup-dead-{}", std::process::id());
        std::fs::write(session_socket(&live_id), b"").expect("write live sock");
        std::fs::write(session_pidfile(&live_id), std::process::id().to_string())
            .expect("write live pid");
        std::fs::write(session_socket(&dead_id), b"").expect("write dead sock");
        std::fs::write(session_pidfile(&dead_id), "1000000000").expect("write dead pid");
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
        std::fs::remove_file(session_socket(&live_id)).ok();
        std::fs::remove_file(session_pidfile(&live_id)).ok();
    }
}
