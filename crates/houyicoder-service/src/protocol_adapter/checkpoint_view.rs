//! Map the host's fence state and its recovery answer to the forms the
//! terminal reads, so the sandbox pane renders them without importing the
//! engine crate.
//!
//! The mapping copies what the host measured and never fills a gap: a write
//! the host could not judge keeps its reason, and a boundary the sandbox port
//! does not describe is not invented at the terminal.

use houyicoder_api::sandbox::FenceStatus;
use houyicoder_api::sandbox::SandboxSession;
use houyicoder_core::snapshot::file_transaction::{
    ImageSummary, RecoveredWrite, RecoveryReport, RestoreOutcome, WriteState,
};
use houyicoder_protocol::frontend::checkpoint::{
    CheckpointImage, CheckpointListView, CheckpointRestoreView, CheckpointState, CheckpointView,
    FenceState, RestoreView, SandboxBoundariesView, UnreadableView,
};

/// Map the boundaries this session runs inside.
pub(crate) fn map_boundaries(sandbox: &dyn SandboxSession) -> SandboxBoundariesView {
    let workspace_root = sandbox.workspace_root().to_string_lossy().into_owned();
    let mut writable_roots = vec![workspace_root.clone()];
    writable_roots.extend(sandbox.writing_dirs());
    SandboxBoundariesView {
        fence: map_fence(sandbox.fence_status()),
        workspace_root,
        writable_roots,
    }
}

fn map_fence(status: FenceStatus) -> FenceState {
    match status {
        FenceStatus::Enforced => FenceState::Enforced,
        FenceStatus::NotEnforced => FenceState::NotEnforced,
        FenceStatus::Failed(reason) => FenceState::Failed(reason),
        FenceStatus::Unavailable => FenceState::Unavailable,
    }
}

/// Map every write the session recorded, keeping the writes the host could
/// not judge beside the ones it could.
pub(crate) fn map_checkpoints(report: RecoveryReport) -> CheckpointListView {
    CheckpointListView {
        checkpoints: report.writes.into_iter().map(map_write).collect(),
        unreadable: report
            .unreadable
            .into_iter()
            .map(|unreadable| UnreadableView {
                operation: unreadable.operation,
                reason: unreadable.error.to_string(),
            })
            .collect(),
    }
}

fn map_write(write: RecoveredWrite) -> CheckpointView {
    CheckpointView {
        operation: write.operation,
        target: write.target,
        state: map_state(write.state),
        written_ms: write.written_ms,
        before: map_image(write.before),
        after: map_image(write.after),
    }
}

fn map_state(state: WriteState) -> CheckpointState {
    match state {
        WriteState::Applied => CheckpointState::Applied,
        WriteState::NotApplied => CheckpointState::NotApplied,
        WriteState::Conflict => CheckpointState::Conflict,
        WriteState::Unverified => CheckpointState::Unverified,
    }
}

fn map_image(image: ImageSummary) -> CheckpointImage {
    CheckpointImage {
        present: image.present,
        hash: image.hash,
        bytes: image.bytes,
    }
}

/// Map what a restore did.
pub(crate) fn map_restore(operation: String, outcome: RestoreOutcome) -> CheckpointRestoreView {
    let outcome = match outcome {
        RestoreOutcome::Unchanged => RestoreView::Unchanged,
        RestoreOutcome::Restored => RestoreView::Restored,
        RestoreOutcome::Kept { copy } => RestoreView::Kept {
            copy: copy.map(|path| path.to_string_lossy().into_owned()),
        },
        RestoreOutcome::Unbacked => RestoreView::Unbacked,
    };
    CheckpointRestoreView { operation, outcome }
}

#[cfg(test)]
mod tests;
