use super::*;
use crate::agent::tools::{EnterWorktreeTool, ExitWorktreeTool};
use crate::agent::worktree::preview::preview_remove_worktree;
use crate::agent::worktree::record::lease_path;
use crate::agent::worktree::session::WorktreeError;
use houyicoder_api::sandbox::WorktreeFenceGuard;
use houyicoder_api::tool::{Tool, ToolCtx};
use houyicoder_async::PFut;
use houyicoder_context::{DirEntry, ExecConfig, ExecResult, SandboxError, SessionEvent};
use houyicoder_memory::InMemoryBackend;
use houyicoder_session::SessionStore;
use serde_json::json;
use std::env::temp_dir;
use std::fs::create_dir_all;
use std::fs::remove_dir_all;
use std::fs::write;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::id;
use std::sync::Barrier;
use std::sync::atomic::AtomicU64;
use std::thread;

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
    fn workspace_root(&self) -> Arc<Path> {
        Arc::from(PathBuf::from("/"))
    }
    fn narrow_to_worktree(
        &self,
        _worktree: &Path,
        _git_common_dir: &Path,
    ) -> Result<WorktreeFenceGuard, SandboxError> {
        if self.narrow_err {
            return Err(SandboxError::Unsupported(
                "stub narrow failure for fail-closed test".into(),
            ));
        }
        self.narrows.fetch_add(1, Ordering::SeqCst);
        Ok(WorktreeFenceGuard::new(Box::new(|| Ok(()))))
    }
    fn active_exec_count(&self) -> usize {
        self.in_flight
    }
}

pub(crate) fn make_repo(tag: u64) -> PathBuf {
    let dir = temp_dir().join(format!("houyi-wt-ctrl-{}-{tag}", id()));
    drop(remove_dir_all(&dir));
    create_dir_all(&dir).expect("mkdir");
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

/// A session in a worktree holds its lease for the whole session, so a
/// removal preview beside it reads the tree as in use; exit gives the lease
/// up whether it keeps or removes the tree.
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
    let preview = preview_remove_worktree(&repo, "feat-a").expect("preview");
    assert!(preview.leased, "the entered tree reads as in use");
    controller
        .exit(ExitAction::Keep, false)
        .await
        .expect("exit keep");
    assert!(!controller.current(), "out of session after exit");
    let after = preview_remove_worktree(&repo, "feat-a").expect("preview after exit");
    assert!(!after.leased, "exit releases the lease");
    remove_dir_all(&repo).ok();
}

/// A session enter that fails after the lease was taken gives the lease back:
/// the file it made would otherwise be residue nothing clears, and the next
/// session for that slug would read it as in use.
#[tokio::test]
async fn test_failed_enter_releases_lease() {
    let repo = make_repo(17);
    let (controller, _store, _cwd) = wired(&repo, 0);
    let slug = "feat-collide";
    let taken = Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["branch", &worktree_session::worktree_branch_name(slug)])
        .status()
        .expect("take the branch name");
    assert!(taken.success(), "the branch name is taken");

    let error = controller
        .enter(Some(slug.into()))
        .await
        .expect_err("the branch name is taken");

    assert!(
        matches!(error, WorktreeError::Collision { .. }),
        "expected Collision, got {error:?}"
    );
    assert!(
        !lease_path(&repo, slug).exists(),
        "the lease file is removed with the failed create"
    );
    drop(remove_dir_all(&repo));
}

/// A per-child enter follows the same rule: a create that fails after the
/// lease was taken leaves no lease file behind for the shutdown sweep.
#[tokio::test]
async fn test_child_enter_releases_lease() {
    let repo = make_repo(18);
    let (controller, _store, _cwd) = wired(&repo, 0);
    let slug = "child-collide";
    let taken = Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["branch", &worktree_session::worktree_branch_name(slug)])
        .status()
        .expect("take the branch name");
    assert!(taken.success(), "the branch name is taken");

    let error = controller.enter_for_child(slug.into()).await.err();

    assert!(
        matches!(error, Some(WorktreeError::Collision { .. })),
        "expected Collision, got {error:?}"
    );
    assert!(
        !lease_path(&repo, slug).exists(),
        "the lease file is removed with the failed create"
    );
    drop(remove_dir_all(&repo));
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
    remove_dir_all(&repo).ok();
}

/// The action a session's durable exit entry recorded.
fn exit_action(store: &SessionStore, controller: &WorktreeController) -> String {
    store
        .trajectory_tail(controller.session_id)
        .into_iter()
        .find_map(|entry| match entry.event {
            SessionEvent::WorktreeExit { action, .. } => Some(action),
            _ => None,
        })
        .expect("a worktree exit event")
}

/// The durable action names each state a stopped removal leaves, so a reader of
/// the log can tell the directory taken with the branch kept from the
/// registration gone with the directory and branch kept and recorded.
#[test]
fn test_exit_action_names_states() {
    let path = PathBuf::from("/repo/.houyicoder/worktrees/tree");
    let incomplete = WorktreeError::RemovalIncomplete {
        path: path.clone(),
        branch: "worktree-tree".into(),
        reason: "the ref is locked".into(),
    };
    let pending = WorktreeError::CleanupPending {
        path: path.clone(),
        reason: "a file inside it is in use".into(),
    };
    let failed = WorktreeError::RemovalFailed {
        path,
        reason: "git refused".into(),
    };

    assert_eq!(
        recorded_exit_action(Some(&incomplete), "remove"),
        "remove-incomplete"
    );
    assert_eq!(
        recorded_exit_action(Some(&pending), "remove"),
        "cleanup-pending"
    );
    assert_eq!(recorded_exit_action(Some(&failed), "remove"), "keep");
    assert_eq!(recorded_exit_action(None, "remove"), "remove");
    assert_eq!(recorded_exit_action(None, "keep"), "keep");
}

/// A removal Git refuses still ends the session: the cwd and fence already
/// went back to the repository root and the lease is gone, so reporting a
/// session inside the tree would describe a hold this controller no longer
/// has. The tree keeps its record, which is what a later removal acts on, and
/// the answer says the session ended so a caller does not keep working as if it
/// were still isolated.
#[tokio::test]
async fn test_failed_remove_ends_session() {
    let repo = make_repo(16);
    let (controller, store, _cwd) = wired(&repo, 0);
    let entered = controller
        .enter(Some("feat-held".into()))
        .await
        .expect("enter");
    let path = entered.worktree_path.clone();
    let locked = Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["worktree", "lock"])
        .arg(&path)
        .status()
        .expect("lock the worktree");
    assert!(locked.success(), "the worktree is locked");

    let error = controller
        .exit(ExitAction::Remove, true)
        .await
        .expect_err("a locked worktree is not removed");

    assert!(
        matches!(error, WorktreeError::RemovalFailed { .. }),
        "the removal failed and the session ended: {error:?}"
    );
    let message = error.to_string();
    assert!(
        message.contains("is locked"),
        "the answer carries why the removal stopped: {message}"
    );
    assert!(
        message.contains("session ended"),
        "the answer says the isolation is over: {message}"
    );
    assert!(
        !controller.current(),
        "the session ended even though the removal did not"
    );
    assert!(
        Path::new(&path).exists(),
        "the tree survives the refused removal"
    );
    assert!(
        preview_remove_worktree(&repo, "feat-held").is_ok(),
        "the record survives, so a later removal finds its object"
    );
    assert_eq!(
        exit_action(&store, &controller),
        "keep",
        "the durable action names the state the session ended in"
    );
    drop(remove_dir_all(&repo));
}

/// A branch Git will not delete after the directory is gone leaves the act half
/// done, and the durable event has to name that third state: recording it as a
/// plain keep would tell a later reader the tree is still on disk.
#[tokio::test]
async fn test_branch_lock_keeps_tree() {
    let repo = make_repo(19);
    let (controller, store, _cwd) = wired(&repo, 0);
    let entered = controller
        .enter(Some("feat-branch-held".into()))
        .await
        .expect("enter");
    let path = entered.worktree_path.clone();
    let branch = entered.worktree_branch.clone();
    let ref_lock = repo
        .join(".git")
        .join("refs")
        .join("heads")
        .join(format!("{branch}.lock"));
    create_dir_all(ref_lock.parent().expect("refs dir")).expect("refs dir");
    write(&ref_lock, "").expect("hold the branch ref lock");

    let error = controller
        .exit(ExitAction::Remove, true)
        .await
        .expect_err("the branch cannot be deleted");

    assert!(
        matches!(error, WorktreeError::RemovalFailed { .. }),
        "the act finished half done and the session ended: {error:?}"
    );
    assert!(
        !Path::new(&path).exists(),
        "the directory was removed before the branch step"
    );
    assert_eq!(
        exit_action(&store, &controller),
        "remove-incomplete",
        "the durable action names the branch-only residue"
    );
    drop(remove_dir_all(&repo));
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
    remove_dir_all(&repo).ok();
}

/// Two callers that enter at the same instant: one installs the session and
/// the other is refused, so only one fence guard is ever installed.
#[test]
fn test_second_enter_refused_concurrently() {
    let repo = make_repo(50);
    let (controller, _store, _cwd) = wired(&repo, 0);
    let start = Barrier::new(2);
    let results = thread::scope(|scope| {
        let handles: Vec<_> = ["feat-p", "feat-q"]
            .into_iter()
            .map(|slug| {
                let controller = Arc::clone(&controller);
                let start = &start;
                scope.spawn(move || {
                    start.wait();
                    controller.enter_prepared(Some(slug.into()))
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().expect("thread"))
            .collect::<Vec<_>>()
    });
    let (entered, refused): (Vec<_>, Vec<_>) = results.into_iter().partition(Result::is_ok);
    assert_eq!(entered.len(), 1, "one enter installs the session");
    assert_eq!(refused.len(), 1, "the other is refused");
    let winner = entered.into_iter().next().expect("winner").expect("ok");
    assert!(winner.path.exists(), "the winner's tree exists");
    let error = refused
        .into_iter()
        .next()
        .expect("loser")
        .expect_err("refused");
    assert!(
        error.to_string().contains("already in a worktree"),
        "the refusal names the session that won"
    );
    assert!(controller.current(), "the controller holds the winner");
    drop(controller);
    remove_dir_all(&repo).ok();
}

#[tokio::test]
async fn test_enter_refused_while_running() {
    let repo = make_repo(4);
    let (controller, _store, _cwd) = wired(&repo, 1);
    let err = controller
        .enter(Some("feat-b".into()))
        .await
        .expect_err("running exec refused");
    assert!(err.to_string().contains("still running"));
    remove_dir_all(&repo).ok();
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
    remove_dir_all(&repo).ok();
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
    remove_dir_all(&repo).ok();
}

#[tokio::test]
async fn test_tools_execute_through_controller() {
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
    assert!(exit.requires_approval_for(&json!({
        "action": "remove",
        "discard_changes": true
    })));
    assert!(!exit.requires_approval_for(&json!({"action": "remove"})));
    assert!(!exit.requires_approval_for(&json!({"action":"keep"})));
    let out = exit
        .execute(ToolCtx::new("call-2"), json!({"action":"keep"}))
        .await
        .expect("exit tool");
    assert_eq!(out["action"].as_str().unwrap(), "keep");
    remove_dir_all(&repo).ok();
}

/// Exiting isolation emits a user notice when the main branch ref changed.
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
    let captured = Arc::new(Mutex::new(Vec::<String>::new()));
    let cap = captured.clone();
    controller.set_user_notice_handler(Arc::new(move |event: UserNoticeEvent| {
        cap.lock().unwrap().push(event.message);
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
    remove_dir_all(&repo).ok();
}

mod child;
mod preview;
