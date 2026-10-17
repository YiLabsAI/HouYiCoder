//! Per-child worktree + fence tests. Reuses the sibling test module's
//! harness (make_repo, wired, wired_err); the helpers are pub(crate).

use houyicoder_api::hook_fire::HookFire;
use houyicoder_async::CancellationToken;
use houyicoder_context::{HookEventKind, HookFirePayload, SessionEvent, SessionId};
use houyicoder_memory::InMemoryBackend;
use houyicoder_session::SessionStore;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;
use tokio::task::spawn_blocking;
use tokio::time::timeout;

use crate::agent::ToolRegistry;
use crate::agent::multi_agent::bus_types::ChildRunMode;
use crate::agent::multi_agent::registry::IsolationMode;
use crate::agent::multi_agent::spawn::{SpawnError, SpawnRequest, TriggerSource, spawn_child};
use crate::agent::runner_config::RunnerConfig;
use crate::provider::test_support::FakeProvider;

use super::super::ChildCleanup;
use super::super::ExitAction;
use super::super::run_conservative_reclaim;
use super::super::tests::{make_repo, wired, wired_err};
use houyicoder_api::provider::ModelProvider;
use std::fs::remove_dir_all;
use std::fs::write;

/// enter_for_child creates a worktree + returns a guard, without entering a
/// user worktree session or repointing the parent cwd. The parent stays
/// out of a session and on its original cwd -- the per-child isolation
/// contract that distinguishes it from the user-facing enter.
#[tokio::test]
async fn test_enter_child_leaves_parent() {
    let repo = make_repo(10);
    let (controller, _store, cwd) = wired(&repo, 0);
    let cw = controller
        .enter_for_child("agent-deadbeef".into())
        .await
        .expect("enter_for_child");
    assert!(cw.worktree_path.exists(), "worktree dir created");
    assert_ne!(
        cw.worktree_path, repo,
        "child worktree is separate from repo"
    );
    assert!(!cw.worktree_branch.is_empty(), "branch name present");
    assert!(
        !controller.current(),
        "parent not in a user worktree session"
    );
    assert_eq!(*cwd.read().unwrap(), repo, "parent cwd unchanged");
    remove_dir_all(&repo).ok();
}

/// enter_for_child refuses while a sandbox exec is in flight (an in-flight
/// exec keeps its spawn-time profile and could escape the narrow). The
/// fail-closed guard the user-facing enter also enforces.
#[tokio::test]
async fn test_enter_child_refused_inflight() {
    let repo = make_repo(11);
    let (controller, _store, _cwd) = wired(&repo, 1);
    match controller.enter_for_child("agent-inflight".into()).await {
        Ok(_) => panic!("in-flight must refuse"),
        Err(e) => assert!(e.to_string().contains("in flight"), "{e}"),
    }
    remove_dir_all(&repo).ok();
}

/// spawn_child with Worktree isolation creates a child + a per-child
/// worktree, and the SubagentSpawn boundary records isolation="worktree"
/// (not the hardcoded "none" from the no-isolation path).
#[tokio::test]
async fn test_spawn_child_worktree_isolation() {
    let repo = make_repo(12);
    let (controller, store, _cwd) = wired(&repo, 0);
    let parent_sid = SessionId::new();
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::text("ok"));
    let req = SpawnRequest {
        parent_sid,
        parent_store: store.clone(),
        provider,
        tools: ToolRegistry::new(),
        config: RunnerConfig {
            model: "parent-model".into(),
            ..RunnerConfig::default()
        },
        subagent_type: "explore".to_string(),
        prompt: "find auth".to_string(),
        prompt_summary: "find auth".to_string(),
        trigger: TriggerSource::ModelTool {
            tool_call_id: "wt-call".to_string(),
        },
        depth: 0,
        isolation: IsolationMode::Worktree,
        worktree_controller: Some(controller.clone()),
        run_mode: ChildRunMode::Foreground,
        child_cancel: CancellationToken::new(),
        bus: None,
    };
    let handle = spawn_child(req).await.expect("spawn");
    assert!(handle.worktree.is_some(), "child carries a worktree guard");
    let events = store.trajectory_tail(parent_sid);
    let iso = events
        .iter()
        .find_map(|e| match &e.event {
            SessionEvent::SubagentSpawn { isolation, .. } => Some(isolation.clone()),
            _ => None,
        })
        .expect("spawn boundary present");
    assert_eq!(iso, "worktree", "boundary records the worktree isolation");
    drop(handle);
    remove_dir_all(&repo).ok();
}

/// A worktree fence failure rejects the spawn (fail-closed: no degradation
/// to no isolation) and writes no boundary -- no phantom spawn for resume
/// to reconcile against a child that never ran.
#[tokio::test]
async fn test_spawn_child_fence_fail() {
    let repo = make_repo(13);
    let controller = wired_err(&repo);
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let parent_sid = SessionId::new();
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::text("ok"));
    let req = SpawnRequest {
        parent_sid,
        parent_store: store.clone(),
        provider,
        tools: ToolRegistry::new(),
        config: RunnerConfig {
            model: "parent-model".into(),
            ..RunnerConfig::default()
        },
        subagent_type: "explore".to_string(),
        prompt: "find auth".to_string(),
        prompt_summary: "find auth".to_string(),
        trigger: TriggerSource::ModelTool {
            tool_call_id: "wt-call".to_string(),
        },
        depth: 0,
        isolation: IsolationMode::Worktree,
        worktree_controller: Some(controller),
        run_mode: ChildRunMode::Foreground,
        child_cancel: CancellationToken::new(),
        bus: None,
    };
    match spawn_child(req).await {
        Ok(_) => panic!("fence failure must reject, not spawn"),
        Err(e) => assert_eq!(e, SpawnError::WorktreeFenceNarrowFail),
    }
    assert!(
        store.trajectory_tail(parent_sid).is_empty(),
        "a rejected spawn writes no boundary"
    );
    remove_dir_all(&repo).ok();
}

/// cleanup_child on a clean worktree (no uncommitted files, no new commits)
/// restores the fence + removes the worktree dir -- terminal-state
/// auto-cleanup so finished children leave no stale trees behind.
#[tokio::test]
async fn test_cleanup_clean_worktree_removed() {
    let repo = make_repo(20);
    let (controller, _store, _cwd) = wired(&repo, 0);
    let cw = controller
        .enter_for_child("agent-clean".into())
        .await
        .expect("enter");
    let path = cw.worktree_path.clone();
    let outcome = controller.cleanup_child(cw).await;
    match outcome {
        ChildCleanup::Removed { worktree_path } => assert_eq!(worktree_path, path),
        ChildCleanup::Kept { .. } => panic!("clean worktree must be removed, not kept"),
    }
    assert!(!path.exists(), "clean worktree dir removed");
    remove_dir_all(&repo).ok();
}

/// cleanup_child on a dirty worktree (uncommitted changes) restores the
/// fence + keeps the worktree, reporting path + branch so the caller can
/// continue on the branch -- never silently destroy work the child produced.
#[tokio::test]
async fn test_cleanup_dirty_worktree_kept() {
    let repo = make_repo(21);
    let (controller, _store, _cwd) = wired(&repo, 0);
    let cw = controller
        .enter_for_child("agent-dirty".into())
        .await
        .expect("enter");
    write(cw.worktree_path.join("uncommitted.txt"), "dirty").expect("write");
    let path = cw.worktree_path.clone();
    let outcome = controller.cleanup_child(cw).await;
    match outcome {
        ChildCleanup::Kept {
            worktree_path,
            worktree_branch,
        } => {
            assert_eq!(worktree_path, path);
            assert!(!worktree_branch.is_empty(), "branch reported back");
        }
        ChildCleanup::Removed { .. } => panic!("dirty worktree must be kept, not removed"),
    }
    assert!(path.exists(), "dirty worktree dir preserved");
    remove_dir_all(&repo).ok();
}

/// reclaim_abandoned removes a clean per-child worktree that was never
/// cleaned up (a force-aborted child dropped its driver future before its
/// finalize cleanup tail), so the shutdown owner recycles the abandoned tree
/// instead of leaking its dir + branch.
#[tokio::test]
async fn test_reclaim_removes_clean_worktree() {
    let repo = make_repo(22);
    let (controller, _store, _cwd) = wired(&repo, 0);
    let cw = controller
        .enter_for_child("agent-abandoned".into())
        .await
        .expect("enter");
    let path = cw.worktree_path.clone();
    assert!(path.exists(), "worktree dir created");
    assert!(path.join(".git").exists(), "worktree is a linked checkout");
    drop(cw);
    // The drop restored the fence only; the tree is still on disk until the
    // shutdown owner reclaims it.
    assert!(path.exists(), "drop leaves the worktree for reclaim");
    controller.reclaim_abandoned().await;
    assert!(!path.exists(), "abandoned clean worktree removed");
    remove_dir_all(&repo).ok();
}

/// reclaim_abandoned preserves a dirty per-child worktree (uncommitted
/// changes) the same way cleanup_child does: an aborted child that produced
/// work does not have that work destroyed by the shutdown sweep.
#[tokio::test]
async fn test_reclaim_keeps_dirty_worktree() {
    let repo = make_repo(23);
    let (controller, _store, _cwd) = wired(&repo, 0);
    let cw = controller
        .enter_for_child("agent-abandon-dirty".into())
        .await
        .expect("enter");
    write(cw.worktree_path.join("uncommitted.txt"), "dirty").expect("write");
    let path = cw.worktree_path.clone();
    drop(cw);
    controller.reclaim_abandoned().await;
    assert!(path.exists(), "dirty abandoned worktree preserved");
    remove_dir_all(&repo).ok();
}

/// The shutdown sweep removes an abandoned clean worktree without firing
/// WorktreeRemove, unlike the normal cleanup path: an aborted child's silent
/// recovery is physical-only, so the lone event is the WorktreeCreate its
/// enter fired.
#[tokio::test]
async fn test_reclaim_skips_remove_hook() {
    let repo = make_repo(28);
    let (controller, _store, _cwd) = wired(&repo, 0);
    let events = Arc::new(Mutex::new(Vec::new()));
    controller.set_hook_fire(Some(Arc::new(RecordingHookFire {
        events: events.clone(),
    }) as Arc<dyn HookFire>));
    let cw = controller
        .enter_for_child("agent-abandon-clean".into())
        .await
        .expect("enter");
    let path = cw.worktree_path.clone();
    drop(cw);
    controller.reclaim_abandoned().await;
    assert!(!path.exists(), "abandoned clean worktree removed");
    let fired = events.lock().expect("events lock").clone();
    assert!(
        fired.contains(&HookEventKind::WorktreeCreate),
        "enter fired WorktreeCreate: {fired:?}"
    );
    assert!(
        !fired.contains(&HookEventKind::WorktreeRemove),
        "sweep does not fire WorktreeRemove: {fired:?}"
    );
    remove_dir_all(&repo).ok();
}

/// cleanup_child removes the reclaim record, so a later shutdown sweep finds
/// nothing to re-delete: a normal cleanup produces exactly one removal and one
/// WorktreeRemove, never a double delete or a second event.
#[tokio::test]
async fn test_cleanup_removes_worktree_once() {
    let repo = make_repo(24);
    let (controller, _store, _cwd) = wired(&repo, 0);
    let events = Arc::new(Mutex::new(Vec::new()));
    controller.set_hook_fire(Some(Arc::new(RecordingHookFire {
        events: events.clone(),
    }) as Arc<dyn HookFire>));
    let cw = controller
        .enter_for_child("agent-once".into())
        .await
        .expect("enter");
    let path = cw.worktree_path.clone();
    let outcome = controller.cleanup_child(cw).await;
    assert!(
        matches!(outcome, ChildCleanup::Removed { .. }),
        "clean worktree removed"
    );
    controller.reclaim_abandoned().await;
    let fired = events.lock().expect("events lock").clone();
    assert_eq!(
        fired
            .iter()
            .filter(|e| **e == HookEventKind::WorktreeRemove)
            .count(),
        1,
        "one WorktreeRemove fires: {fired:?}"
    );
    assert!(!path.exists(), "worktree removed exactly once");
    remove_dir_all(&repo).ok();
}

/// cleanup_child on a committed worktree (new commits beyond the parent tip)
/// preserves it: a child's committed output is real work, never auto-deleted.
#[tokio::test]
async fn test_cleanup_committed_worktree_kept() {
    let repo = make_repo(25);
    let (controller, _store, _cwd) = wired(&repo, 0);
    let cw = controller
        .enter_for_child("agent-committed".into())
        .await
        .expect("enter");
    drop(
        Command::new("git")
            .arg("-C")
            .arg(&cw.worktree_path)
            .args(["commit", "--allow-empty", "-m", "child-wip", "-q"])
            .status(),
    );
    let path = cw.worktree_path.clone();
    let outcome = controller.cleanup_child(cw).await;
    match outcome {
        ChildCleanup::Kept {
            worktree_path,
            worktree_branch,
        } => {
            assert_eq!(worktree_path, path);
            assert!(!worktree_branch.is_empty(), "branch reported back");
        }
        ChildCleanup::Removed { .. } => panic!("committed worktree must be kept, not removed"),
    }
    assert!(path.exists(), "committed worktree dir preserved");
    remove_dir_all(&repo).ok();
}

/// A worktree whose state cannot be verified (the checkout is gone before
/// cleanup) is kept, not treated as clean: the fail-closed change check errs
/// toward preserving, and the fence still restores without a panic.
#[tokio::test]
async fn test_cleanup_missing_worktree_kept() {
    let repo = make_repo(26);
    let (controller, _store, _cwd) = wired(&repo, 0);
    let cw = controller
        .enter_for_child("agent-gone".into())
        .await
        .expect("enter");
    let path = cw.worktree_path.clone();
    remove_dir_all(&path).expect("remove checkout");
    let outcome = controller.cleanup_child(cw).await;
    assert!(
        matches!(outcome, ChildCleanup::Kept { .. }),
        "unverifiable worktree kept"
    );
    remove_dir_all(&repo).ok();
}

/// Resuming an existing child worktree (the same slug re-entered) never treats
/// the prior child's output as "no new changes" and deletes it: a resumed tree
/// is not registered for auto-reclaim, so it is always kept.
#[tokio::test]
async fn test_cleanup_reused_worktree_kept() {
    let repo = make_repo(27);
    let (controller, _store, _cwd) = wired(&repo, 0);
    // Create the tree through the user-facing enter (outside the per-child
    // reclaim map), then commit output that advances its tip.
    let entered = controller
        .enter(Some("agent-reuse".into()))
        .await
        .expect("enter");
    let path = PathBuf::from(&entered.worktree_path);
    drop(
        Command::new("git")
            .arg("-C")
            .arg(&path)
            .args(["commit", "--allow-empty", "-m", "prior-output", "-q"])
            .status(),
    );
    controller
        .exit(ExitAction::Keep, false)
        .await
        .expect("exit keep");
    // The same slug now resolves to the existing tree; its committed work must
    // read as preserved, not as a clean tree safe to delete.
    let resumed = controller
        .enter_for_child("agent-reuse".into())
        .await
        .expect("resume");
    let outcome = controller.cleanup_child(resumed).await;
    assert!(
        matches!(outcome, ChildCleanup::Kept { .. }),
        "reused worktree kept"
    );
    assert!(path.exists(), "prior output preserved");
    remove_dir_all(&repo).ok();
}

/// A HookFire that records the event kinds it was called with, for asserting
/// the controller fires at the per-child enter and cleanup boundaries.
struct RecordingHookFire {
    events: Arc<Mutex<Vec<HookEventKind>>>,
}
impl HookFire for RecordingHookFire {
    fn fire(
        &self,
        event: HookEventKind,
        _payload: HookFirePayload,
    ) -> houyicoder_async::PFut<'_, ()> {
        self.events.lock().expect("recorder lock").push(event);
        Box::pin(async {})
    }
}

/// enter_for_child fires WorktreeCreate at the per-child spawn boundary, so
/// the controller covers the spawn path (not only the user-facing enter tool).
#[tokio::test]
async fn test_enter_fires_worktree_create() {
    let repo = make_repo(30);
    let (controller, _store, _cwd) = wired(&repo, 0);
    let events = Arc::new(Mutex::new(Vec::new()));
    controller.set_hook_fire(Some(Arc::new(RecordingHookFire {
        events: events.clone(),
    }) as Arc<dyn HookFire>));
    let _cw = controller
        .enter_for_child("agent-fire".into())
        .await
        .expect("enter");
    let fired = events.lock().expect("events lock").clone();
    assert!(
        fired.contains(&HookEventKind::WorktreeCreate),
        "enter_for_child fires WorktreeCreate: {fired:?}"
    );
    remove_dir_all(&repo).ok();
}

/// cleanup_child on a removed (clean) worktree fires WorktreeRemove at the
/// boundary, pairing with the WorktreeCreate the enter fired.
#[tokio::test]
async fn test_cleanup_removed_fires_remove() {
    let repo = make_repo(31);
    let (controller, _store, _cwd) = wired(&repo, 0);
    let events = Arc::new(Mutex::new(Vec::new()));
    controller.set_hook_fire(Some(Arc::new(RecordingHookFire {
        events: events.clone(),
    }) as Arc<dyn HookFire>));
    let cw = controller
        .enter_for_child("agent-clean".into())
        .await
        .expect("enter");
    let outcome = controller.cleanup_child(cw).await;
    assert!(
        matches!(outcome, ChildCleanup::Removed { .. }),
        "clean worktree removed"
    );
    let fired = events.lock().expect("events lock").clone();
    assert!(
        fired.contains(&HookEventKind::WorktreeCreate)
            && fired.contains(&HookEventKind::WorktreeRemove),
        "enter + cleanup fire Create + Remove: {fired:?}"
    );
    remove_dir_all(&repo).ok();
}

/// The user-facing enter + exit(Remove) tool path fires WorktreeCreate +
/// WorktreeRemove at the controller boundary, so the tool path and the spawn
/// path share one fire point.
#[tokio::test]
async fn test_toolpath_fires_create_remove() {
    let repo = make_repo(40);
    let (controller, _store, _cwd) = wired(&repo, 0);
    let events = Arc::new(Mutex::new(Vec::new()));
    controller.set_hook_fire(Some(Arc::new(RecordingHookFire {
        events: events.clone(),
    }) as Arc<dyn HookFire>));
    controller
        .enter(Some("user-wt".into()))
        .await
        .expect("enter");
    controller
        .exit(ExitAction::Remove, true)
        .await
        .expect("exit remove");
    let fired = events.lock().expect("events lock").clone();
    assert!(
        fired.contains(&HookEventKind::WorktreeCreate),
        "enter fires WorktreeCreate: {fired:?}"
    );
    assert!(
        fired.contains(&HookEventKind::WorktreeRemove),
        "exit Remove fires WorktreeRemove: {fired:?}"
    );
    remove_dir_all(&repo).ok();
}

/// reclaim_abandoned awaits an already-launched reclaim instead of relaunching
/// Git. Models the Git ref-lock counterexample: a child's cleanup launched the
/// conservative reclaim (worktree remove + branch delete) and was then aborted
/// mid-await, leaving the blocking Git task still running with the branch ref
/// locked by another process. The shutdown sweep must wait on that original
/// task, not start a fresh directory re-check that returns before the branch
/// delete completes.
#[tokio::test]
async fn test_reclaim_awaits_launched_task() {
    let repo = make_repo(50);
    let (controller, _store, _cwd) = wired(&repo, 0);
    let cw = controller
        .enter_for_child("agent-launched-reclaim".into())
        .await
        .expect("enter");
    let path = cw.worktree_path.clone();
    drop(cw);

    // Simulate a launched reclaim that is parked (a Git ref lock held by
    // another process). Mark the slot launched and post the blocking task
    // that holds the completion: it parks on the latch, then runs the real
    // conservative reclaim, then sets the outcome. This is the blocking task
    // cleanup_child would have launched, with the awaiter already aborted.
    // The handle is kept only so the failure path can observe the task to
    // completion before asserting.
    let latch = Arc::new((Mutex::new(false), Condvar::new()));
    let latch_for_task = latch.clone();
    let task = {
        let mut map = controller.pending_reclaims.lock().expect("reclaims lock");
        let slot = map
            .get_mut("agent-launched-reclaim")
            .expect("reclaim record present");
        assert!(!slot.launched, "fresh slot not yet launched");
        slot.launched = true;
        let completion = Arc::clone(&slot.completion);
        let record = slot.record.clone();
        spawn_blocking(move || {
            // Park until released, modeling a blocked branch delete while the
            // awaiter was aborted.
            {
                let (lock, cvar) = &*latch_for_task;
                let mut guard = lock.lock().expect("latch lock");
                while !*guard {
                    guard = cvar.wait(guard).expect("latch wait");
                }
            }
            let outcome = run_conservative_reclaim(&record);
            completion.set(outcome);
        })
    };

    // The sweep must await the launched completion, not relaunch Git. A short
    // timeout must not complete: the task is still parked. The old
    // drain-and-relaunch path ran a fresh check that returned before this
    // task finished. Record the verdict instead of failing in place: the
    // latch release and the task observation below must run on every path,
    // or a failure would leave the task parked and runtime shutdown would
    // wait on it, turning a red test into a hang.
    let reclaim = controller.reclaim_abandoned();
    tokio::pin!(reclaim);
    let returned_early = timeout(Duration::from_millis(200), &mut reclaim)
        .await
        .is_ok();

    // Release the latch; the launched reclaim runs and lands the outcome.
    {
        let (lock, cvar) = &*latch;
        let mut guard = lock.lock().expect("latch lock");
        *guard = true;
        cvar.notify_one();
    }
    task.await.expect("planted task completes");

    assert!(
        !returned_early,
        "reclaim_abandoned returned before the launched task; it relaunches instead of awaiting the launched completion"
    );

    // The sweep observes the outcome and returns.
    (&mut reclaim).await;
    assert!(
        !path.exists(),
        "launched task removed the tree, observed by the sweep"
    );
    remove_dir_all(&repo).ok();
}
