//! What a directory in the sessions root is, answered once for the whole tree.
//!
//! Every reader of the store needs the same two answers about a directory:
//! whether it holds one of the user's sessions, and where a given session's
//! files live. Answered separately they diverge, so the number a user is told
//! can disagree with the rows they see, and a session can be listed without
//! being openable.

use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::{SessionDescriptor, SessionEvent, SessionId, SessionLogEntry, SessionProvenance};

/// The event log a session's directory holds.
pub const LOG_FILE: &str = "log.jsonl";
/// The sidecar a session's directory holds.
pub const SIDECAR_FILE: &str = "session.json";
/// How much of a log's head is read to find the delegation a child wrote
/// there. A delegation record holds two ids and a type name, so this bound
/// is generous for what it looks for and small enough to read per
/// sidecar-less directory.
const HEAD_BYTES: u64 = 4096;

/// The directory a session's files live in, under the store root.
///
/// The store names a directory with the sid's display form, but a store may
/// hold directories named in the ULID spelling -- written before the uuid
/// format, or seeded from an id in that form -- so a path rebuilt from the id
/// alone can miss the files a scan just read. Both spellings are tried and the
/// one on disk wins; with neither present the display form is returned, which
/// is where a new session is written.
pub fn session_dir(root: &Path, sid: SessionId) -> PathBuf {
    let dir = root.join(sid.to_string());
    if dir.is_dir() {
        return dir;
    }
    let ulid = root.join(sid.ulid_name());
    if ulid.is_dir() {
        return ulid;
    }
    dir
}

/// What a directory in the sessions root is, judged from files on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionClass {
    /// Resumable: a log plus a sidecar recording no parent.
    User,
    /// A sub-agent session: its sidecar, or the delegation its own log
    /// opens with, names the parent that started it.
    SubAgent,
    /// A log with no sidecar and no parent named in it: a leftover from a
    /// test run, or a session whose lineage was never written.
    LogOnly,
    /// Neither log nor sidecar: the directory a process leaves when it takes
    /// the lock and appends nothing.
    Shell,
}

/// The delegation a session's own log opens with: the parent it was
/// delegated from, and the agent type it ran as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParentLink {
    pub parent_session_id: String,
    pub subagent_type: String,
}

/// A classified directory: its id, class, and last-active second.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionEntry {
    pub sid: SessionId,
    /// The directory the entry was scanned from, already joined onto the
    /// root. A directory name may be in a legacy id format, whose spelling
    /// differs from the sid's display form, so a caller that rebuilds this
    /// path from the id can miss the files it holds: a consumer of a scan
    /// result reads the session through this, and a caller holding only an id
    /// reads it through session_dir.
    pub path: PathBuf,
    pub class: SessionClass,
    /// The sidecar the class was judged from, carried so the caller does not
    /// read it again. None for a shell: a directory with no log is a shell
    /// whatever its sidecar says, so that sidecar is never read.
    pub descriptor: Option<SessionDescriptor>,
    /// The parent this session's log names, read only where the sidecar left
    /// the question open. Carried for the same reason as the sidecar: whoever
    /// needs this session's lineage reads it here rather than opening the log
    /// again.
    pub parent: Option<ParentLink>,
    /// The log's mtime, else the directory's mtime. Zero when neither is
    /// readable.
    pub last_active: u64,
}

impl SessionEntry {
    /// Whether this is one of the user's sessions: what the store count, the
    /// retention cap, and the resume picker all ask, so no two of them can
    /// disagree. A sub-agent session is excluded because it is not the user's
    /// to resume; a shell because it has nothing to resume; a log with no
    /// sidecar and no parent because nothing says whose it is, and a
    /// directory the user cannot tell apart from a sub-agent's should not be
    /// counted or listed as one of theirs.
    pub fn is_user_session(&self) -> bool {
        self.class == SessionClass::User
    }
}

/// The rule, over the three facts that decide it: whether a log is there,
/// what the sidecar says, and the parent the log names.
///
/// The sidecar decides when it is there. The two lineage sources cannot
/// disagree: a delegation is written to the child's log and to its sidecar
/// at the same boundary, from the same facts, so the log only answers where
/// the sidecar is missing.
pub fn classify(
    has_log: bool,
    provenance: Option<&SessionProvenance>,
    parent: Option<&ParentLink>,
) -> SessionClass {
    match (has_log, provenance) {
        (false, _) => SessionClass::Shell,
        (true, Some(SessionProvenance::SpawnedBy { .. })) => SessionClass::SubAgent,
        (true, Some(_)) => SessionClass::User,
        (true, None) if parent.is_some() => SessionClass::SubAgent,
        (true, None) => SessionClass::LogOnly,
    }
}

/// A directory that could be a session, holding only the facts that are cheap
/// to take for every directory in the store.
struct Candidate {
    sid: SessionId,
    path: PathBuf,
    last_active: u64,
}

impl Candidate {
    /// The classified entry: reads this directory's sidecar and judges the class.
    fn classified(self) -> SessionEntry {
        let log = self.path.join(LOG_FILE);
        let has_log = log.is_file();
        // A shell is a shell whatever its sidecar says, so a directory with no
        // log is not parsed. Shells are the class that accumulates fastest, and
        // parsing one per launch would tax every scan.
        let descriptor = if has_log {
            read_sidecar(&self.path)
        } else {
            None
        };
        // The log is opened only where the sidecar left the lineage open, so
        // a store whose sessions all have sidecars pays nothing here.
        let parent = if has_log && descriptor.is_none() {
            read_parent_link(&log)
        } else {
            None
        };
        let class = classify(
            has_log,
            descriptor.as_ref().map(|d| &d.provenance),
            parent.as_ref(),
        );
        SessionEntry {
            sid: self.sid,
            path: self.path,
            class,
            descriptor,
            parent,
            last_active: self.last_active,
        }
    }
}

/// Every directory under root whose name is a session id, with the cheap facts:
/// a directory whose name is not a session id is not a session, so the block
/// store and index directories are skipped rather than examined. Read-only and
/// best-effort: an unreadable root yields nothing, an unreadable log or
/// directory yields an unknown last-active, and neither fails the scan.
fn candidates(root: &Path) -> Vec<Candidate> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let Some(sid) = dir
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(SessionId::from_display_string)
        else {
            continue;
        };
        let last_active = file_secs(&dir.join(LOG_FILE))
            .or_else(|| file_secs(&dir))
            .unwrap_or(0);
        out.push(Candidate {
            sid,
            path: dir,
            last_active,
        });
    }
    out
}

/// Classify every session directory under root in one pass.
///
/// An unreadable sidecar leaves the lineage unknown rather than failing the
/// scan, and a lock is never consulted: it says someone is writing, not that a
/// session exists.
pub fn scan_sessions(root: &Path) -> Vec<SessionEntry> {
    candidates(root)
        .into_iter()
        .map(Candidate::classified)
        .collect()
}

/// The user's own sessions, newest-active first, at most limit of them.
///
/// The class comes from the rule the full scan applies; what is bounded is the
/// sidecar reads. Directories are ranked on the cheap fact first and their
/// sidecars are read in that order, so a store holding thousands of sub-agent
/// directories pays no parse for a row the limit would drop. The limit decides
/// the ranking, so the result is the top of the same order rather than a list
/// cut to size, and only a caller whose own output is bounded by the same limit
/// may ask for one. A limit of zero asks for none of them.
pub fn recent_user_sessions(root: &Path, limit: usize) -> Vec<SessionEntry> {
    let mut candidates = candidates(root);
    candidates.sort_by_key(|candidate| std::cmp::Reverse(candidate.last_active));
    let mut out = Vec::new();
    for candidate in candidates {
        if out.len() >= limit {
            break;
        }
        let entry = candidate.classified();
        if entry.is_user_session() {
            out.push(entry);
        }
    }
    out
}

/// Read and parse one session's sidecar. None when absent or unreadable.
fn read_sidecar(dir: &Path) -> Option<SessionDescriptor> {
    let bytes = fs::read(sidecar_path(dir)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// The delegation a session's log opens with. A child writes it as its first
/// durable record, at the boundary that mints the session, so the head is
/// where it lands. Any other first record, an unreadable log, or a line past
/// the head bound means no delegation is recorded: the class then rests on
/// the sidecar alone.
fn read_parent_link(log: &Path) -> Option<ParentLink> {
    let file = fs::File::open(log).ok()?;
    let mut head = String::new();
    BufReader::new(file)
        .take(HEAD_BYTES)
        .read_line(&mut head)
        .ok()?;
    let entry: SessionLogEntry = serde_json::from_str(&head).ok()?;
    match entry.event {
        SessionEvent::ChildDelegated {
            parent_session_id,
            subagent_type,
        } => Some(ParentLink {
            parent_session_id,
            subagent_type,
        }),
        _ => None,
    }
}

fn sidecar_path(dir: &Path) -> PathBuf {
    dir.join(SIDECAR_FILE)
}

fn file_secs(path: &Path) -> Option<u64> {
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    Some(epoch_secs(modified))
}

fn epoch_secs(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
#[path = "session_class_tests.rs"]
mod session_class_tests;
