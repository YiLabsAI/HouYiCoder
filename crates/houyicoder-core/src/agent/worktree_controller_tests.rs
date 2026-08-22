use super::*;
use houyicoder_async::PFut;
use houyicoder_context::{DirEntry, ExecConfig, ExecResult, SandboxError};
use houyicoder_memory::InMemoryBackend;
use houyicoder_session::SessionStore;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::AtomicU64;

/// A stub sandbox whose narrow returns a no-op restore guard. The
/// controller only calls narrow_to_worktree + active_exec_count; the other
/// trait methods are stubbed to satisfy the trait object.
struct StubSandbox {
    narrows: AtomicU64,
    in_flight: usize,
    narrow_err: bool,
}
impl SandboxSession for StubSandbox {
    fn exec_with_config(
        &self,
        _command: &str,
        _config: ExecConfig,
    ) -> PFut<'_, Result<ExecResult, SandboxError>> {
        Box::pin(async move {
            Ok(ExecResult {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: Some(0),
            })
        })
    }
    fn read_file(&self, _path: &str, _max: usize) -> PFut<'_, Result<Vec<u8>, SandboxError>> {
        Box::pin(async move { Ok(Vec::new()) })
    }
    fn write_file(&self, _path: &str, _content: Vec<u8>) -> PFut<'_, Result<(), SandboxError>> {
        Box::pin(async move { Ok(()) })
    }
    fn list_dir(&self, _path: &str) -> PFut<'_, Result<Vec<DirEntry>, SandboxError>> {
        Box::pin(async move { Ok(Vec::new()) })
    }
    fn path_exists(&self, _path: &str) -> PFut<'_, Result<bool, SandboxError>> {
        Box::pin(async move { Ok(false) })
    }
    fn workspace_root(&self) -> Arc<std::path::Path> {
        Arc::from(std::path::PathBuf::from("/"))
    }
    fn narrow_to_worktree(
        &self,
        _worktree: &Path,
        _git_common_dir: &Path,
    ) -> Result<houyicoder_api::sandbox::WorktreeFenceGuard, SandboxError> {
        if self.narrow_err {
            return Err(SandboxError::Unsupported(
                "stub narrow failure for fail-closed test".into(),
            ));
        }
        self.narrows.fetch_add(1, Ordering::SeqCst);
        Ok(houyicoder_api::sandbox::WorktreeFenceGuard::new(Box::new(
            || Ok(()),
        )))
    }
    fn active_exec_count(&self) -> usize {
        self.in_flight
    }
}

pub(crate) fn make_repo(tag: u64) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("houyi-wt-ctrl-{}-{tag}", std::process::id()));
    drop(std::fs::remove_dir_all(&dir));
    std::fs::create_dir_all(&dir).expect("mkdir");
    drop(
        Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(["init", "-q"])
            .status(),
    );
    drop(
        Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(["config", "user.email", "t@x"])
            .status(),
    );
    drop(
        Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(["config", "user.name", "t"])
            .status(),
    );
    drop(
        Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(["commit", "--allow-empty", "-m", "init", "-q"])
            .status(),
    );
    dir
}

/// Build a controller wired to a throwaway repo + a stub sandbox + a real
/// in-memory session store. Returns the controller, the store (for replay
/// assertions), + the shared cwd Arc.
pub(crate) fn wired(
    repo: &Path,
    in_flight: usize,
) -> (
    Arc<WorktreeController>,
    Arc<SessionStore>,
    Arc<RwLock<PathBuf>>,
) {
    wired_opt(repo, in_flight, false)
}

/// Same as wired but with a sandbox whose narrow always fails -- for the
/// fail-closed path (a fence failure must reject, not degrade).
pub(crate) fn wired_err(repo: &Path) -> Arc<WorktreeController> {
    wired_opt(repo, 0, true).0
}

fn wired_opt(
    repo: &Path,
    in_flight: usize,
    narrow_err: bool,
) -> (
    Arc<WorktreeController>,
    Arc<SessionStore>,
    Arc<RwLock<PathBuf>>,
) {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let store_dyn: Arc<dyn SessionLog> = store.clone();
    let sandbox: Arc<dyn SandboxSession> = Arc::new(StubSandbox {
        narrows: AtomicU64::new(0),
        in_flight,
        narrow_err,
    });
    let cwd = Arc::new(RwLock::new(repo.to_path_buf()));
    let session_id = SessionId::new();
    let controller = Arc::new(WorktreeController::new(
        repo.to_path_buf(),
        repo.join(".git"),
        sandbox,
        store_dyn,
        session_id,
    ));
    controller.set_cwd_handle(Arc::clone(&cwd));
    (controller, store, cwd)
}

#[tokio::test]
async fn test_enter_exit_round_trip() {
    let repo = make_repo(1);
    let (controller, _store, cwd) = wired(&repo, 0);
    let r = controller
        .enter(Some("feat-a".into()))
        .await
        .expect("enter");
    assert!(controller.current(), "in session after enter");
    assert_eq!(*cwd.read().unwrap(), PathBuf::from(&r.worktree_path));
    controller
        .exit(ExitAction::Keep, false)
        .await
        .expect("exit keep");
    assert!(!controller.current(), "out of session after exit");
    std::fs::remove_dir_all(&repo).ok();
}

#[tokio::test]
async fn test_exit_remove_clean_succeeds() {
    let repo = make_repo(2);
    let (controller, _store, _cwd) = wired(&repo, 0);
    controller
        .enter(Some("feat-r".into()))
        .await
        .expect("enter");
    controller
        .exit(ExitAction::Remove, false)
        .await
        .expect("exit remove clean");
    assert!(!controller.current());
    std::fs::remove_dir_all(&repo).ok();
}

#[tokio::test]
async fn test_second_enter_refused() {
    let repo = make_repo(3);
    let (controller, _store, _cwd) = wired(&repo, 0);
    controller
        .enter(Some("feat-s".into()))
        .await
        .expect("first enter");
    let err = controller
        .enter(Some("feat-s2".into()))
        .await
        .expect_err("second refused");
    assert!(err.to_string().contains("already in a worktree"));
    controller.exit(ExitAction::Remove, true).await.ok();
    std::fs::remove_dir_all(&repo).ok();
}

#[tokio::test]
async fn test_enter_refused_inflight_exec() {
    let repo = make_repo(4);
    let (controller, _store, _cwd) = wired(&repo, 1);
    let err = controller
        .enter(Some("feat-b".into()))
        .await
        .expect_err("in-flight refused");
    assert!(err.to_string().contains("in flight"));
    std::fs::remove_dir_all(&repo).ok();
}

#[tokio::test]
async fn test_remove_refuses_without_discard() {
    let repo = make_repo(7);
    let (controller, _store, _cwd) = wired(&repo, 0);
    let r = controller
        .enter(Some("feat-d".into()))
        .await
        .expect("enter");
    // Make a commit inside the worktree so remove-without-discard refuses.
    drop(
        Command::new("git")
            .arg("-C")
            .arg(&r.worktree_path)
            .args(["commit", "--allow-empty", "-m", "wip", "-q"])
            .status(),
    );
    let err = controller
        .exit(ExitAction::Remove, false)
        .await
        .expect_err("refuse without discard");
    // The refuse must carry all three parts the model and the user need:
    // that the loss is permanent, that the user has to confirm, and the
    // flag to re-invoke with. Pinned here rather than on a rendered row
    // because the row previews the body at the row width, which would
    // make the wording assertion depend on the terminal size.
    let msg = err.to_string();
    assert!(
        msg.contains("Removing will discard"),
        "explains the discard is permanent: {msg}"
    );
    assert!(
        msg.contains("Confirm with the user"),
        "points back to the user: {msg}"
    );
    assert!(
        msg.contains("discard_changes"),
        "names the flag to re-invoke with: {msg}"
    );
    // With discard=true the remove proceeds.
    controller
        .exit(ExitAction::Remove, true)
        .await
        .expect("exit remove with discard");
    assert!(!controller.current());
    std::fs::remove_dir_all(&repo).ok();
}

#[tokio::test]
async fn test_exit_without_session_noop() {
    let repo = make_repo(5);
    let (controller, _store, _cwd) = wired(&repo, 0);
    let outcome = controller
        .exit(ExitAction::Keep, false)
        .await
        .expect("noop");
    assert!(outcome.message.contains("No active worktree"));
    std::fs::remove_dir_all(&repo).ok();
}

#[tokio::test]
async fn test_tools_execute_through_controller() {
    use crate::agent::tools::worktree_enter::EnterWorktreeTool;
    use crate::agent::tools::worktree_exit::ExitWorktreeTool;
    use houyicoder_api::tool::{Tool, ToolCtx};
    use serde_json::json;

    let repo = make_repo(6);
    let (controller, _store, _cwd) = wired(&repo, 0);
    let enter = EnterWorktreeTool::new(controller.clone());
    let out = enter
        .execute(ToolCtx::new("call-1"), json!({ "name": "tool-wt" }))
        .await
        .expect("enter tool");
    assert!(out["worktree_branch"].as_str().unwrap().contains("tool-wt"));
    assert!(!enter.is_concurrency_safe());
    assert!(!enter.is_destructive());

    let exit = ExitWorktreeTool::new(controller.clone());
    assert!(exit.requires_approval_for(&json!({"action":"remove"})));
    assert!(!exit.requires_approval_for(&json!({"action":"keep"})));
    let out = exit
        .execute(ToolCtx::new("call-2"), json!({"action":"keep"}))
        .await
        .expect("exit tool");
    assert_eq!(out["action"].as_str().unwrap(), "keep");
    std::fs::remove_dir_all(&repo).ok();
}

/// When the main branch ref moves while the agent is isolated, exit
/// surfaces a system line through the live sink so the user sees the
/// history rewrite — not just a diagnostic log entry. The sink is the
/// user-visible channel; without it the alert is silent.
#[tokio::test]
async fn test_exit_warns_history_rewrite() {
    let repo = make_repo(8);
    let (controller, _store, _cwd) = wired(&repo, 0);
    // Enter the worktree.
    controller.enter(None).await.expect("enter");
    // Rewrite the main branch ref while isolated (simulates the agent
    // rewriting history inside the fence).
    drop(
        Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["commit", "--allow-empty", "-m", "rewrite", "-q", "--amend"])
            .status(),
    );
    // Attach a capturing sink.
    let captured = Arc::new(Mutex::new(Vec::<String>::new()));
    let cap = captured.clone();
    controller.set_live_sink(Arc::new(move |ev: &LiveEvent| {
        if let LiveEvent::SystemLine { text } = ev {
            cap.lock().unwrap().push(text.clone());
        }
    }));
    // Exit.
    controller
        .exit(ExitAction::Keep, false)
        .await
        .expect("exit");
    let lines = captured.lock().unwrap().clone();
    assert!(
        lines
            .iter()
            .any(|l| l.contains("main branch") && l.contains("moved")),
        "main-branch-moved surfaces a system line: {lines:?}"
    );
    std::fs::remove_dir_all(&repo).ok();
}
