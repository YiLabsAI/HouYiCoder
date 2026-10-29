//! Tests for the sandbox pane mapping: the boundaries a sandbox reports, every
//! write state, every fence state, and every restore outcome.

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use houyicoder_api::sandbox::FenceStatus;
use houyicoder_api::sandbox::SandboxSession;
use houyicoder_async::PFut;
use houyicoder_context::ExecConfig;
use houyicoder_context::ExecResult;
use houyicoder_context::SandboxError;
use houyicoder_core::snapshot::file_transaction::{
    ImageSummary, RecoveredWrite, RecoveryError, RecoveryReport, RestoreOutcome, SideText,
    UnreadableWrite, WriteState, WriteText,
};
use houyicoder_protocol::frontend::checkpoint::{
    CheckpointImage, CheckpointSide, CheckpointState, FenceState, RestoreView,
};

use super::{map_boundaries, map_checkpoints, map_fence, map_restore, map_texts};

/// A sandbox naming one workspace root, its scratch directory, and one
/// directory the user added beyond them.
struct Boundaries {
    root: PathBuf,
    scratch: Option<PathBuf>,
    writing: Vec<String>,
}

impl SandboxSession for Boundaries {
    fn exec_with_config(
        &self,
        _command: &str,
        _config: ExecConfig,
    ) -> PFut<'_, Result<ExecResult, SandboxError>> {
        Box::pin(async move {
            Err(SandboxError::Unsupported(
                "no execution in this stub".into(),
            ))
        })
    }
    fn workspace_root(&self) -> Arc<Path> {
        Arc::from(self.root.clone())
    }
    fn scratch_dir(&self) -> Option<PathBuf> {
        self.scratch.clone()
    }
    fn writing_dirs(&self) -> Vec<String> {
        self.writing.clone()
    }
}

fn image(hash: Option<&str>, bytes: Option<u64>) -> ImageSummary {
    ImageSummary {
        present: true,
        hash: hash.map(str::to_string),
        bytes,
    }
}

fn write(state: WriteState) -> RecoveredWrite {
    RecoveredWrite {
        operation: "0001-7-000000".into(),
        target: "a.txt".into(),
        state,
        committed: true,
        written_ms: Some(1_700_000_000_000),
        before: image(Some("before-hash"), Some(3)),
        after: image(Some("after-hash"), Some(3)),
    }
}

#[test]
fn test_checkpoints_name_each_state() {
    let states = [
        (WriteState::Applied, CheckpointState::Applied),
        (WriteState::NotApplied, CheckpointState::NotApplied),
        (WriteState::Conflict, CheckpointState::Conflict),
        (WriteState::Unverified, CheckpointState::Unverified),
    ];
    for (core_state, wire_state) in states {
        let report = RecoveryReport {
            writes: vec![write(core_state)],
            unreadable: Vec::new(),
        };
        let view = map_checkpoints(report);
        assert_eq!(view.checkpoints.len(), 1, "the write is listed");
        assert_eq!(
            view.checkpoints[0].state, wire_state,
            "the state crosses unchanged"
        );
        assert_eq!(
            view.checkpoints[0].before,
            CheckpointImage {
                present: true,
                hash: Some("before-hash".into()),
                bytes: Some(3),
            },
            "the earlier image keeps the name its bytes are stored under"
        );
    }
}

#[test]
fn test_checkpoints_keep_unreadable() {
    let report = RecoveryReport {
        writes: vec![write(WriteState::Applied)],
        unreadable: vec![UnreadableWrite {
            operation: None,
            error: RecoveryError::Record {
                path: PathBuf::from("prepared.json"),
                reason: "the record does not parse".into(),
            },
        }],
    };
    let view = map_checkpoints(report);

    assert_eq!(view.checkpoints.len(), 1, "the readable write is listed");
    assert_eq!(view.unreadable.len(), 1, "the unreadable write is kept");
    assert_eq!(view.unreadable[0].operation, None, "it has no name to show");
    assert!(
        view.unreadable[0].reason.contains("does not parse"),
        "the reason is the one the host reported: {}",
        view.unreadable[0].reason
    );
}

/// The scratch directory is a writable root of the session even though the
/// user never granted it: the child runs with it as TMPDIR, so a boundary
/// report that left it out would name a narrower fence than the one enforced.
#[test]
fn test_boundaries_list_writable_roots() {
    let sandbox = Boundaries {
        root: PathBuf::from("/work/repo"),
        scratch: Some(PathBuf::from("/var/folders/hy-tmp-501-0")),
        writing: vec!["/tmp/deps".into()],
    };
    let view = map_boundaries(&sandbox);

    assert_eq!(
        view.workspace_root, "/work/repo",
        "the root is the one reported"
    );
    assert_eq!(
        view.writable_roots,
        vec!["/work/repo".to_string(), "/tmp/deps".to_string()],
        "the root comes first, then the added directories"
    );
    assert_eq!(
        view.scratch_root.as_deref(),
        Some("/var/folders/hy-tmp-501-0"),
        "the scratch dir is reported apart from the granted roots"
    );
}

/// A backend that creates no scratch directory reports only what it holds, so
/// the mapping does not invent a path the fence never opened.
#[test]
fn test_boundaries_without_scratch() {
    let sandbox = Boundaries {
        root: PathBuf::from("/work/repo"),
        scratch: None,
        writing: Vec::new(),
    };
    let view = map_boundaries(&sandbox);

    assert_eq!(
        view.writable_roots,
        vec!["/work/repo".to_string()],
        "no scratch dir means no row for it"
    );
    assert_eq!(view.scratch_root, None, "the mapping invents no path");
}

#[test]
fn test_fence_names_each_state() {
    assert_eq!(map_fence(FenceStatus::Enforced), FenceState::Enforced);
    assert_eq!(map_fence(FenceStatus::NotEnforced), FenceState::NotEnforced);
    assert_eq!(map_fence(FenceStatus::Unavailable), FenceState::Unavailable);
    assert_eq!(
        map_fence(FenceStatus::Failed("no seatbelt".into())),
        FenceState::Failed("no seatbelt".into()),
        "a fence that failed carries the reason it failed"
    );
}

#[test]
fn test_restore_names_each_outcome() {
    let outcomes = [
        (RestoreOutcome::Unchanged, RestoreView::Unchanged),
        (RestoreOutcome::Restored, RestoreView::Restored),
        (RestoreOutcome::Unbacked, RestoreView::Unbacked),
    ];
    for (core_outcome, wire_outcome) in outcomes {
        let view = map_restore("op".into(), core_outcome);
        assert_eq!(view.operation, "op");
        assert_eq!(view.outcome, wire_outcome, "the outcome crosses unchanged");
    }
}

/// Every side a record can report crosses unchanged, so the detail states
/// what the host knows instead of comparing a gap to an empty file.
#[test]
fn test_texts_map_each_side() {
    let cases = [
        (
            SideText::Text {
                text: "x".into(),
                truncated: false,
            },
            CheckpointSide::Text {
                text: "x".into(),
                truncated: false,
            },
        ),
        (SideText::Absent, CheckpointSide::Absent),
        (SideText::NotKept, CheckpointSide::NotKept),
        (SideText::NotText, CheckpointSide::NotText),
    ];
    for (side, expected) in cases {
        let view = map_texts(WriteText {
            operation: "op".into(),
            target: "a.txt".into(),
            before: side,
            after: SideText::Absent,
        });
        assert_eq!(view.operation, "op");
        assert_eq!(view.target, "a.txt");
        assert_eq!(view.before, expected);
    }
}

#[test]
fn test_restore_keeps_copy_path() {
    let kept = map_restore(
        "op".into(),
        RestoreOutcome::Kept {
            copy: Some(PathBuf::from("a.txt.recovery-op")),
        },
    );
    assert_eq!(kept.operation, "op");
    assert_eq!(
        kept.outcome,
        RestoreView::Kept {
            copy: Some("a.txt.recovery-op".into()),
        },
        "the copy path crosses so the notice can name the file"
    );
}
