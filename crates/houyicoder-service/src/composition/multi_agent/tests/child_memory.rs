//! Read-only memory inheritance for general-purpose children while keeping
//! explore and plan agents free of memory overhead and blocking child writes.

use std::env::temp_dir;
use std::fs::{create_dir_all, remove_dir_all};
use std::process::id;
use std::sync::{Arc, Mutex};

use houyicoder_api::launcher::StdProcessLauncher;
use houyicoder_api::memory::MemoryProvider;
use houyicoder_api::provider::stream_from_response;
use houyicoder_async::{PFut, PStream};
use houyicoder_context::{MemoryEntry, MemoryScope, MemoryType};
use houyicoder_core::agent::{MemoryGates, MemoryWriteTool};
use houyicoder_memory::MarkdownMemoryProvider;
use houyicoder_protocol::llm::{
    CompletionRequest, CompletionResponse, LlmEvent, ModelCapabilities, OutputItem, ProviderError,
    Usage,
};

use super::*;

struct CapturingChildProvider {
    requests: Mutex<Vec<CompletionRequest>>,
}

impl CapturingChildProvider {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            requests: Mutex::new(Vec::new()),
        })
    }
}

impl ModelProvider for CapturingChildProvider {
    fn complete(
        &self,
        req: CompletionRequest,
    ) -> PFut<'_, Result<CompletionResponse, ProviderError>> {
        self.requests.lock().expect("requests").push(req);
        let resp = CompletionResponse {
            output: vec![OutputItem::Text {
                text: "child finished".into(),
            }],
            usage: Usage::default(),
            model: "test".into(),
        };
        Box::pin(async move { Ok(resp) })
    }
    fn stream(&self, req: CompletionRequest) -> PStream<'_, Result<LlmEvent, ProviderError>> {
        self.requests.lock().expect("requests").push(req);
        stream_from_response(CompletionResponse {
            output: vec![OutputItem::Text {
                text: "child finished".into(),
            }],
            usage: Usage::default(),
            model: "test".into(),
        })
    }
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
}

fn build_test_runtime(
    dir: PathBuf,
    model: Arc<CapturingChildProvider>,
    parent_tools: ToolRegistry,
    mem: Arc<MarkdownMemoryProvider>,
    gates: MemoryGates,
) -> MultiAgentRuntime {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let registry: Arc<dyn AgentRegistry> = Arc::new(BuiltInRegistry::from_agents(built_in_all()));
    MultiAgentRuntime::new(MultiAgentDeps {
        registry,
        store,
        provider: model as Arc<dyn ModelProvider>,
        tools: parent_tools,
        config: RunnerConfig::default(),
        worktree_controller: None,
        workspace: Some(dir),
        bus: None,
        descriptor_store: None,
        skill_registry: None,
        conditional: None,
        hooks: None,
        launcher: Arc::new(StdProcessLauncher::new()),
        mode_gate: None,
    })
    .with_inheritance(RunnerInheritance {
        memory_provider: Some(mem as Arc<dyn MemoryProvider>),
        memory_reranker: None,
        memory_gates: Some(gates),
        ..RunnerInheritance::default()
    })
}

#[tokio::test]
async fn test_general_child_reads_memory() {
    let dir = temp_dir().join(format!("child-mem-{}", id()));
    let user = dir.join("user");
    let proj = dir.join("project");
    let auto = dir.join("auto");
    for d in [&user, &proj, &auto] {
        create_dir_all(d).expect("mkdir");
    }
    let mem = Arc::new(MarkdownMemoryProvider::new_multi(vec![user, proj, auto]));
    mem.add_in_scope(
        MemoryEntry::new(
            "no-shell-edit",
            "Always use file edit tools instead of shell text substitution.",
            MemoryType::Feedback,
        )
        .with_meta("Use structured file edit tools", 10),
        MemoryScope::User,
    )
    .expect("seed user rule");
    mem.add(
        MemoryEntry::new(
            "deploy-fact",
            "Staging deploys run from the release branch.",
            MemoryType::Project,
        )
        .with_meta("Staging deploys use release branch", 20),
    )
    .expect("seed auto fact");

    let mut parent_tools = ToolRegistry::new();
    parent_tools.register(Arc::new(MemoryWriteTool::new(
        Arc::clone(&mem) as Arc<dyn MemoryProvider>
    )));

    let model = CapturingChildProvider::new();
    let runtime = build_test_runtime(
        dir.clone(),
        Arc::clone(&model),
        parent_tools,
        Arc::clone(&mem),
        MemoryGates::new(true, true),
    );

    let ctx = ToolCtx::new("c1").with_session(SessionId::new());
    runtime
        .spawn(
            &ctx,
            SpawnArgs::new("general-purpose", "inspect deploy path", "inspect deploy"),
        )
        .await
        .expect("general-purpose spawn");

    let gp_req = model.requests.lock().expect("reqs")[0].clone();
    assert!(gp_req.instructions.contains("# Active Rules"));
    assert!(gp_req.instructions.contains("no-shell-edit"));
    assert!(gp_req.instructions.contains("## Memory index"));
    assert!(gp_req.instructions.contains("deploy-fact"));
    let gp_tools: Vec<&str> = gp_req.tools.iter().map(|t| t.name.as_str()).collect();
    assert!(gp_tools.contains(&"memory_read") && gp_tools.contains(&"memory_search"));
    assert!(!gp_tools.contains(&"memory_write"));

    runtime
        .spawn(&ctx, SpawnArgs::new("explore", "search files", "search"))
        .await
        .expect("explore spawn");
    let explore_req = model.requests.lock().expect("reqs")[1].clone();
    assert!(!explore_req.instructions.contains("## Memory index"));
    assert!(
        explore_req
            .tools
            .iter()
            .all(|t| !t.name.starts_with("memory_"))
    );

    let off_runtime = build_test_runtime(
        dir.clone(),
        Arc::clone(&model),
        ToolRegistry::new(),
        Arc::clone(&mem),
        MemoryGates::new(false, false),
    );
    off_runtime
        .spawn(
            &ctx,
            SpawnArgs::new("general-purpose", "gate off check", "gate off"),
        )
        .await
        .expect("gate off spawn");
    let off_req = model.requests.lock().expect("reqs")[2].clone();
    assert!(!off_req.instructions.contains("## Memory index"));
    assert!(off_req.tools.iter().all(|t| !t.name.starts_with("memory_")));

    drop(remove_dir_all(&dir));
}
