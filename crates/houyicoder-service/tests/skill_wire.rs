//! Skill integration tests: drive the real Runner + the real skill
//! registry (real SKILL.md files on disk) + a stub provider through the
//! two invocation paths (slash + model Skill tool). These are the
//! end-to-end verifications the inline unit tests (stub registry) cannot
//! reach — they prove discover then resolve then prepare_body then inject
//! with real files, and the dual-path convergence at the shared ungated
//! prepare_body.
//!
//! Runs in make test integration / make check-full (pre-push), NOT in
//! make check --lib (the commit gate stays unit-only + fast). No live
//! binary, no network — a stub provider + temp SKILL.md files. Not ignored.

#![allow(clippy::unwrap_in_result)]

use std::path::Path;
use std::sync::Arc;

use houyicoder_api::skill::SkillRegistry;
use houyicoder_context::{SessionId, TurnEventKind};
use houyicoder_core::agent::runner_config::RunnerConfig;
use houyicoder_core::agent::{Runner, SkillTool, ToolRegistry};
use houyicoder_memory::InMemoryBackend;
use houyicoder_protocol::llm::OutputItem;
use houyicoder_provider::FakeProvider;
use houyicoder_service::composition::SkillRegistryImpl;
use houyicoder_session::SessionStore;

/// Write a SKILL.md fixture the discoverer picks up, with the given body.
fn write_skill(dir: &Path, name: &str, body: &str) {
    let skill_dir = dir.join(".houyicoder").join("skills").join(name);
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {name} skill\n---\n{body}\n"),
    )
    .unwrap();
}

/// The real end-to-end slash path. A real SKILL.md on disk is discovered,
/// then Runner.run("/commit fix typo") resolves the slash, keeps the raw
/// /-text as UserInput, and appends the real body (file content + the
/// base-dir header) as a durable SkillBody. Proves discover then resolve
/// then prepare_body then inject with real files, not stubs.
#[tokio::test]
async fn test_run_slash_real_body() {
    let tmp = std::env::temp_dir().join(format!("skill-wire-slash-{}", std::process::id()));
    write_skill(&tmp, "commit", "run git status\nstage changes\n");
    let reg: Arc<dyn SkillRegistry> =
        Arc::new(SkillRegistryImpl::discover_with_home(Some(&tmp), None));
    let store: Arc<dyn houyicoder_api::session::SessionLog> =
        Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let runner = Runner::with_shared_store(
        store.clone(),
        Arc::new(FakeProvider::text("done")),
        ToolRegistry::new(),
        RunnerConfig {
            model: "test".into(),
            instructions: String::new(),
            max_turns: 5,
            max_output_tokens: 8_000,
            ..RunnerConfig::default()
        },
    )
    .with_skill_registry(Arc::clone(&reg));
    let session = SessionId::new();
    runner
        .run(session, "/commit fix typo".into())
        .await
        .expect("run completes");
    let view = store.current_view(session).await.unwrap();
    // The raw /-text is the UserInput (transcript fidelity).
    let user = view.events.iter().find_map(|e| match &e.kind {
        TurnEventKind::UserInput { text } => Some(text.clone()),
        _ => None,
    });
    assert_eq!(user.as_deref(), Some("/commit fix typo"), "raw /-text kept");
    // The real body read from disk lands as a durable SkillBody (not a
    // MetaUser, so it survives a compaction boundary) with the base-dir
    // header, not a stub string.
    let body = view.events.iter().find_map(|e| match &e.kind {
        TurnEventKind::SkillBody {
            skill_name,
            content,
            untrusted,
            ..
        } => Some((skill_name.clone(), content.clone(), *untrusted)),
        _ => None,
    });
    let (name, content, untrusted) = body.expect("a SkillBody with the real body was appended");
    assert_eq!(name, "commit", "skill_name carried: {name}");
    assert!(
        content.contains("run git status"),
        "real body content from disk: {content}"
    );
    assert!(
        content.contains("Base directory for this skill"),
        "base-dir header prepended: {content}"
    );
    // A project-local skill (discovered under the cwd, not a managed or
    // user source) is untrusted, so the projection frames its body as
    // data — the fail-closed trust determination the slash path carries.
    assert!(untrusted, "project-local skill body is untrusted");
    drop(std::fs::remove_dir_all(&tmp));
}

/// The model-invoke path. A provider emits a Skill tool call, the agent
/// loop dispatches the SkillTool, and the real body read from disk lands
/// as a ToolResult. Proves the SkillTool is reachable by the model and
/// the find-then-gate-then-prepare_body chain runs end-to-end. Together
/// with test_run_slash_real_body this is the dual-path convergence: both
/// invocation paths reach the shared ungated prepare_body and produce the
/// same real body.
#[tokio::test]
async fn test_run_model_skill_tool() {
    let tmp = std::env::temp_dir().join(format!("skill-wire-model-{}", std::process::id()));
    write_skill(&tmp, "commit", "run git status\nstage changes\n");
    let reg: Arc<dyn SkillRegistry> =
        Arc::new(SkillRegistryImpl::discover_with_home(Some(&tmp), None));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(SkillTool::new(Arc::clone(&reg))));
    // First call: the model asks for the commit skill. Second: done.
    let provider = FakeProvider::from_outputs(vec![
        vec![OutputItem::ToolCall {
            id: "c1".into(),
            name: "skill".into(),
            input: serde_json::json!({"skill":"commit","args":"fix"}),
        }],
        vec![OutputItem::Text {
            text: "done".into(),
        }],
    ]);
    let store: Arc<dyn houyicoder_api::session::SessionLog> =
        Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let runner = Runner::with_shared_store(
        store.clone(),
        Arc::new(provider),
        tools,
        RunnerConfig {
            model: "test".into(),
            instructions: String::new(),
            max_turns: 5,
            max_output_tokens: 8_000,
            ..RunnerConfig::default()
        },
    )
    .with_skill_registry(reg);
    let session = SessionId::new();
    runner
        .run(session, "use the commit skill".into())
        .await
        .expect("run completes");
    let view = store.current_view(session).await.unwrap();
    // The SkillTool ran; its ToolResult carries the real body read from disk
    // (not a stub), with the base-dir header.
    let tool_result = view.events.iter().find_map(|e| match &e.kind {
        TurnEventKind::ToolResult { output, .. } => Some(output.clone()),
        _ => None,
    });
    let tr = tool_result.expect("a ToolResult from the Skill tool");
    assert!(
        tr.to_string().contains("run git status"),
        "real body in ToolResult: {tr}"
    );
    assert!(
        tr.to_string().contains("Base directory for this skill"),
        "base-dir header in ToolResult: {tr}"
    );
    // A project-local skill (discovered under the cwd) is untrusted, so the
    // Skill tool frames its body as data — the same framing the slash path
    // applies, so the two invocation paths do not differ.
    assert!(
        tr.to_string().contains("untrusted_skill"),
        "untrusted project skill body framed in the ToolResult: {tr}"
    );
    assert!(
        tr.to_string().contains("unverified data"),
        "framing note in the ToolResult: {tr}"
    );
    drop(std::fs::remove_dir_all(&tmp));
}

/// A skill body past the large-output isolate threshold lands as a
/// block_ref marker (preview + hint) in the ToolResult, not the raw bytes
/// — the agent loop externalizes the largest string field to the CAS so
/// the served view stays small and the raw stays retrievable. Proves the
/// isolation applies to skill bodies (not just bash/grep output).
#[tokio::test]
async fn test_run_large_body_compacts() {
    let tmp = std::env::temp_dir().join(format!("skill-wire-large-{}", std::process::id()));
    // Body past ISOLATE_LARGE_OUTPUT_BYTES (8192) once the base-dir header
    // + JSON envelope are added.
    let large_body = "x".repeat(9_000);
    write_skill(&tmp, "big", &large_body);
    let reg: Arc<dyn SkillRegistry> =
        Arc::new(SkillRegistryImpl::discover_with_home(Some(&tmp), None));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(SkillTool::new(Arc::clone(&reg))));
    let provider = FakeProvider::from_outputs(vec![
        vec![OutputItem::ToolCall {
            id: "c1".into(),
            name: "skill".into(),
            input: serde_json::json!({"skill":"big"}),
        }],
        vec![OutputItem::Text {
            text: "done".into(),
        }],
    ]);
    let store: Arc<dyn houyicoder_api::session::SessionLog> =
        Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let runner = Runner::with_shared_store(
        store.clone(),
        Arc::new(provider),
        tools,
        RunnerConfig {
            model: "test".into(),
            instructions: String::new(),
            max_turns: 5,
            max_output_tokens: 8_000,
            ..RunnerConfig::default()
        },
    )
    .with_skill_registry(reg);
    let session = SessionId::new();
    runner
        .run(session, "use the big skill".into())
        .await
        .expect("run completes");
    let view = store.current_view(session).await.unwrap();
    let tr = view
        .events
        .iter()
        .find_map(|e| match &e.kind {
            TurnEventKind::ToolResult { output, .. } => Some(output.clone()),
            _ => None,
        })
        .expect("a ToolResult from the Skill tool");
    // The "result" field is a block_ref marker object, not the raw string.
    let result = tr.get("result").expect("result field present");
    assert!(
        result.is_object(),
        "large result externalized to a block_ref marker, not raw: {result}"
    );
    assert!(
        result.get("block_ref").is_some(),
        "block_ref hash in the marker: {result}"
    );
    assert!(
        result.get("preview").is_some(),
        "inline preview in the marker: {result}"
    );
    // The full raw body is NOT in the served ToolResult — it is in the
    // CAS; the marker carries only a short preview. A run longer than the
    // preview cap confirms the bulk stayed out.
    assert!(
        !tr.to_string().contains(&"x".repeat(1_000)),
        "raw large body (9000 chars) not in the ToolResult, only the preview: {tr}"
    );
    drop(std::fs::remove_dir_all(&tmp));
}

/// A compaction that folds the listing out of the served view triggers a
/// re-announce on the next turn: inject_skill_listing scans the
/// manifest-applied view, finds no surviving listing, and appends a new
/// one. The model is never skill-blind after a compact. Proves the
/// compact-re-announce wiring (inject at compact re-drive sites) end-to-end
/// with a real manifest, not just the dedup unit test.
#[tokio::test]
async fn test_compact_reinjects_listing() {
    use houyicoder_context::{CheckpointId, CheckpointManifest, Disposition, TurnGroup};

    let tmp = std::env::temp_dir().join(format!("skill-wire-compact-{}", std::process::id()));
    write_skill(&tmp, "commit", "run git status");
    let reg: Arc<dyn SkillRegistry> =
        Arc::new(SkillRegistryImpl::discover_with_home(Some(&tmp), None));
    let store: Arc<dyn houyicoder_api::session::SessionLog> =
        Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let runner = Runner::with_shared_store(
        store.clone(),
        Arc::new(FakeProvider::text("ok")),
        ToolRegistry::new(),
        RunnerConfig {
            model: "test".into(),
            instructions: String::new(),
            max_turns: 5,
            max_output_tokens: 8_000,
            ..RunnerConfig::default()
        },
    )
    .with_skill_registry(reg);
    let session = SessionId::new();

    // Turn 1: listing injected.
    runner
        .run(session, "first message".into())
        .await
        .expect("run completes");
    let view1 = store.current_view(session).await.unwrap();
    let listing_id = view1
        .events
        .iter()
        .find_map(|e| matches!(e.kind, TurnEventKind::SkillListing { .. }).then(|| e.id))
        .expect("listing injected on turn 1");
    let last_event = view1.events.last().unwrap().id;

    // Simulate a compact: write a manifest that Summarizes the listing's
    // turn group so apply_manifest folds it out of the served view.
    let manifest = CheckpointManifest {
        id: CheckpointId::new(),
        session,
        last_event,
        summary: Some("compacted".into()),
        plan: vec![TurnGroup {
            turn_id: listing_id,
            disposition: Disposition::Summarized,
            event_ids: vec![listing_id],
        }],
        ts: 0,
    };
    store
        .write_checkpoint(manifest)
        .await
        .expect("checkpoint written");

    // Turn 2: inject_skill_listing sees the listing folded -> re-injects.
    runner
        .run(session, "second message".into())
        .await
        .expect("run completes");
    let view2 = store.current_view(session).await.unwrap();
    let listing_count = view2
        .events
        .iter()
        .filter(|e| matches!(e.kind, TurnEventKind::SkillListing { .. }))
        .count();
    assert!(
        listing_count >= 2,
        "listing re-injected after compact folded the first: found {listing_count}"
    );
    drop(std::fs::remove_dir_all(&tmp));
}
