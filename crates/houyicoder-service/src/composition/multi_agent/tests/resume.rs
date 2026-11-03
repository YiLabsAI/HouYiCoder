//! Continuing a child session by id.

use super::*;

/// A resume id that is not a session id is refused before the spawn has any
/// side effect: the host cannot guess which session the model meant, and
/// starting a fresh child would report success for a continuation that never
/// happened.
#[tokio::test]
async fn test_resume_rejects_bad_id() {
    let (runtime, store, parent_sid) = runtime_with_text_child("done");
    let ctx = ToolCtx::new("c1").with_session(parent_sid);
    let mut args = SpawnArgs::new("explore", "task", "task");
    args.resume = Some("not a session id".to_string());

    let error = runtime
        .spawn(&ctx, args)
        .await
        .expect_err("the id is refused");

    assert!(
        matches!(error, SpawnFailure::InvalidResumeTarget { .. }),
        "the refusal names the resume target: {error:?}"
    );
    assert!(
        store
            .trajectory_tail(parent_sid)
            .iter()
            .all(|e| !matches!(e.event, SessionEvent::SubagentSpawn { .. })),
        "a refused resume writes no spawn boundary"
    );
}

/// A resume of a child that is still running is refused. Both spawns would
/// drive one session's log at once, so neither would see the other's turns and
/// the later one would interleave its conversation with work in progress.
#[tokio::test]
async fn test_resume_rejects_running_child() {
    let (provider, entered) = StallForeverProvider::new();
    let (runtime, _store, parent_sid) = runtime_with(provider, None, ToolRegistry::new());
    let ctx = ToolCtx::new("c1").with_session(parent_sid);
    let mut args = SpawnArgs::new("explore", "task", "task");
    args.run_in_background = true;
    let first = runtime.spawn(&ctx, args).await.expect("the child starts");
    entered.await.expect("the child enters its model call");
    let mut again = SpawnArgs::new("explore", "more", "more");
    again.resume = Some(first.child_session_id.clone());

    let error = runtime
        .spawn(&ctx, again)
        .await
        .expect_err("a running child is not resumable");

    assert!(
        matches!(error, SpawnFailure::InvalidResumeTarget { .. }),
        "the refusal names the resume target: {error:?}"
    );
    runtime.kill_child(&first.child_session_id);
    runtime.join_children().await;
}

/// A resume of a child this parent started continues that session instead of
/// minting a new one, so the work its log already holds is what the child
/// continues from.
#[tokio::test]
async fn test_resume_continues_child() {
    let (runtime, _store, parent_sid) = runtime_with_text_child("done");
    let ctx = ToolCtx::new("c1").with_session(parent_sid);
    let first = runtime
        .spawn(&ctx, SpawnArgs::new("explore", "task", "task"))
        .await
        .expect("the first delegation runs");
    let mut args = SpawnArgs::new("explore", "more", "more");
    args.resume = Some(first.child_session_id.clone());

    let second = runtime.spawn(&ctx, args).await.expect("the resume runs");

    assert_eq!(
        second.child_session_id, first.child_session_id,
        "the resumed child keeps the session it was asked to continue"
    );
}
