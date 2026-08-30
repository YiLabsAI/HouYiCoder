//! Tests for the bash destructive-snapshot decision. Lives in a tests file
//! (suffix _tests.rs) so the stub boilerplate is not counted by the diff-cov
//! gate; the tests still drive coverage of the production lines in
//! bash_snapshot.rs.

use super::*;
use crate::agent::BashTool;
use houyicoder_api::sandbox::SandboxSession;
use houyicoder_api::tool::Tool;
use houyicoder_async::PFut;
use houyicoder_context::{ExecConfig, ExecResult, SandboxError};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// A sandbox stub whose working_dirs() is configurable. The other trait
/// methods are required by the trait but never called by prepare_with_probe,
/// so they panic if reached (the asserts are about the snapshot decision,
/// not the command's result).
struct StubSession {
    root: PathBuf,
    extra: Vec<String>,
}
impl SandboxSession for StubSession {
    fn exec_with_config(
        &self,
        _command: &str,
        _config: ExecConfig,
    ) -> PFut<'_, Result<ExecResult, SandboxError>> {
        unreachable!("prepare runs before exec")
    }
    fn read_file(&self, _path: &str, _max: usize) -> PFut<'_, Result<Vec<u8>, SandboxError>> {
        unreachable!("prepare does not read")
    }
    fn write_file(&self, _path: &str, _content: Vec<u8>) -> PFut<'_, Result<(), SandboxError>> {
        unreachable!("prepare does not write")
    }
    fn list_dir(
        &self,
        _path: &str,
    ) -> PFut<'_, Result<Vec<houyicoder_context::DirEntry>, SandboxError>> {
        unreachable!("prepare does not list")
    }
    fn path_exists(&self, _path: &str) -> PFut<'_, Result<bool, SandboxError>> {
        unreachable!("prepare does not stat")
    }
    fn workspace_root(&self) -> Arc<Path> {
        Arc::from(self.root.clone())
    }
    fn working_dirs(&self) -> Vec<String> {
        self.extra.clone()
    }
}

fn temp_workspace() -> PathBuf {
    static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path =
        std::env::temp_dir().join(format!("bash-snap-test-{}-{n}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&path).unwrap();
    path
}

/// A snapshot that succeeds pushes the entry onto the stack and returns
/// Ok(None), so the caller proceeds to run the command with no notice.
#[test]
fn test_prepare_ok_pushes_proceeds() {
    let ws = temp_workspace();
    let store = SnapshotStore::new(&ws).expect("store");
    let stack = Mutex::new(UndoStack::new());
    let session = StubSession {
        root: ws.clone(),
        extra: vec![],
    };
    let result = prepare(&store, &stack, &session);
    assert!(result.is_ok(), "ok snapshot proceeds: {:?}", result);
    assert!(result.unwrap().is_none(), "no notice on a pushed snapshot");
    assert_eq!(stack.lock().unwrap().len(), 1, "entry pushed");
    std::fs::remove_dir_all(&ws).ok();
}

/// A real I/O failure from the walk (an additional root that does not
/// exist) refuses to run the command: Err, no entry pushed. This is the
/// fail-closed path for undo genuinely unavailable mid-call.
#[test]
fn test_prepare_walk_failure_refuses() {
    let ws = temp_workspace();
    let store = SnapshotStore::new(&ws).expect("store");
    let stack = Mutex::new(UndoStack::new());
    // An additional root that does not exist: WalkDir yields an error on
    // the first iteration, which walk_root_into propagates. Verify the
    // error surfaces (rather than WalkDir silently yielding nothing)
    // before relying on this path as the failure trigger.
    let bogus = ws.join("does-not-exist-xyz");
    let snapshot_result = store.snapshot(std::slice::from_ref(&bogus));
    assert!(
        snapshot_result.is_err(),
        "nonexistent root must error, not skip"
    );
    let session = StubSession {
        root: ws.clone(),
        extra: vec![bogus.to_string_lossy().into_owned()],
    };
    let result = prepare(&store, &stack, &session);
    assert!(result.is_err(), "walk failure refuses: {:?}", result);
    let msg = result.unwrap_err().to_string();
    assert!(msg.contains("snapshot failed"), "{msg}");
    assert!(msg.contains("refusing"), "{msg}");
    assert_eq!(stack.lock().unwrap().len(), 0, "no entry pushed on failure");
    std::fs::remove_dir_all(&ws).ok();
}

/// A policy decline (injected probe that returns Err) proceeds without a
/// snapshot and returns a notice the caller routes to the user -- a
/// performance skip, not a safety refusal. The notice carries the probe's
/// reason (the probe is injected, so the reason is not always size). This
/// branch is not exercisable through the real probe on copy-on-write
/// filesystems, so the seam covers it.
#[test]
fn test_prepare_decline_returns_notice() {
    let ws = temp_workspace();
    let store = SnapshotStore::new(&ws).expect("store");
    let stack = Mutex::new(UndoStack::new());
    let session = StubSession {
        root: ws.clone(),
        extra: vec![],
    };
    let decline = |_s: &SnapshotStore| -> std::io::Result<()> {
        Err(std::io::Error::other("probe-decline-reason"))
    };
    let result = prepare_with_probe(&store, &stack, &session, decline);
    let notice = result.expect("decline proceeds with a notice");
    assert!(notice.is_some(), "decline carries a notice");
    let notice = notice.unwrap();
    assert!(notice.contains("snapshot skipped"), "{notice}");
    assert!(notice.contains("undo unavailable"), "{notice}");
    // The probe's reason is carried, not hardcoded as size.
    assert!(notice.contains("probe-decline-reason"), "{notice}");
    assert_eq!(stack.lock().unwrap().len(), 0, "no entry pushed on decline");
    std::fs::remove_dir_all(&ws).ok();
}

/// A poisoned undo stack refuses to run the command even when the snapshot
/// itself succeeded: undo is unavailable because internal state is
/// corrupted, not because undo was never wired. The message says so.
#[test]
fn test_prepare_poisoned_stack_refuses() {
    let ws = temp_workspace();
    let store = SnapshotStore::new(&ws).expect("store");
    let stack = std::sync::Mutex::new(UndoStack::new());
    // Poison the mutex by panicking while holding the lock.
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = stack.lock().unwrap();
        panic!("poison the undo stack");
    }))
    .ok();
    let session = StubSession {
        root: ws.clone(),
        extra: vec![],
    };
    let result = prepare(&store, &stack, &session);
    assert!(result.is_err(), "poisoned stack refuses: {:?}", result);
    let msg = result.unwrap_err().to_string();
    assert!(msg.contains("corrupted"), "{msg}");
    assert!(msg.contains("refusing"), "{msg}");
    std::fs::remove_dir_all(&ws).ok();
}

/// The bash schema does not advertise a workdir field. The sandbox runs in
/// a fixed workspace cwd and the executor never reads workdir, so
/// advertising it would let the model believe a different directory is in
/// effect while the command runs in the workspace root. Pin the schema so
/// the field is not re-added without an executor that honors it.
#[test]
fn test_bash_schema_no_workdir() {
    let t = BashTool::new(Arc::new(StubSession {
        root: PathBuf::from("/tmp"),
        extra: vec![],
    }));
    let schema = t.input_schema();
    assert!(
        schema["properties"].get("workdir").is_none(),
        "workdir must not be advertised: {schema}"
    );
    let props = schema["properties"]
        .as_object()
        .expect("properties is an object");
    assert_eq!(props.len(), 1, "command is the sole field: {schema}");
    assert!(
        props.contains_key("command"),
        "command is the sole field: {schema}"
    );
}
