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

use nix::errno::Errno;
use nix::sys::signal::kill;
use nix::unistd::Pid;
use std::env::temp_dir;
use std::env::var_os;
use std::fs::create_dir_all;
use std::fs::read_dir;
use std::fs::read_to_string;
use std::fs::remove_file;
use std::fs::write;
use std::path::PathBuf;
use std::process::id;

/// The conventional per-user directory where detached-session sockets and
/// pidfiles live. Created on first use; never silently uses a shared world
/// location.
pub fn sessions_dir() -> PathBuf {
    let base = var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .or_else(|| var_os("HOME").map(PathBuf::from))
        .unwrap_or_else(temp_dir);
    let dir = base.join("houyicoder").join("sessions");
    create_dir_all(&dir).ok();
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
    write(session_pidfile(session_id), id().to_string()).ok();
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
    match kill(Pid::from_raw(pid), None) {
        Ok(()) => true,
        Err(Errno::EPERM) => true,
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
    let Ok(entries) = read_dir(&dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(id) = name.strip_suffix(".sock") else {
            continue;
        };
        let pid = read_to_string(session_pidfile(id))
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
    let pid = read_to_string(session_pidfile(session_id))
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
        drop(remove_file(session_socket(id)));
        drop(remove_file(session_pidfile(id)));
        reaped += 1;
    }
    reaped
}

#[cfg(test)]
mod tests;
