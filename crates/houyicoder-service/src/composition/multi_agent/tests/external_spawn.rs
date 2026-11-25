//! Tests for external ACP child execution through the spawn path.

use super::*;
use houyicoder_api::launcher::{
    LauncherChild, LauncherExit, ProcessLauncher, SpawnError, SpawnPolicy, SpawnRequest, StdioPipes,
};
use houyicoder_api::provider::ModelProvider;
use houyicoder_api::spawn::{SpawnArgs, SpawnHandle};
use houyicoder_api::tool::ToolCtx;
use houyicoder_core::agent::multi_agent::registry::{
    AgentBackend, AgentDefinition, ExternalAcpSpec,
};
use houyicoder_memory::InMemoryBackend;
use houyicoder_provider::FakeProvider;
use houyicoder_session::SessionStore;
use std::env::temp_dir;
use std::io::Cursor;
use std::io::Write;
use std::io::sink;
use std::sync::Arc;

/// A launcher whose child speaks a canned ACP handshake and one turn, so an
/// external spawn completes without a real process.
struct CannedAcpLauncher;

impl CannedAcpLauncher {
    /// Launch a child whose whole conversation is this script.
    fn with_script(script: &str) -> LauncherChild {
        let stdout = Box::new(Cursor::new(script.as_bytes().to_vec()));
        let stdin: Box<dyn Write + Send> = Box::new(sink());
        let pipes = StdioPipes {
            stdin: Some(stdin),
            stdout: Some(stdout),
            stderr: None,
        };
        let waiter: houyicoder_async::PFut<'static, Result<LauncherExit, SpawnError>> =
            Box::pin(async {
                Ok(LauncherExit {
                    exit_code: Some(0),
                    stdout: None,
                    stderr: None,
                })
            });
        LauncherChild::with_pipes(Some(1), pipes, waiter)
    }

    fn full_script() -> &'static str {
        concat!(
            r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"01JTESTEXTCHILD01"}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"external answer"}}}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":3,"result":{"stopReason":"end_turn"}}"#,
            "\n",
        )
    }
}

impl ProcessLauncher for CannedAcpLauncher {
    fn spawn(&self, _req: SpawnRequest, _policy: SpawnPolicy) -> Result<LauncherChild, SpawnError> {
        Ok(Self::with_script(Self::full_script()))
    }
}

/// A definition whose backend is external ACP over a missing program.
fn external_definition(program: &str) -> AgentDefinition {
    AgentDefinition {
        subagent_type: "review".into(),
        when_to_use: "external reviewer".into(),
        tools: None,
        disallowed_tools: vec![],
        model: None,
        effort: None,
        permission_mode: None,
        max_turns: None,
        skills: vec![],
        mcp_servers: None,
        hooks: vec![],
        initial_prompt: None,
        memory: Default::default(),
        isolation: Default::default(),
        omit_project_context: false,
        color: None,
        system_prompt: houyicoder_core::agent::multi_agent::registry::PromptSource::Owned(
            "review".into(),
        ),
        backend: AgentBackend::ExternalAcp(ExternalAcpSpec {
            program: program.into(),
            args: vec![],
            passthrough_env: vec![],
            env: Vec::new(),
            handshake_timeout_ms: 5000,
        }),
    }
}

fn runtime_over(registry: Arc<dyn AgentRegistry>) -> MultiAgentRuntime {
    runtime_over_with_launcher(registry, Arc::new(CannedAcpLauncher))
}

fn runtime_over_with_launcher(
    registry: Arc<dyn AgentRegistry>,
    launcher: Arc<dyn ProcessLauncher>,
) -> MultiAgentRuntime {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::text("unused"));
    MultiAgentRuntime::new(MultiAgentDeps {
        registry,
        store,
        provider,
        tools: ToolRegistry::new(),
        config: RunnerConfig::default(),
        worktree_controller: None,
        workspace: Some(temp_dir()),
        bus: None,
        descriptor_store: None,
        skill_registry: None,
        conditional: None,
        hooks: None,
        launcher,
        mode_gate: None,
        mcp: None,
    })
}

/// A spawn of an external definition runs the child to its terminal and
/// returns a foreground outcome with no usage.
#[tokio::test]
async fn test_external_spawn_completes() {
    let registry: Arc<dyn AgentRegistry> =
        Arc::new(BuiltInRegistry::from_agents(vec![external_definition(
            "canned",
        )]));
    let runtime = runtime_over(registry);
    let ctx = ToolCtx::new("c1").with_session(SessionId::new());
    let args = SpawnArgs::new("review", "review the diff", "review");
    let outcome = runtime.spawn(&ctx, args).await.expect("spawn");
    assert_eq!(outcome.status.as_deref(), Some("completed"));
    assert!(
        outcome
            .summary
            .as_deref()
            .is_some_and(|s| s.contains("end_turn")),
        "{:?}",
        outcome.summary
    );
    assert!(
        outcome.usage.is_none(),
        "an external child reports no usage"
    );
}

/// A spawn whose program the launcher cannot start reports the failure
/// naming the program.
#[tokio::test]
async fn test_external_spawn_missing_program() {
    struct FailingLauncher;
    impl ProcessLauncher for FailingLauncher {
        fn spawn(
            &self,
            _req: SpawnRequest,
            _policy: SpawnPolicy,
        ) -> Result<LauncherChild, SpawnError> {
            Err(SpawnError::Io("no such file".into()))
        }
    }
    let registry: Arc<dyn AgentRegistry> =
        Arc::new(BuiltInRegistry::from_agents(vec![external_definition(
            "ghost-prog",
        )]));
    let runtime = runtime_over_with_launcher(registry, Arc::new(FailingLauncher));
    let ctx = ToolCtx::new("c1").with_session(SessionId::new());
    let args = SpawnArgs::new("review", "review", "review");
    let err = runtime.spawn(&ctx, args).await.expect_err("must fail");
    match err {
        SpawnFailure::ExternalFailed { reason } => {
            assert!(reason.contains("ghost-prog"), "{reason}");
        }
        other => panic!("expected ExternalFailed, got {other:?}"),
    }
}

/// A child that closes its pipe before answering the handshake fails the
/// spawn instead of hanging.
#[tokio::test]
async fn test_external_spawn_handshake_fails() {
    struct SilentLauncher;
    impl ProcessLauncher for SilentLauncher {
        fn spawn(
            &self,
            _req: SpawnRequest,
            _policy: SpawnPolicy,
        ) -> Result<LauncherChild, SpawnError> {
            Ok(CannedAcpLauncher::with_script(""))
        }
    }
    let runtime = runtime_over_with_launcher(
        Arc::new(BuiltInRegistry::from_agents(vec![external_definition(
            "silent",
        )])),
        Arc::new(SilentLauncher),
    );
    let ctx = ToolCtx::new("c1").with_session(SessionId::new());
    let args = SpawnArgs::new("review", "review", "review");
    let err = runtime.spawn(&ctx, args).await.expect_err("must fail");
    assert!(
        matches!(err, SpawnFailure::ExternalFailed { .. }),
        "{err:?}"
    );
}

/// A child that answers the handshake then closes before its turn ends
/// reports a failed terminal, not a completed one.
#[tokio::test]
async fn test_external_spawn_turn_fails() {
    struct HalfLauncher;
    impl ProcessLauncher for HalfLauncher {
        fn spawn(
            &self,
            _req: SpawnRequest,
            _policy: SpawnPolicy,
        ) -> Result<LauncherChild, SpawnError> {
            Ok(CannedAcpLauncher::with_script(concat!(
                r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1}}"#,
                "\n",
                r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"01JTESTEXTCHILD02"}}"#,
                "\n",
            )))
        }
    }
    let runtime = runtime_over_with_launcher(
        Arc::new(BuiltInRegistry::from_agents(vec![external_definition(
            "half",
        )])),
        Arc::new(HalfLauncher),
    );
    let ctx = ToolCtx::new("c1").with_session(SessionId::new());
    let args = SpawnArgs::new("review", "review", "review");
    let outcome = runtime.spawn(&ctx, args).await.expect("spawn");
    assert_eq!(outcome.status.as_deref(), Some("failed"));
    assert!(
        outcome
            .summary
            .as_deref()
            .is_some_and(|s| s.contains("closed")),
        "{:?}",
        outcome.summary
    );
}

/// A background request for an external child is refused rather than run
/// inline: a process child has no lifecycle that outlives the call, so the
/// model must learn the limit instead of silently blocking its own turn.
#[tokio::test]
async fn test_background_spawn_refused() {
    let runtime = runtime_over(Arc::new(BuiltInRegistry::from_agents(vec![
        external_definition("canned"),
    ])));
    let ctx = ToolCtx::new("c1").with_session(SessionId::new());
    let mut args = SpawnArgs::new("review", "review the diff", "review");
    args.run_in_background = true;
    let err = runtime.spawn(&ctx, args).await.expect_err("must refuse");
    assert!(matches!(err, SpawnFailure::CapabilityDenied), "{err:?}");
}
