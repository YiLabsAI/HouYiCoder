//! Snapshot a destructive shell command's writable roots before the command
//! runs, so /undo can revert it. Split the snapshot outcome by what it means,
//! not just Ok/Err, so a performance skip does not masquerade as a safety
//! refusal and a real failure does not slip through as a skip.
//!
//! The gate asked for the command and the user said yes on the premise that
//! undo exists. A snapshot failure removes that premise, so a real failure
//! refuses to run the command rather than letting it execute without recovery.

use std::path::PathBuf;
use std::sync::Mutex;

use houyicoder_api::sandbox::SandboxSession;
use houyicoder_protocol::extension::ToolError;

use crate::snapshot::{SnapshotStore, UndoStack};

/// Snapshot the session's writable roots and push the entry onto the undo
/// stack, using the store's own copy-on-write policy probe. Returns
/// Ok(None) when the command proceeds with undo pushed; Ok(Some(notice))
/// when it proceeds without undo (a policy decline surfaced so the caller
/// can show the user undo is unavailable); Err when undo is genuinely
/// unavailable and the command must not run. See prepare_with_probe for the
/// per-outcome contract. The notice is returned (not eprintln'd) so the
/// caller can route it into the tool result, which reaches the transcript
/// and the user-facing render.
pub(crate) fn prepare(
    store: &SnapshotStore,
    stack: &Mutex<UndoStack>,
    session: &dyn SandboxSession,
) -> Result<Option<String>, ToolError> {
    prepare_with_probe(store, stack, session, SnapshotStore::check_reflink_policy)
}

/// The testable core of prepare: the policy probe is injected so the decline
/// branch (a performance skip, not a safety event) is exercisable on
/// platforms where the real probe never declines. The probe is run once;
/// the snapshot walk is then taken via snapshot_after_probe so the workspace
/// is not walked twice (the probe already paid the size traversal, and a
/// re-probe inside snapshot() would also race the threshold and report a
/// decline as a real failure).
///
/// Outcomes:
/// - policy decline (undo is possible but deliberately not paid for): run
///   the command; return a notice so the user is not misled into expecting
///   /undo. The decline reason is carried in the notice (the probe is
///   injected, so the reason is not always size).
/// - snapshot pushed: run the command (Ok(None)).
/// - real I/O failure or a poisoned undo stack: undo is genuinely
///   unavailable; refuse to run (Err).
///
/// The decline branch is not exercised by the real probe on copy-on-write
/// filesystems (APFS, btrfs, XFS) where it always returns Ok; it fires on
/// filesystems without reflink when the workspace exceeds the slow-copy
/// threshold. The injected form lets a test cover it regardless of platform.
pub(crate) fn prepare_with_probe(
    store: &SnapshotStore,
    stack: &Mutex<UndoStack>,
    session: &dyn SandboxSession,
    probe: fn(&SnapshotStore) -> std::io::Result<()>,
) -> Result<Option<String>, ToolError> {
    let additional: Vec<PathBuf> = session
        .working_dirs()
        .into_iter()
        .map(PathBuf::from)
        .collect();
    match probe(store) {
        // Performance skip, not a safety event: undo is possible but
        // deliberately not paid for. Run the command; return a notice the
        // caller routes into the tool result so the user is not misled into
        // expecting /undo. Carry the probe's reason -- it is injected, so
        // the reason is not always "workspace too large".
        Err(e) => Ok(Some(format!(
            "bash: snapshot skipped ({e}); undo unavailable for this command"
        ))),
        Ok(()) => match store.snapshot_after_probe(&additional) {
            Ok(entry) => match stack.lock() {
                Ok(mut guard) => {
                    guard.push(entry);
                    Ok(None)
                }
                Err(_) => Err(ToolError::Failed(
                    "bash: undo stack state is corrupted; refusing to run \
                     a destructive command without recovery"
                        .into(),
                )),
            },
            Err(e) => Err(ToolError::Failed(format!(
                "bash: snapshot failed ({e}); undo unavailable, refusing to \
                 run a destructive command without recovery"
            ))),
        },
    }
}

#[cfg(test)]
#[path = "bash_snapshot_tests.rs"]
mod tests;
