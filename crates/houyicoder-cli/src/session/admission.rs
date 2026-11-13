//! Which existing session this process may open. Availability is a probe of
//! the session lock over a candidate list; admission takes that lock for real
//! and owns the refusal copy for a session this process cannot hold.

use std::error::Error;
use std::path::Path;

use houyicoder_context::SessionId;

use crate::detach;
use crate::session::lock::LockError;
use crate::session::lock::LockProbe;
use crate::session::lock::SessionLock;
use crate::session::lock::probe_lock_state;

/// The first candidate no process holds, with the ones skipped for being
/// held or unreadable. This is a probe of the session lock, and admission
/// takes that lock for real, so this decides which session to try rather
/// than guaranteeing it opens.
pub(crate) fn select_free(
    candidates: Vec<SessionId>,
    sessions_root: &Path,
) -> (Option<SessionId>, Vec<SessionId>) {
    let mut skipped = Vec::new();
    for sid in candidates {
        if probe_lock_state(sid, sessions_root) == LockProbe::Free {
            return (Some(sid), skipped);
        }
        skipped.push(sid);
    }
    (None, skipped)
}

/// The session --continue opens: the newest free candidate, else the newest
/// free session a whole-store rescan finds. A bounded listing whose sessions
/// are all unavailable is not a reason to refuse while an older session is
/// free, so the rescan runs only in that case.
pub(crate) fn continue_target(
    candidates: Vec<SessionId>,
    sessions_root: &Path,
    rescan: impl FnOnce() -> Vec<SessionId>,
) -> (Option<SessionId>, Vec<SessionId>) {
    let (selected, skipped) = select_free(candidates, sessions_root);
    match selected {
        Some(sid) => (Some(sid), skipped),
        None => select_free(rescan(), sessions_root),
    }
}

/// How many per-session refusal lines print before the rest are counted in
/// one summary line. A store whose locks all became unreadable is bounded by
/// neither live processes nor the picker's listing.
const REFUSAL_LINE_LIMIT: usize = 10;

/// One refusal line per session that cannot be opened here, saying what the
/// user can do about that particular holder, capped so a large store does
/// not scroll the terminal before the interface starts. The lines differ
/// because the holders differ: a detached process has a local endpoint to
/// attach to, a lock held by some other process has no endpoint at all, and a
/// lock that cannot be read names no holder.
pub(crate) fn unavailable_lines(sids: &[SessionId], sessions_root: &Path) -> Vec<String> {
    let mut lines: Vec<String> = sids
        .iter()
        .take(REFUSAL_LINE_LIMIT)
        .map(|sid| unavailable_line(sid, sessions_root))
        .collect();
    if sids.len() > REFUSAL_LINE_LIMIT {
        lines.push(format!(
            "and {} more sessions, all unavailable",
            sids.len() - REFUSAL_LINE_LIMIT
        ));
    }
    lines
}

fn unavailable_line(sid: &SessionId, sessions_root: &Path) -> String {
    let sid_str = sid.to_string();
    if detach::is_session_live(&sid_str) {
        if let Some(endpoint) = detach::read_endpoint(&sid_str) {
            return attach_hint(&sid_str, &endpoint);
        }
        return format!(
            "{sid_str} is live in another process; close that process, or {}",
            fork_hint(&sid_str)
        );
    }
    match probe_lock_state(*sid, sessions_root) {
        LockProbe::Held => format!(
            "{sid_str} is open in another process; close that process, or {}",
            fork_hint(&sid_str)
        ),
        LockProbe::Unreadable => format!(
            "{sid_str} has a lock file that could not be read; close any \
             process using it, or {}",
            fork_hint(&sid_str)
        ),
        // The holder released between the selection and this line; the next
        // attempt finds the session free.
        LockProbe::Free => format!("{sid_str} is free now; try again"),
    }
}

/// The command that opens a copy of this session under a new id. It names the
/// session, unlike --continue --fork-session, which branches from the newest
/// session of the workspace and would copy work the user was not refused.
fn fork_hint(sid_str: &str) -> String {
    format!("branch with houyi --resume {sid_str} --fork-session")
}

/// The line that names the endpoint a detached process listens on, spelled so
/// the command can be run as printed.
fn attach_hint(sid_str: &str, endpoint: &str) -> String {
    format!(
        "{sid_str} is live in another process; attach with \
         houyi attach {endpoint} {sid_str}, or {}",
        fork_hint(sid_str)
    )
}

/// Refuse a session id that is live elsewhere or already locked before any
/// writer or host for it is built. Returns the held lock on success. Every
/// existing-sid entry passes through this ahead of its build, so a held
/// session is refused before a writer or a host task is constructed. The live
/// check precedes the lock, so a live detached process gets the attach hint.
pub(crate) fn admit_existing_sid(
    sid: &SessionId,
    sessions_root: &Path,
) -> Result<SessionLock, Box<dyn Error>> {
    let sid_str = sid.to_string();
    if detach::is_session_live(&sid_str) {
        // A live pidfile means a detached process, whose endpoint the attach
        // needs; name it so the hint is a command the user can run.
        if let Some(endpoint) = detach::read_endpoint(&sid_str) {
            return Err(Box::<dyn Error>::from(attach_hint(&sid_str, &endpoint)));
        }
        return Err(Box::<dyn Error>::from(format!(
            "session {sid_str} is live in another process; close that process, or {}",
            fork_hint(&sid_str)
        )));
    }
    match SessionLock::acquire(&sid_str, sessions_root) {
        Ok(lock) => Ok(lock),
        // A holder inside this process is work this process started -- a host
        // that has not finished closing, or a background pass still reading
        // the session -- so the lock frees itself shortly. Telling the user to
        // attach would send them to a process that is going away.
        Err(LockError::Held(_)) if SessionLock::held_in_this_process(&sid_str, sessions_root) => {
            Err(Box::<dyn Error>::from(format!(
                "session {sid_str} is in use in this process; retry in a moment"
            )))
        }
        // A held lock with no live detached process belongs to a process with
        // no endpoint to attach to, which is a foreground terminal or a stdio
        // client. The refusal offers closing that process or branching from
        // this session instead of a raw enum.
        Err(LockError::Held(_)) => Err(Box::<dyn Error>::from(format!(
            "session {sid_str} is open in another process; close that \
             process, or {}",
            fork_hint(&sid_str)
        ))),
        // A lock file that cannot be opened or locked says nothing about a
        // holder. The refusal keeps the same honesty as the listing and
        // offers the fork instead of the raw io error; the error itself
        // stays in diagnostics.
        Err(e) => {
            tracing::warn!(sid = %sid_str, error = %e, "session lock could not be taken");
            Err(Box::<dyn Error>::from(unavailable_line(sid, sessions_root)))
        }
    }
}
