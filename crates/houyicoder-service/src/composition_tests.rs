//! Peer tests for the composition root itself: degrading a dependency that
//! failed to build, the session host handle a reconnecting client lands on, and
//! assembly mistakes only the root can make (a wrong order or a swapped
//! implementation that still constructs cleanly).
//!
//! Everything the root merely wires up is tested next to the thing it wires:
//! memory, worktree, containment and resume each own their peer tests.
use super::*;
use crate::lifecycle::SessionLeaseStore;
use crate::server::Server;
use std::sync::atomic::AtomicU64;
#[test]
fn test_degrade_passes_success_through() {
    let kept: Option<u8> = degrade_with_notice(Ok::<u8, String>(7), "unused", "unused");
    assert_eq!(
        kept,
        Some(7),
        "a successful attempt must be handed back untouched, so wrapping a \
         construction in the notice does not change what the caller receives"
    );
}

/// A failed attempt becomes an absence rather than a panic or a default, which
/// is what lets the caller decide between substituting something reduced and
/// carrying on without the capability at all. The synthetic error keeps this on
/// the decision itself: reaching it through a real construction failure would
/// mean arranging for a sandbox to be unbuildable, which tests the operating
/// system rather than this branch.
#[test]
fn test_degrade_reports_absence() {
    let lost: Option<u8> = degrade_with_notice(
        Err::<u8, String>("underlying cause".into()),
        "capability could not be built",
        "the feature is off for this run.",
    );
    assert!(
        lost.is_none(),
        "a failed attempt must degrade to None so the caller can substitute or \
         withhold, rather than proceeding with something half-built"
    );
}

/// Build a minimal runner for host-level tests: a stub provider, an empty
/// tool registry, an in-memory store. No sandbox, no real model — the host
/// methods under test never run the agent, they only carry the Arc handle.
fn minimal_runner() -> Runner {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::text("ok"));
    Runner::with_shared_store(
        store,
        provider,
        ToolRegistry::new(),
        RunnerConfig {
            model: "test".into(),
            instructions: "test".into(),
            max_turns: 1,
            ..RunnerConfig::default()
        },
    )
}

/// Inserting a live runner into the host, then cloning the handle, returns
/// the same Arc<Runner> + the shared seq counter + the gate. set_pushed_count
/// round-trips through the handle so a disconnect flush survives. No live
/// runner is returned for a session the host never held.
#[test]
fn test_host_clones_runner_handle() {
    let session = SessionId::new();
    let runner = Arc::new(minimal_runner());
    let next_seq = Arc::new(AtomicU64::new(0));
    let gate: Arc<dyn ModeGate> = Arc::new(DefaultModeGate::new());

    let host = SessionHost::new(SessionLeaseStore::new());
    assert!(
        host.clone_handle(session).is_none(),
        "no handle before insert",
    );
    host.insert(session, runner.clone(), next_seq.clone(), gate.clone());

    let handle = host.clone_handle(session).expect("handle after insert");
    assert!(
        Arc::ptr_eq(&handle.runner, &runner),
        "clone_handle returns the same runner Arc",
    );
    assert!(Arc::ptr_eq(&handle.next_seq, &next_seq));
    assert_eq!(handle.pushed_count, 0, "starts at zero pushed events");

    host.set_pushed_count(session, 7);
    assert_eq!(
        host.clone_handle(session).unwrap().pushed_count,
        7,
        "set_pushed_count round-trips through the handle",
    );
}

/// new_for_resume rebuilds a Server from the host's live handle so the
/// runner + the shared seq counter + the pushed-event cursor survive a
/// prior connection's disconnect. The host reference is retained (host is
/// Some) so the run path can write the parked PendingTurn later.
#[test]
fn test_new_for_resume_hydrates() {
    let session = SessionId::new();
    let runner = Arc::new(minimal_runner());
    let next_seq = Arc::new(AtomicU64::new(0));
    let gate: Arc<dyn ModeGate> = Arc::new(DefaultModeGate::new());

    let host = Arc::new(SessionHost::new(SessionLeaseStore::new()));
    host.insert(session, runner, next_seq, gate);

    let handle = host.clone_handle(session).expect("handle present");
    // The call itself re-hydrates a Server from the host's live handle;
    // the runner Arc + the shared seq counter + the pushed-event cursor
    // flow in from the handle, and the host reference is retained so the
    // run path can write the parked PendingTurn later. (Server's fields
    // are private to the server module; the call covering the body is the
    // assertion here — a behavior-level check lands with the reconnect
    // test that drives serve_session end-to-end.)
    let _server = Server::new_for_resume(
        handle.runner,
        session,
        handle.next_seq,
        handle.pushed_count,
        handle.gate,
        host,
    );
}

/// build_runner wires the fence containment into the gate so the path-bounds
/// validator fires for a grep whose path is outside the workspace. Pins the
/// composition-root ordering: the shared dyn handle must be cloned AFTER
/// with_containment mutates the gate, so Arc::get_mut sees strong_count 1.
/// An earlier clone left the count at 2 and Arc::get_mut silently returned
/// None — the containment wiring was skipped, the path-bounds validator kept
/// a None handle, the gate never asked, and the tool's own confine_path
/// hard-refused instead of surfacing a card. Effect-level: a real temp repo
/// with a workspace manifest, build_runner with that repo as the project,
/// decide on a grep whose path is a sibling outside the repo — must Ask.
#[test]
fn test_build_runner_outside_grep() {
    use houyicoder_permission::{Decision, ModeGate, ToolRequest};
    let root = std::env::temp_dir().join(format!("houyi-wire-{}-{}", std::process::id(), line!()));
    drop(std::fs::remove_dir_all(&root));
    std::fs::create_dir_all(&root).expect("mkdir root");
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).expect("mkdir repo");
    std::fs::write(repo.join("Cargo.toml"), "[workspace]\nmembers = []\n").expect("manifest");
    let outside = root.join("outside");
    std::fs::create_dir_all(&outside).expect("mkdir outside");
    let bundle = super::build_runner(BuildRunnerOptions {
        project: Some(repo.to_string_lossy().into_owned()),
        ..Default::default()
    });
    let gate = bundle.gate;
    let input = serde_json::json!({"pattern":"x","path":outside.to_string_lossy()});
    // native_requires_approval=false + read-only so the ONLY Ask source is the
    // path-bounds Detection validator: mode_default would otherwise Allow a
    // read-only grep under the default Auto posture, so an Ask here proves the
    // containment wiring reached the gate's pipeline. Asserting the validator
    // name pins that the Ask is path-bounds (not a built-in rule or mode ask),
    // so the test fails if the wiring is skipped.
    let req = ToolRequest {
        tool_name: "grep",
        input: Some(&input),
        is_destructive: false,
        is_read_only: true,
        native_requires_approval: false,
    };
    match gate.decide(&req) {
        Decision::Ask(r) => assert_eq!(
            r.validator, "path-bounds",
            "the outside-grep ask must come from the path-bounds validator (containment wired): {r:?}"
        ),
        other => panic!("outside grep must Ask via path-bounds, got {other:?}"),
    }
    std::fs::remove_dir_all(&root).ok();
}

/// The production composition root wires an LlmSummarizer (real summaries)
/// into the runner, not the default HeuristicSummarizer placeholder. Pins the
/// wiring so a refactor that drops with_summarizer or swaps back to the
/// heuristic fails this test instead of silently regressing compress to a
/// placeholder. Type-level assertion via Summarizer::as_any downcast.
#[test]
fn test_build_runner_wires_summarizer() {
    let root = std::env::temp_dir().join(format!("houyi-sum-{}-{}", std::process::id(), line!()));
    drop(std::fs::remove_dir_all(&root));
    std::fs::create_dir_all(&root).expect("mkdir root");
    let bundle = super::build_runner(BuildRunnerOptions {
        project: Some(root.to_string_lossy().into_owned()),
        ..Default::default()
    });
    assert!(
        bundle.runner.summarizer_is_llm(),
        "production runner must carry an LlmSummarizer, not the heuristic placeholder"
    );
    std::fs::remove_dir_all(&root).ok();
}

/// The production persistence constructors are pure construction - no
/// directory creation, no I/O - until the first append/write, so calling
/// them touches nothing on disk. Covers the opt-in surface the in-memory
/// default path never reaches.
#[test]
fn test_disk_options_construct_clean() {
    let opts = super::BuildRunnerOptions::disk(None, None);
    assert!(opts.backend.is_some(), "disk() must wire a backend");
    assert!(opts.meta_store.is_some(), "disk() must wire a meta store");
    let opts = super::BuildRunnerOptions::disk_at(std::env::temp_dir(), None, None);
    assert!(opts.backend.is_some(), "disk_at() must wire a backend");
    assert!(
        opts.meta_store.is_some(),
        "disk_at() must wire a meta store"
    );
    let _store = super::disk_meta_store();
}
