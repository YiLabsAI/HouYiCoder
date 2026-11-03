//! What a spawned child's descriptor records about that child.

use std::time::{Duration, Instant};

use tokio::time::sleep;

use super::*;

/// The child's descriptor carries the prompt the child ran on. The child
/// runner is built with the parent's descriptor store, so the first user
/// event of the child's own log reaches the descriptor a resume by id reads.
#[tokio::test]
async fn test_child_prompt_reaches_descriptor() {
    let descriptors = Arc::new(InMemoryDescriptorStore::new());
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let registry: Arc<dyn AgentRegistry> = Arc::new(BuiltInRegistry::from_agents(built_in_all()));
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::text("done"));
    let runtime = MultiAgentRuntime::new(MultiAgentDeps {
        registry,
        store: store.clone(),
        provider,
        tools: ToolRegistry::new(),
        config: RunnerConfig::default(),
        worktree_controller: None,
        workspace: Some(PathBuf::from("/tmp")),
        bus: None,
        descriptor_store: Some(descriptors.clone()),
        skill_registry: None,
        conditional: None,
        hooks: None,
    });
    let parent_sid = SessionId::new();
    let ctx = ToolCtx::new("c1").with_session(parent_sid);
    runtime
        .spawn(
            &ctx,
            SpawnArgs::new("explore", "find the auth module", "find auth"),
        )
        .await
        .expect("spawn");
    let child = store
        .trajectory_tail(parent_sid)
        .into_iter()
        .find_map(|entry| match entry.event {
            SessionEvent::SubagentSpawn {
                child_session_id, ..
            } => Some(child_session_id),
            _ => None,
        })
        .expect("the spawn boundary names the child");
    let child_sid = SessionId::from_display_string(&child).expect("child sid parses");
    // The child runner lives inside the future that starts the delegation and
    // is gone once it returns, so there is no handle here to wait on. Poll
    // instead, with a deadline so a write that never lands fails the test
    // instead of hanging.
    let deadline = Instant::now() + Duration::from_secs(5);
    let descriptor = loop {
        if let Some(descriptor) = descriptors.read_descriptor(child_sid)
            && descriptor.first_prompt.is_some()
        {
            break descriptor;
        }
        assert!(
            Instant::now() < deadline,
            "the child's prompt never reached the descriptor"
        );
        sleep(Duration::from_millis(5)).await;
    };
    assert_eq!(
        descriptor
            .first_prompt
            .expect("the child's prompt is recorded")
            .text,
        "find the auth module"
    );
}

/// A descriptor that already records this delegation keeps every other field.
/// The edit changes nothing, so the store reports that it wrote nothing and
/// the record stands as it is: writing a fresh one would reset the name, the
/// creation time, and both prompt slots the child has filled since.
#[test]
fn test_stamp_keeps_existing_record() {
    let descriptors = Arc::new(InMemoryDescriptorStore::new());
    let parent = SessionId::new();
    let child = SessionId::new();
    let provenance = SessionProvenance::SpawnedBy {
        parent_session_id: parent.to_string(),
        subagent_type: "explore".to_string(),
        task_id: "task-1".to_string(),
    };
    let mut existing =
        SessionDescriptor::delegated_child(provenance, 1_700_000_000, PromptCoverage::Complete);
    existing.name = Some("kept".to_string());
    descriptors
        .write_descriptor(child, &existing)
        .expect("seed the child's record");
    let store: Option<Arc<dyn SessionDescriptorStore>> = Some(descriptors.clone());
    record_provenance(&store, child, parent, "explore", "task-1");
    assert_eq!(
        descriptors.read_descriptor(child),
        Some(existing),
        "a record that already states the delegation is left alone"
    );
}
