//! Detached-session discovery. A background agent run with --detached binds a
//! local endpoint derived from the session id and records it in a sidecar file
//! beside the pidfile, so a listing can enumerate live sessions and an
//! attaching client can find the endpoint of the session it was given. The
//! conventional dir sits under the per-user runtime dir when that is set, and
//! falls back to a sessions subdir of the user home.

use std::env::temp_dir;
use std::env::var_os;
use std::fs::read_dir;
use std::fs::read_to_string;
use std::fs::remove_file;
use std::fs::write;
use std::io::Result;
use std::path::PathBuf;
use std::process::id;

use houyicoder_api::path::home_directory;
use houyicoder_platform::ipc;
use houyicoder_platform::process;

/// The environment variable that overrides where detached-session endpoints
/// and pidfiles land. Tests set it to a per-process temp directory so the
/// suite never reads or reaps the user's real sessions.
const ENV_HOUYICODER_RUNTIME_DIR: &str = "HOUYICODER_RUNTIME_DIR";

/// The conventional per-user directory where detached-session endpoints and
/// pidfiles live. Created owner-only on first use; never silently uses a
/// shared world location.
///
/// The runtime override sits above the standard per-user runtime dir so a
/// test can point every endpoint and pidfile at its own temp directory: the
/// tests that drive cleanup and listing must not touch the real home, where
/// a cleanup pass would delete files the user placed there.
///
/// The home fallback joins the dotted configuration directory the rest of
/// the per-user state uses, so one prefix names every home-level path. The
/// undotted path this replaced never held a session and no build binds an
/// endpoint there any more, so no fallback reads it.
pub fn sessions_dir() -> PathBuf {
    let base = var_os(ENV_HOUYICODER_RUNTIME_DIR)
        .or_else(|| var_os("XDG_RUNTIME_DIR"))
        .map(PathBuf::from);
    let dir = match base {
        Some(runtime) => runtime.join("houyicoder").join("sessions"),
        None => home_directory()
            .unwrap_or_else(temp_dir)
            .join(".houyicoder")
            .join("sessions"),
    };
    drop(ipc::create_owner_only_dir(&dir));
    dir
}

/// The sidecar file that records the endpoint a session listens on. The
/// endpoint itself is a socket file on Unix and a pipe name on Windows, so the
/// record is what lets both platforms share one discovery rule.
pub(crate) fn endpoint_file(session_id: &str) -> PathBuf {
    sessions_dir().join(format!("{session_id}.endpoint"))
}

/// The pidfile path for a session id under the conventional dir.
fn session_pidfile(session_id: &str) -> PathBuf {
    sessions_dir().join(format!("{session_id}.pid"))
}

/// Record the endpoint a session has already bound, so a listing can discover
/// it and an attaching client can reach it. The caller binds first: a record
/// written for an endpoint nobody listens on makes a session that failed to
/// listen look attachable.
pub fn record_endpoint(session_id: &str, endpoint: &str) -> Result<()> {
    write(endpoint_file(session_id), endpoint)
}

/// The recorded endpoint of a session, when the record exists and names one.
pub fn read_endpoint(session_id: &str) -> Option<String> {
    let endpoint = read_to_string(endpoint_file(session_id)).ok()?;
    let endpoint = endpoint.trim();
    (!endpoint.is_empty()).then(|| endpoint.to_string())
}

/// Write the current process pid into the session pidfile so a later listing
/// can report it. Best-effort: a write failure only means the listing omits
/// the pid for this session; the endpoint still works.
pub fn write_pidfile(session_id: &str) {
    write(session_pidfile(session_id), id().to_string()).ok();
}

/// A discovered detached session: its id, the pid its pidfile recorded (if
/// any), and whether that pid is still alive. An endpoint record present means
/// the agent bound (or a predecessor crashed without cleanup); the pid is read
/// from the sidecar pidfile so a user can terminate the session with a plain
/// kill. The live flag is false when the pid is dead (a crashed session) so
/// the listing can flag stale entries and cleanup_stale can reap them.
pub struct DiscoveredSession {
    pub id: String,
    pub pid: Option<i32>,
    pub live: bool,
}

/// List the detached sessions exposed under the conventional dir, each with
/// the pid its pidfile recorded (if any) and whether that pid is alive. Sorted
/// by id. A session whose pid is dead is still listed (so the listing can show
/// it as stale); cleanup_stale reaps them.
pub fn list_sessions() -> Vec<DiscoveredSession> {
    let dir = sessions_dir();
    let mut out = Vec::new();
    let Ok(entries) = read_dir(&dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(id) = name.strip_suffix(".endpoint") else {
            continue;
        };
        let pid = read_to_string(session_pidfile(id))
            .ok()
            .and_then(|s| s.trim().parse().ok());
        let live = pid.map(process::alive).unwrap_or(false);
        out.push(DiscoveredSession {
            id: id.to_string(),
            pid,
            live,
        });
    }
    out.sort_by_key(|s| s.id.clone());
    out
}

/// True when a session id has a live process holding it: the pidfile exists
/// and the pid is alive. The resume path uses this to refuse resuming a
/// session a live process is still writing (the hash chain needs a single
/// writer) and point at attach or fork-session instead. The answer means the
/// recorded identifier is in use, not that the process which recorded it is
/// still the holder: a reused identifier reads alive.
pub fn is_session_live(session_id: &str) -> bool {
    let pid = read_to_string(session_pidfile(session_id))
        .ok()
        .and_then(|s| s.trim().parse::<i32>().ok());
    pid.map(process::alive).unwrap_or(false)
}

/// Reap stale entries: remove the endpoint, its record, and the pidfile for
/// sessions whose pid is dead (a predecessor crashed without cleanup).
/// Best-effort: a remove failure is swallowed (a read-only runtime dir leaves
/// the stale entry, which the listing then shows as not-live). Returns the
/// count reaped.
pub fn cleanup_stale() -> usize {
    let mut reaped = 0;
    for s in list_sessions() {
        if s.live {
            continue;
        }
        if let Some(endpoint) = read_endpoint(&s.id) {
            drop(ipc::remove_endpoint(&endpoint));
        }
        drop(remove_file(endpoint_file(&s.id)));
        drop(remove_file(session_pidfile(&s.id)));
        reaped += 1;
    }
    reaped
}

#[cfg(test)]
mod tests;
