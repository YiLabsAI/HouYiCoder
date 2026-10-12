//! Shutdown-owner worktree reclaim: a child force-aborted before its finalize
//! cleanup tail leaves its per-child worktree in the controller's reclaim
//! map, and join_children removes it before the parent lease is released.

use std::env::temp_dir;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, id};
use std::sync::Arc;

use houyicoder_api::sandbox::{SandboxSession, WorktreeFenceGuard};
use houyicoder_api::spawn::{SpawnArgs, SpawnHandle};
use houyicoder_api::tool::ToolCtx;
use houyicoder_async::PFut;
use houyicoder_context::{ExecConfig, ExecResult, SandboxError, SessionId};
use houyicoder_core::agent::ToolRegistry;
use houyicoder_core::agent::multi_agent::registry::AgentRegistry;
use houyicoder_core::agent::multi_agent::registry::BuiltInRegistry;
use houyicoder_core::agent::multi_agent::registry::built_in_all;
use houyicoder_core::agent::runner_config::RunnerConfig;
use houyicoder_core::agent::worktree_controller::WorktreeController;
use houyicoder_memory::{InMemoryBackend, InMemoryDescriptorStore};
use houyicoder_provider::FakeProvider;
use houyicoder_session::SessionStore;

use super::super::{MultiAgentDeps, MultiAgentRuntime};
use super::BlockingStartHook;

/// A sandbox whose narrow returns a no-op restore guard. The worktree
/// controller only exercises exec_with_config, workspace_root, and
/// narrow_to_worktree on this path; the remaining methods fall back to the
/// trait defaults.
struct StubSandbox;

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

    fn workspace_root(&self) -> Arc<Path> {
        Arc::from(PathBuf::from("/"))
    }

    fn narrow_to_worktree(
        &self,
        _worktree: &Path,
        _git_common_dir: &Path,
    ) -> Result<WorktreeFenceGuard, SandboxError> {
        Ok(WorktreeFenceGuard::new(Box::new(|| Ok(()))))
    }
}

/// A disposable git repo with one commit, so a per-child worktree can be
/// linked and cleanly reclaimed against it.
#[expect(
    clippy::disallowed_methods,
    reason = "test fixture spawn, not model-driven"
)]
fn make_repo(tag: u64) -> PathBuf {
    let dir = temp_dir().join(format!("houyi-multiagent-wt-{}-{tag}", id()));
    drop(fs::remove_dir_all(&dir));
    fs::create_dir_all(&dir).expect("repo dir");
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

/// A background child wedged in its Start hook never reaches its finalize
/// cleanup tail once the shutdown join force-aborts it, so its per-child
/// worktree is only reachable through the controller's reclaim map. The join,
/// as the shutdown owner, reclaims that clean tree before it returns, so the
/// worktrees dir is empty by the time the parent lease would release.
#[tokio::test]
async fn test_join_reclaims_abandoned_worktree() {
    let repo = make_repo(1);
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let registry: Arc<dyn AgentRegistry> = Arc::new(BuiltInRegistry::from_agents(built_in_all()));
    let sandbox: Arc<dyn SandboxSession> = Arc::new(StubSandbox);
    let controller = Arc::new(WorktreeController::new(
        repo.clone(),
        repo.join(".git"),
        sandbox,
        store.clone(),
        SessionId::new(),
    ));
    let runtime = MultiAgentRuntime::new(MultiAgentDeps {
        registry,
        store: store.clone(),
        provider: Arc::new(FakeProvider::text("unused")),
        tools: ToolRegistry::new(),
        config: RunnerConfig::default(),
        worktree_controller: Some(controller),
        workspace: Some(repo.clone()),
        bus: None,
        descriptor_store: Some(Arc::new(InMemoryDescriptorStore::new())),
    });
    let hook = Arc::new(BlockingStartHook {
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let parent_sid = SessionId::new();
    let ctx = ToolCtx::new("c1")
        .with_session(parent_sid)
        .with_hook_fire(hook.clone());
    let mut args = SpawnArgs::new("explore", "task", "task");
    args.isolation = "worktree".to_string();
    args.run_in_background = true;
    runtime
        .spawn(&ctx, args)
        .await
        .expect("background spawn launches");
    // The child's worktree exists before the driver reaches the Start hook it
    // is now wedged on, and the driver's finalize tail (which would clean it)
    // never runs once the join aborts it.
    let worktrees_dir = repo.join(".houyicoder").join("worktrees");
    assert_eq!(
        fs::read_dir(&worktrees_dir).map(|d| d.count()).unwrap_or(0),
        1,
        "one per-child worktree exists on disk"
    );
    runtime.join_children().await;
    assert_eq!(
        fs::read_dir(&worktrees_dir).map(|d| d.count()).unwrap_or(0),
        0,
        "the abandoned worktree is reclaimed before the join returns"
    );
    drop(fs::remove_dir_all(&repo));
}
