//! Tests for how provider tools reach the registry: the order the two sources
//! register in, which source keeps a name both claim, and whether an external
//! adapter's call reaches the permission gate once registered.

use super::*;
use houyicoder_api::sandbox::{Containment, Coverage, SideEffect};
use houyicoder_api::tool::{Tool, ToolCtx};
use houyicoder_async::PFut;
use houyicoder_permission::{DefaultModeGate, Effect, PermissionMode, Rule};
use houyicoder_protocol::extension::ToolError;
use serde_json::{Map, Value, json};
use std::env::temp_dir;
use std::fs::{create_dir_all, remove_dir_all};
use std::path::{Path, PathBuf};
use std::process::id;

/// A tool carrying the name and description the registry reports, with the
/// trait's fail-closed defaults. The description tells which source won a name
/// collision without reaching into the registry's storage.
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

/// A provider contributing a fixed tool set, standing in for a remote server
/// without starting one.
struct FixedProvider {
    name: &'static str,
    tools: Vec<Arc<dyn Tool>>,
}

impl ToolProvider for FixedProvider {
    fn name(&self) -> &str {
        self.name
    }
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        self.tools.clone()
    }
}

/// A fence over one workspace root, standing in for the session's containment
/// so the boundary stage has a root to judge an external target against.
struct RootFence(PathBuf);

impl Containment for RootFence {
    fn coverage(&self) -> Coverage {
        Coverage::Fenced {
            writable_roots: vec![self.0.clone()],
        }
    }
    fn would_block(&self, _effect: SideEffect) -> Option<String> {
        None
    }
    fn boundary_root(&self) -> Option<Arc<Path>> {
        Some(Arc::from(self.0.clone().into_boxed_path()))
    }
}

/// A registry gate over one external provider, starting from an empty
/// registry so each test states the source order it exercises.
fn register_external(gate: Arc<dyn ModeGate>, tools: Vec<Arc<dyn Tool>>) -> ToolRegistry {
    let builtin = FixedProvider {
        name: "builtin",
        tools: Vec::new(),
    };
    let external: Vec<Box<dyn ToolProvider>> = vec![Box::new(FixedProvider { name: "mcp", tools })];
    let mut registry = ToolRegistry::new();
    register_in_order(&mut registry, &gate, &builtin, &external);
    registry
}

/// A server that cannot start contributes no provider: the failure is reported
/// and the session keeps every tool the other sources already registered.
#[test]
fn test_missing_server_is_skipped() {
    let cfg = McpServerConfig {
        program: "/nonexistent/houyi-mcp-server".into(),
        args: Vec::new(),
    };
    let started = start_servers(&[cfg]);
    assert!(started.is_empty(), "a failed server adds no provider");
}

/// Built-in tools register before external ones, so a remote server cannot
/// take a name the registry already holds: the tool the model already
/// addresses keeps its implementation, and a server that calls its own shell
/// bash cannot replace the built-in one behind that word.
#[test]
fn test_collision_keeps_builtin() {
    let builtin = FixedProvider {
        name: "builtin",
        tools: vec![Arc::new(NamedTool {
            name: "bash",
            description: "the built-in shell",
        })],
    };
    let external: Vec<Box<dyn ToolProvider>> = vec![Box::new(FixedProvider {
        name: "mcp",
        tools: vec![Arc::new(NamedTool {
            name: "bash",
            description: "a remote shell",
        })],
    })];
    let gate: Arc<dyn ModeGate> = Arc::new(DefaultModeGate::with_mode(PermissionMode::Auto));
    let mut tools = ToolRegistry::new();
    register_in_order(&mut tools, &gate, &builtin, &external);
    assert_eq!(tools.len(), 1, "the colliding tool is not registered");
    let kept = tools.get("bash").expect("the built-in tool stays");
    assert_eq!(kept.description(), "the built-in shell");
}

/// An external write tool runs behind the permission gate once registered: a
/// target outside the workspace reaches the boundary stage, which names the
/// directory the approval would grant. A tool registered on its adapter
/// defaults alone would execute with no card at all.
#[test]
fn test_external_write_asks() {
    let base = temp_dir().join(format!("houyi-ext-{}", id()));
    let root = base.join("root");
    let outside = base.join("outside");
    create_dir_all(&root).expect("mkdir root");
    create_dir_all(&outside).expect("mkdir outside");
    let fence = RootFence(dunce::canonicalize(&root).expect("root resolves"));
    let gate: Arc<dyn ModeGate> = Arc::new(
        DefaultModeGate::with_mode(PermissionMode::Auto).with_containment(Arc::new(fence)),
    );
    let tools = register_external(
        gate,
        vec![Arc::new(NamedTool {
            name: "patch",
            description: "a remote patcher",
        })],
    );
    let tool = tools.get("patch").expect("the external tool is registered");
    let input = json!({"path": outside.join("file.txt").to_string_lossy()});
    let reason = tool
        .approval_reason(&input)
        .expect("an outside target must reach the human");
    assert_eq!(reason.validator, "path-bounds");
    let parent = dunce::canonicalize(&outside).expect("outside resolves");
    assert!(
        reason
            .detail
            .contains(&parent.to_string_lossy().to_string()),
        "the card names the directory the approval grants: {}",
        reason.detail
    );
    remove_dir_all(&base).ok();
}

/// A remote tool whose name the gate's built-in vocabulary does not know still
/// reaches the gate: a deny rule on that name stops the call before the
/// adapter runs. An adapter registered without the wrapper would execute with
/// no verdict at all.
#[tokio::test]
async fn test_deny_rule_stops_external() {
    let gate = Arc::new(DefaultModeGate::with_mode(PermissionMode::Auto));
    gate.add_rule(Rule::new("write_file", Effect::Deny).expect("deny rule"))
        .expect("the rule is accepted");
    let gate: Arc<dyn ModeGate> = gate;
    let tools = register_external(
        gate,
        vec![Arc::new(NamedTool {
            name: "write_file",
            description: "a remote writer",
        })],
    );
    let tool = tools
        .get("write_file")
        .expect("the external tool is registered");
    let input = json!({"path": "notes.txt"});
    assert!(
        tool.approval_reason(&input).is_none(),
        "a deny rule leaves no card to ask"
    );
    let err = tool
        .execute(ToolCtx::new("test"), input)
        .await
        .expect_err("the denied call must not reach the adapter");
    match err {
        ToolError::Failed(msg) => assert!(
            msg.contains("denied by mode"),
            "the refusal carries the gate's reason: {msg}"
        ),
        other => panic!("expected a gate refusal, got {other:?}"),
    }
}

/// An external tool whose name the gate's table does not know is judged by
/// the adapter's fail-closed defaults, so Auto asks before it runs; an allow
/// rule on the name is what removes the card, not the absence of a
/// classification.
#[tokio::test]
async fn test_external_tool_default_asks() {
    let gate = Arc::new(DefaultModeGate::with_mode(PermissionMode::Auto));
    let tools = register_external(
        Arc::clone(&gate) as Arc<dyn ModeGate>,
        vec![Arc::new(NamedTool {
            name: "write_note",
            description: "a remote writer",
        })],
    );
    let tool = tools
        .get("write_note")
        .expect("the external tool is registered");
    let input = json!({"text": "hi"});
    let reason = tool
        .approval_reason(&input)
        .expect("an unclassified remote tool must reach the human");
    assert_eq!(reason.validator, "mode_default");
    assert!(
        reason.detail.contains("destructive"),
        "the card names the destructive default: {}",
        reason.detail
    );
    let err = tool
        .execute(ToolCtx::new("test"), input.clone())
        .await
        .expect_err("an unanswered ask must not reach the adapter");
    assert!(
        err.to_string().contains("requires approval"),
        "the refusal names the pending approval: {err}"
    );
    gate.add_rule(Rule::new("write_note", Effect::Allow).expect("allow rule"))
        .expect("the rule is accepted");
    assert!(
        tool.approval_reason(&input).is_none(),
        "an allow rule on the name removes the card"
    );
    tool.execute(ToolCtx::new("test"), input)
        .await
        .expect("the allowed call reaches the adapter");
}
