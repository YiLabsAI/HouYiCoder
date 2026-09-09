//! Peer tests for the composition root itself: degrading a dependency that
//! failed to build, the session host handle a reconnecting client lands on, and
//! assembly mistakes only the root can make (a wrong order or a swapped
//! implementation that still constructs cleanly).
//!
//! Everything the root merely wires up is tested next to the thing it wires:
//! memory, worktree, containment and resume each own their peer tests.
use super::*;
use crate::lifecycle::SessionLeaseStore;
use crate::server::{EventSequencer, Server};
use houyicoder_protocol::frontend::FrontendEvent;
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

/// A cloned host handle retains the same runner and event sequencer.
#[test]
fn test_host_clones_runner_handle() {
    let session = SessionId::new();
    let runner = Arc::new(minimal_runner());
    let event_sequencer = EventSequencer::new();
    let gate: Arc<dyn ModeGate> = Arc::new(DefaultModeGate::new());

    let host = SessionHost::new(SessionLeaseStore::new());
    assert!(
        host.clone_handle(session).is_none(),
        "no handle before insert",
    );
    host.insert(
        session,
        runner.clone(),
        event_sequencer.clone(),
        gate.clone(),
        std::sync::Arc::new(tokio::sync::Notify::new()),
    );

    let handle = host.clone_handle(session).expect("handle after insert");
    assert!(
        Arc::ptr_eq(&handle.runner, &runner),
        "clone_handle returns the same runner Arc",
    );
    drop(
        handle
            .event_sequencer
            .sequence_reliable(FrontendEvent::SystemLine {
                text: "shared".into(),
            }),
    );
    assert_eq!(
        event_sequencer.next_seq(),
        1,
        "cloned handle shares the sequencer state",
    );
}

/// new_for_resume rebuilds a Server from the retained session state.
#[test]
fn test_new_for_resume_hydrates() {
    let session = SessionId::new();
    let runner = Arc::new(minimal_runner());
    let event_sequencer = EventSequencer::new();
    let gate: Arc<dyn ModeGate> = Arc::new(DefaultModeGate::new());

    let host = Arc::new(SessionHost::new(SessionLeaseStore::new()));
    host.insert(
        session,
        runner,
        event_sequencer,
        gate,
        std::sync::Arc::new(tokio::sync::Notify::new()),
    );

    let handle = host.clone_handle(session).expect("handle present");
    let _server = Server::new_for_resume(
        handle.runner,
        session,
        handle.event_sequencer,
        handle.gate,
        host,
        handle.append_notify,
    );
}

/// Paths outside the workspace must reach the gate's containment validator.
#[test]
fn test_external_approval() {
    use houyicoder_permission::{Decision, ModeGate, ToolRequest};
    let root = std::env::temp_dir().join(format!("houyi-wire-{}-{}", std::process::id(), line!()));
    drop(std::fs::remove_dir_all(&root));
    std::fs::create_dir_all(&root).expect("mkdir root");
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).expect("mkdir repo");
    std::fs::write(repo.join("Cargo.toml"), "[workspace]\nmembers = []\n").expect("manifest");
    let external_path = root.join("external");
    std::fs::create_dir_all(&external_path).expect("mkdir external path");
    let bundle = super::build_runner(BuildRunnerOptions {
        project: Some(repo.to_string_lossy().into_owned()),
        ..Default::default()
    });
    let gate = bundle.gate;
    let input = serde_json::json!({
        "pattern": "x",
        "path": external_path.to_string_lossy()
    });
    let req = ToolRequest {
        tool_name: "grep",
        input: Some(&input),
        is_destructive: false,
        is_read_only: true,
        native_requires_approval: false,
    };
    match gate.decide(&req) {
        Decision::Ask(reason) => assert_eq!(reason.validator, "path-bounds"),
        other => panic!("external path must require approval, got {other:?}"),
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
    assert!(
        opts.descriptor_store.is_some(),
        "disk() must wire a descriptor store"
    );
    let opts = super::BuildRunnerOptions::disk_at(std::env::temp_dir(), None, None);
    assert!(opts.backend.is_some(), "disk_at() must wire a backend");
    assert!(
        opts.descriptor_store.is_some(),
        "disk_at() must wire a descriptor store"
    );
    let _store = super::disk_descriptor_store();
}

/// Descriptor stores are isolated by their sessions root.
#[test]
fn test_store_isolation() {
    use houyicoder_context::{NameSource, SessionDescriptor, SessionProvenance};
    let root = std::env::temp_dir().join(format!("houyi-dms-{}-{}", std::process::id(), line!()));
    let _r = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("mkdir dms root");
    let store = super::disk_descriptor_store_at(root.clone());
    let sid = SessionId::new();
    let descriptor = SessionDescriptor {
        name: Some("named".into()),
        name_source: NameSource::Auto,
        cwd: "/repo".into(),
        model: "test-model".into(),
        provenance: SessionProvenance::Fresh,
        version: "t".into(),
        created_at: 1,
        child_session_ids: Vec::new(),
    };
    store
        .write_descriptor(sid, &descriptor)
        .expect("write_descriptor");
    let back = store
        .read_descriptor(sid)
        .expect("read_descriptor roundtrips at same root");
    assert_eq!(back.name.as_deref(), Some("named"));
    let other = std::env::temp_dir().join(format!(
        "houyi-dms-other-{}-{}",
        std::process::id(),
        line!()
    ));
    let other_store = super::disk_descriptor_store_at(other.clone());
    assert!(
        other_store.read_descriptor(sid).is_none(),
        "a store at a different root must not see the descriptor"
    );
    std::fs::remove_dir_all(&root).ok();
    std::fs::remove_dir_all(&other).ok();
}

/// Cloning a resolved provider shares the one provider instance rather than
/// standing up a second. Session swap depends on this: a clone per session
/// that rebuilt the provider would re-run the key helper and throw away the
/// warm connection pool.
#[test]
fn test_clone_shares_provider() {
    let resolved = ResolvedProvider::stub();
    let copy = resolved.clone();
    assert!(
        Arc::ptr_eq(&resolved.provider, &copy.provider),
        "a clone must point at the same provider, not a rebuilt one"
    );
}

/// The warnings ride along on the clone. They name where a repo is sending
/// traffic, so a swapped session that kept the provider but dropped them would
/// stop telling the user after the first session.
#[test]
fn test_clone_keeps_warnings() {
    let mut resolved = ResolvedProvider::stub();
    resolved.warnings.push(houyicoder_config::ConfigWarning {
        field: "provider.base_url".into(),
        reason: "this repo redirects model traffic".into(),
    });
    let copy = resolved.clone();
    assert_eq!(
        copy.warnings.len(),
        1,
        "the notice must survive the clone the swap path uses"
    );
}
