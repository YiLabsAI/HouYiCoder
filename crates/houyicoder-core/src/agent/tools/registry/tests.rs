//! Tests for this module.

use super::*;
use pollster::block_on;

#[test]
fn test_tool_is_object_safe() {
    let _boxed: Arc<dyn Tool> = Arc::new(StubTool::new("echo"));
}

#[test]
fn test_registry_dispatches_by_name() {
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(StubTool::new("echo")));
    let defs = reg.tool_defs();
    assert_eq!(defs.len(), 1);
    assert_eq!(defs[0].name, "echo");
    let tool = reg.get("echo").expect("registered");
    let out = block_on(tool.execute(ToolCtx::new("test"), serde_json::json!({"x": 1}))).unwrap();
    assert_eq!(out, serde_json::json!({"echo": {"x": 1}}));
    assert!(reg.get("missing").is_none());
}

#[test]
fn test_narrow_drops_disallowed() {
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(StubTool::new("read")));
    reg.register(Arc::new(StubTool::new("write")));
    reg.register(Arc::new(StubTool::new("agent")));
    let child = reg.narrow(&["write".into(), "Agent".into()]);
    assert!(child.get("read").is_some());
    assert!(child.get("write").is_none(), "write must be dropped");
    assert!(
        child.get("agent").is_none(),
        "agent must be dropped (case-insensitive)"
    );
    assert_eq!(child.len(), 1);
}

#[test]
fn test_tool_defs_deterministic_sort() {
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(StubTool::new("zeta")));
    reg.register(Arc::new(StubTool::new("alpha")));
    reg.register(Arc::new(StubTool::new("mid")));
    let names: Vec<String> = reg.tool_defs().into_iter().map(|t| t.name).collect();
    assert_eq!(names, vec!["alpha", "mid", "zeta"]);
}

#[test]
fn test_fail_closed_defaults() {
    let t = StubTool::new("echo");
    // StubTool overrides all four; verify a bare trait object via a
    // minimal struct shows the fail-closed defaults instead.
    struct Dangerous;
    impl Tool for Dangerous {
        fn name(&self) -> &str {
            "danger"
        }
        fn description(&self) -> &str {
            ""
        }
        fn input_schema(&self) -> Value {
            serde_json::json!({})
        }
        fn execute(&self, _ctx: ToolCtx, _: Value) -> PFut<'_, Result<Value, ToolError>> {
            Box::pin(async { Ok(serde_json::json!({})) })
        }
    }
    let d = Dangerous;
    assert!(!d.is_concurrency_safe());
    assert!(!d.is_read_only());
    assert!(d.is_destructive());
    assert!(!d.requires_approval());
    // StubTool is the safe opposite.
    assert!(t.is_concurrency_safe());
    assert!(t.is_read_only());
    assert!(!t.is_destructive());
    assert!(!t.requires_approval());
}
