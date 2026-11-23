//! The product binary driven as an ACP child: the runtime launches the real
//! houyi in ACP mode and drives one prompt through the southbound client, so
//! the entry point a user would run is proven reachable from the delegation
//! path rather than only from a fixture.
//!
//! The binary runs harness-free so the environment a child inherits is set
//! once, before any thread reads it.

use std::env::temp_dir;
use std::fs::create_dir_all;
use std::fs::remove_dir_all;
use std::panic::catch_unwind;
use std::path::PathBuf;
use std::process::exit;
use std::process::id;
use std::sync::Arc;

use tokio::runtime::Builder as RuntimeBuilder;

use houyicoder_api::launcher::StdProcessLauncher;
use houyicoder_api::provider::ModelProvider;
use houyicoder_api::spawn::{SpawnArgs, SpawnHandle};
use houyicoder_api::tool::ToolCtx;
use houyicoder_context::SessionId;
use houyicoder_core::agent::ToolRegistry;
use houyicoder_core::agent::multi_agent::registry::{
    AgentBackend, AgentDefinition, AgentRegistry, BuiltInRegistry, ExternalAcpSpec, PromptSource,
};
use houyicoder_core::agent::runner_config::RunnerConfig;
use houyicoder_memory::InMemoryBackend;
use houyicoder_provider::FakeProvider;
use houyicoder_service::composition::multi_agent::{MultiAgentDeps, MultiAgentRuntime};
use houyicoder_session::SessionStore;

/// A scripted model: one text reply, so the child ends its turn at once.
const SCRIPT: &str = r#"[[{"type":"Text","text":"from the real binary"}]]"#;

fn scratch(name: &str) -> PathBuf {
    let dir = temp_dir().join(format!("houyi-acp-child-{name}-{}", id()));
    create_dir_all(&dir).expect("scratch dir");
    dir
}

async fn test_houyi_acp_child_round_trip() {
    let home = scratch("home");
    let workspace = scratch("workspace");
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let definition = AgentDefinition {
        subagent_type: "review".into(),
        when_to_use: "external reviewer".into(),
        tools: None,
        disallowed_tools: vec![],
        model: None,
        effort: None,
        permission_mode: None,
        max_turns: None,
        skills: vec![],
        mcp_servers: vec![],
        hooks: vec![],
        initial_prompt: None,
        memory: Default::default(),
        isolation: Default::default(),
        omit_project_context: false,
        color: None,
        system_prompt: PromptSource::Owned("review".into()),
        backend: AgentBackend::ExternalAcp(ExternalAcpSpec {
            program: env!("CARGO_BIN_EXE_houyi").to_string(),
            args: vec!["--acp".to_string()],
            passthrough_env: Vec::new(),
            // The definition sets what the child needs; the host environment
            // is never read for these, so the test process stays untouched.
            env: vec![
                (
                    "HOUYICODER_SESSIONS_DIR".into(),
                    home.join("sessions").display().to_string(),
                ),
                (
                    "HOUYICODER_TEST_STUB_RESPONSE_SCRIPT".into(),
                    SCRIPT.to_string(),
                ),
                ("HOME".into(), home.display().to_string()),
                ("USERPROFILE".into(), home.display().to_string()),
            ],
            handshake_timeout_ms: 20_000,
        }),
    };
    let registry: Arc<dyn AgentRegistry> = Arc::new(BuiltInRegistry::from_agents(vec![definition]));
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::text("unused"));
    let runtime = MultiAgentRuntime::new(MultiAgentDeps {
        registry,
        store,
        provider,
        tools: ToolRegistry::new(),
        config: RunnerConfig::default(),
        worktree_controller: None,
        workspace: Some(workspace.clone()),
        bus: None,
        descriptor_store: None,
        skill_registry: None,
        conditional: None,
        hooks: None,
        launcher: Arc::new(StdProcessLauncher::new()),
        mode_gate: None,
    });
    let ctx = ToolCtx::new("c1").with_session(SessionId::new());
    let args = SpawnArgs::new("review", "review the diff", "review");
    let outcome = runtime.spawn(&ctx, args).await.expect("the child runs");
    assert_eq!(outcome.status.as_deref(), Some("completed"));
    assert!(
        outcome
            .summary
            .as_deref()
            .is_some_and(|s| s.contains("end_turn")),
        "{:?}",
        outcome.summary
    );
    drop(remove_dir_all(&home));
    drop(remove_dir_all(&workspace));
}

fn main() {
    // A harness-free binary still answers the two questions a runner asks:
    // which tests exist, and whether this invocation is the one that runs.
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|arg| arg == "--list") {
        println!("test_houyi_acp_child_round_trip: test");
        return;
    }
    let named = args
        .iter()
        .position(|arg| arg == "--exact")
        .and_then(|at| args.get(at + 1))
        .is_none_or(|name| name == "test_houyi_acp_child_round_trip");
    if !named {
        return;
    }
    let runtime = RuntimeBuilder::new_current_thread()
        .enable_all()
        .build()
        .expect("a tokio runtime");
    let failed = catch_unwind(|| {
        runtime.block_on(test_houyi_acp_child_round_trip());
    })
    .is_err();
    if failed {
        println!("test test_houyi_acp_child_round_trip ... FAILED");
        exit(1);
    }
    println!("test test_houyi_acp_child_round_trip ... ok");
    println!("1 passed, 0 failed");
}
