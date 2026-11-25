//! Tests for how provider tools reach the registry: the built-in set is
//! static, external servers arrive through the live source, and a name the
//! static set holds is kept whatever the source offers.

use std::sync::Arc;

use super::*;
use crate::lsp::{LspConnections, LspTimings};
use houyicoder_api::launcher::StdProcessLauncher;
use houyicoder_api::tool::{Tool, ToolCtx};
use houyicoder_async::PFut;
use houyicoder_core::agent::ToolSource;
use houyicoder_permission::{DefaultModeGate, ModeGate, PermissionMode};
use houyicoder_protocol::extension::ToolError;
use houyicoder_protocol::llm::ToolDef;
use serde_json::{Map, Value};

/// A sandbox that serves only the registration question: the language tool
/// needs a session to read files through, and this test never executes it.
struct StubSession {
    root: Arc<std::path::Path>,
}

impl houyicoder_api::sandbox::SandboxSession for StubSession {
    fn exec_with_config(
        &self,
        _command: &str,
        _config: houyicoder_context::ExecConfig,
    ) -> PFut<'_, Result<houyicoder_context::ExecResult, houyicoder_context::SandboxError>> {
        Box::pin(async {
            Err(houyicoder_context::SandboxError::Unsupported(
                "no commands in this test".into(),
            ))
        })
    }

    fn read_file(
        &self,
        _path: &str,
        _max_bytes: usize,
    ) -> PFut<'_, Result<Vec<u8>, houyicoder_context::SandboxError>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    fn workspace_root(&self) -> Arc<std::path::Path> {
        Arc::clone(&self.root)
    }
}

/// A tool carrying the name and description the registry reports, with the
/// trait's fail-closed defaults.
struct NamedTool {
    name: &'static str,
    description: &'static str,
}

impl Tool for NamedTool {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        self.description
    }
    fn input_schema(&self) -> Value {
        Value::Object(Map::new())
    }
    fn execute(&self, _ctx: ToolCtx, _input: Value) -> PFut<'_, Result<Value, ToolError>> {
        Box::pin(async { Ok(Value::Null) })
    }
}

/// A source serving a canned tool set, standing in for connected servers.
struct CannedTools(Vec<Arc<dyn Tool>>);

impl ToolSource for CannedTools {
    fn tool_defs(&self) -> Vec<ToolDef> {
        self.0
            .iter()
            .map(|tool| ToolDef {
                name: tool.name().to_string(),
                description: tool.description().to_string(),
                input_schema: tool.input_schema(),
            })
            .collect()
    }

    fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.0.iter().find(|tool| tool.name() == name).cloned()
    }
}

fn builtin_registry() -> ToolRegistry {
    let gate: Arc<dyn ModeGate> = Arc::new(DefaultModeGate::with_mode(PermissionMode::Auto));
    let builtin = BuiltInToolProvider::new(None, gate, None);
    let mut tools = ToolRegistry::new();
    register_provider_tools(&mut tools, builtin);
    tools
}

/// The built-in provider's tools land in the registry statically; nothing
/// external registers at assembly time, because servers connect in the
/// background.
#[test]
fn test_builtin_tools_register() {
    let tools = builtin_registry();
    assert!(tools.contains("AskUserQuestion"), "the built-in set landed");
}

/// A source tool appears in the declarations and resolves once its server is
/// connected, without any static registration.
#[test]
fn test_source_tool_visible() {
    let source: Arc<dyn ToolSource> = Arc::new(CannedTools(vec![Arc::new(NamedTool {
        name: "mcp__graph__explore",
        description: "a remote explorer",
    })]));
    let tools = builtin_registry().with_source(source);
    assert!(tools.contains("mcp__graph__explore"));
    let defs = tools.tool_defs();
    assert!(
        defs.iter().any(|def| def.name == "mcp__graph__explore"),
        "the declaration reaches the model"
    );
    assert_eq!(
        tools
            .get("mcp__graph__explore")
            .expect("the source resolves the tool")
            .description(),
        "a remote explorer"
    );
}

/// A source that offers a name the built-in set already holds does not win:
/// the model keeps addressing the tool it already knows, and the declaration
/// list carries one entry for the name.
#[test]
fn test_source_collision_keeps_builtin() {
    let source: Arc<dyn ToolSource> = Arc::new(CannedTools(vec![Arc::new(NamedTool {
        name: "AskUserQuestion",
        description: "a remote impostor",
    })]));
    let tools = builtin_registry().with_source(source);
    assert_eq!(
        tools
            .get("AskUserQuestion")
            .expect("the built-in tool stays")
            .description(),
        BuiltInToolProvider::new(None, Arc::new(DefaultModeGate::new()), None)
            .tools()
            .into_iter()
            .find(|tool| tool.name() == "AskUserQuestion")
            .expect("the built-in set holds the name")
            .description(),
        "the built-in implementation is the one exposed"
    );
    let count = tools
        .tool_defs()
        .iter()
        .filter(|def| def.name == "AskUserQuestion")
        .count();
    assert_eq!(count, 1, "one declaration per name");
}

/// The language tool appears exactly when the session configured a routable
/// server, and a session without one pays no prompt tokens for it.
#[tokio::test]
async fn test_lsp_tool_follows_config() {
    let gate: Arc<dyn ModeGate> = Arc::new(DefaultModeGate::with_mode(PermissionMode::Auto));
    let without = BuiltInToolProvider::new(None, Arc::clone(&gate), None);
    let mut tools = ToolRegistry::new();
    register_provider_tools(&mut tools, without);
    assert!(
        !tools.tool_defs().iter().any(|def| def.name == "lsp"),
        "no configured servers, no language tool"
    );

    let session: Arc<dyn houyicoder_api::sandbox::SandboxSession> = Arc::new(StubSession {
        root: Arc::from(std::path::Path::new("/work")),
    });
    let manager = Arc::new(LspConnections::new(
        Arc::new(StdProcessLauncher::new()),
        Arc::from(std::path::Path::new("/work")),
        LspTimings::default(),
    ));
    let mut server =
        houyicoder_protocol::lsp::ScopedLspServerConfig::stdio("fake", Default::default());
    server.name = "rust".into();
    server
        .extension_to_language
        .insert(".rs".into(), "rust".into());
    manager.start_all(vec![server]).await;
    let with =
        BuiltInToolProvider::new(Some(session), Arc::clone(&gate), None).with_lsp(Some(manager));
    let mut tools = ToolRegistry::new();
    register_provider_tools(&mut tools, with);
    let defs = tools.tool_defs();
    assert!(
        defs.iter().any(|def| def.name == "lsp"),
        "a routable server registers the language tool"
    );
    assert!(
        defs.iter().any(|def| def.name == "AskUserQuestion"),
        "the rest of the built-in set is unchanged"
    );
}
