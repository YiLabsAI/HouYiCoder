use super::*;
use crate::gate::DefaultModeGate;
use crate::mode::PermissionMode;
use crate::rule::{Effect, Rule};
use houyicoder_protocol::extension::ToolError;
use pollster::block_on;
use serde_json::Map;
use std::sync::Mutex;

/// A destructive tool that records whether execute ran. Tracks the shape
/// of the real bash/edit tools (destructive + requires approval) so the
/// gate decisions exercise the real code path.
struct DangerTool {
    ran: Mutex<bool>,
}
impl Tool for DangerTool {
    fn name(&self) -> &str {
        "danger"
    }
    fn description(&self) -> &str {
        ""
    }
    fn input_schema(&self) -> Value {
        Value::Object(Map::new())
    }
    fn execute(&self, _ctx: ToolCtx, _input: Value) -> PFut<'_, Result<Value, ToolError>> {
        *self.ran.lock().unwrap() = true;
        Box::pin(async { Ok(Value::Null) })
    }
    fn is_destructive(&self) -> bool {
        true
    }
    fn requires_approval(&self) -> bool {
        true
    }
}

fn guarded(mode: PermissionMode) -> (Arc<DangerTool>, Arc<dyn Tool>) {
    let inner = Arc::new(DangerTool {
        ran: Mutex::new(false),
    });
    let gate: Arc<dyn ModeGate> = Arc::new(DefaultModeGate::with_mode(mode));
    let wrapped: Arc<dyn Tool> = Arc::new(GuardedTool::new(inner.clone(), gate));
    (inner, wrapped)
}

#[test]
fn test_manual_asks_before_destructive() {
    let (_inner, tool) = guarded(PermissionMode::Manual);
    assert!(tool.requires_approval());
}

#[test]
fn test_auto_asks_destructive_tool() {
    // Auto still asks for a tool that declares it needs approval (the
    // recoverable invariant, not a blanket skip, governs destructive ops).
    let (_inner, tool) = guarded(PermissionMode::Auto);
    assert!(tool.requires_approval());
}

#[test]
fn test_whitelist_rule_auto_allows() {
    let inner = Arc::new(DangerTool {
        ran: Mutex::new(false),
    });
    let gate = Arc::new(DefaultModeGate::with_mode(PermissionMode::Auto));
    gate.add_rule(Rule::new("danger", Effect::Allow).unwrap());
    let tool: Arc<dyn Tool> = Arc::new(GuardedTool::new(inner.clone(), gate));
    assert!(!tool.requires_approval());
    block_on(tool.execute(ToolCtx::new("test"), Value::Null)).unwrap();
    assert!(*inner.ran.lock().unwrap());
}

#[test]
fn test_deny_rule_blocks_run() {
    let inner = Arc::new(DangerTool {
        ran: Mutex::new(false),
    });
    let gate = Arc::new(DefaultModeGate::with_mode(PermissionMode::Auto));
    gate.add_rule(Rule::new("danger", Effect::Deny).unwrap());
    let tool: Arc<dyn Tool> = Arc::new(GuardedTool::new(inner.clone(), gate));
    let err = block_on(tool.execute(ToolCtx::new("test"), Value::Null)).unwrap_err();
    assert!(err.to_string().contains("denied"));
    assert!(!*inner.ran.lock().unwrap());
}

#[test]
fn test_wrapped_preserves_metadata() {
    let (_inner, tool) = guarded(PermissionMode::Manual);
    assert_eq!(tool.name(), "danger");
    assert!(tool.is_destructive());
}

/// The protected-path pre-check is write-only, so the two capability
/// claims split: a read-only tool whose input touches a protected path (a
/// glob of .git/) passes, while a write tool over the same path escalates
/// to Ask. The input-aware pre-check still reaches the marker that the
/// input-blind check misses (safety_check sees no content with
/// input=None), so the write side stays guarded end to end.
#[test]
fn test_precheck_split_by_write() {
    struct GlobTool;
    impl Tool for GlobTool {
        fn name(&self) -> &str {
            "glob"
        }
        fn description(&self) -> &str {
            ""
        }
        fn input_schema(&self) -> Value {
            Value::Object(Map::new())
        }
        fn execute(&self, _ctx: ToolCtx, _input: Value) -> PFut<'_, Result<Value, ToolError>> {
            Box::pin(async { Ok(Value::Null) })
        }
        fn is_read_only(&self) -> bool {
            true
        }
        fn is_destructive(&self) -> bool {
            false
        }
    }
    let gate = Arc::new(DefaultModeGate::with_mode(PermissionMode::Auto));
    let tool: Arc<dyn Tool> = Arc::new(GuardedTool::new(Arc::new(GlobTool), gate));
    // Input-blind pre-check: no content -> safety misses -> Auto allows
    // a read-only tool -> false (would inline-execute).
    assert!(!tool.requires_approval());
    // Input-aware pre-check over the same protected pattern: a read-only
    // call must pass (the stage guards writes only).
    assert!(
        !tool.requires_approval_for(&serde_json::json!({"pattern": ".git/config"})),
        "a read-only glob of a protected path must not escalate"
    );
    // The write claim over a protected path still escalates.
    struct WriteTool;
    impl Tool for WriteTool {
        fn name(&self) -> &str {
            "write"
        }
        fn description(&self) -> &str {
            ""
        }
        fn input_schema(&self) -> Value {
            Value::Object(Map::new())
        }
        fn execute(&self, _ctx: ToolCtx, _input: Value) -> PFut<'_, Result<Value, ToolError>> {
            Box::pin(async { Ok(Value::Null) })
        }
        fn is_read_only(&self) -> bool {
            false
        }
        fn is_destructive(&self) -> bool {
            true
        }
    }
    let write_tool: Arc<dyn Tool> = Arc::new(GuardedTool::new(
        Arc::new(WriteTool),
        Arc::new(DefaultModeGate::with_mode(PermissionMode::Auto)),
    ));
    assert!(
        write_tool.requires_approval_for(&serde_json::json!({"path": ".git/config"})),
        "a write to a protected path must escalate at the pre-check"
    );
    // Exercise the trait surface the gate reads so no stub method is left
    // uncovered, and name/description stay correct on the wrapper.
    assert_eq!(tool.name(), "glob");
    assert_eq!(tool.description(), "");
    assert_eq!(write_tool.name(), "write");
    assert_eq!(write_tool.description(), "");
    let _schema = tool.input_schema();
    let _wschema = write_tool.input_schema();
    let _exec = block_on(tool.execute(ToolCtx::new("t"), Value::Null));
    let _wexec = block_on(write_tool.execute(ToolCtx::new("w"), Value::Null));
}

/// A tool whose own per-input approval gate returns true for a specific
/// input (matches the worktree-exit shape: non-destructive, non-read-only,
/// input-blind approval false, but action=="remove" asks) must surface
/// that Ask through GuardedTool even in Auto mode. The gate alone does
/// not model per-input native approval (its ToolRequest carries only the
/// input-blind native_requires_approval), so GuardedTool must OR in the
/// inner tool's requires_approval_for; otherwise the inner signal is
/// dead and the runner inline-executes a remove with no human card. Also
/// pins the Ask arm (a tool the gate ASKs for, native approval) and the
/// Deny arm (a deny rule wins; returns false so the runner inline-executes
/// and fail-closes, no misleading card for a denied call).
#[test]
fn test_auto_forwards_input_approval() {
    struct RemoveTool;
    impl Tool for RemoveTool {
        fn name(&self) -> &str {
            "remove_thing"
        }
        fn description(&self) -> &str {
            ""
        }
        fn input_schema(&self) -> Value {
            Value::Object(Map::new())
        }
        fn execute(&self, _ctx: ToolCtx, _input: Value) -> PFut<'_, Result<Value, ToolError>> {
            Box::pin(async { Ok(Value::Null) })
        }
        fn is_read_only(&self) -> bool {
            false
        }
        fn is_destructive(&self) -> bool {
            false
        }
        fn requires_approval(&self) -> bool {
            false
        }
        fn requires_approval_for(&self, input: &Value) -> bool {
            input
                .get("action")
                .and_then(|v| v.as_str())
                .map(|a| a == "remove")
                .unwrap_or(true)
        }
    }
    let gate = Arc::new(DefaultModeGate::with_mode(PermissionMode::Auto));
    let tool: Arc<dyn Tool> = Arc::new(GuardedTool::new(Arc::new(RemoveTool), gate));
    // Input-blind: Auto + non-destructive -> Allow -> false.
    assert!(!tool.requires_approval());
    // Allow arm: the inner tool's per-input verdict is forwarded. remove
    // asks; keep does not.
    let remove = serde_json::json!({"action": "remove"});
    assert!(tool.requires_approval_for(&remove));
    let keep = serde_json::json!({"action": "keep"});
    assert!(!tool.requires_approval_for(&keep));
    // Exercise the trait surface so no RemoveTool method is an uncovered
    // stub (execute goes through the Allow path: Auto + non-destructive).
    assert_eq!(tool.name(), "remove_thing");
    assert_eq!(tool.description(), "");
    let _schema = tool.input_schema();
    let _exec = block_on(tool.execute(ToolCtx::new("t"), keep.clone()));

    // Ask arm: a tool the gate ASKs for (native requires approval) returns
    // true at the pre-check. DangerTool declares native approval, so Auto
    // asks for it; the Ask arm returns true regardless of the inner signal.
    let ask_inner = Arc::new(DangerTool {
        ran: Mutex::new(false),
    });
    let ask_gate = Arc::new(DefaultModeGate::with_mode(PermissionMode::Auto));
    let ask_tool: Arc<dyn Tool> = Arc::new(GuardedTool::new(ask_inner, ask_gate));
    assert!(ask_tool.requires_approval_for(&Value::Null));

    // Deny arm: a deny rule wins; requires_approval_for returns false even
    // when the inner tool would ask, so the runner inline-executes + the
    // enforcement re-check fail-closes (no misleading card for a deny).
    let deny_inner = Arc::new(DangerTool {
        ran: Mutex::new(false),
    });
    let deny_gate = Arc::new(DefaultModeGate::with_mode(PermissionMode::Auto));
    deny_gate.add_rule(Rule::new("danger", Effect::Deny).unwrap());
    let deny_tool: Arc<dyn Tool> = Arc::new(GuardedTool::new(deny_inner, deny_gate));
    assert!(!deny_tool.requires_approval_for(&Value::Null));
}

/// A native-approval exec tool (the bash shape: requires_approval is true
/// and requires_approval_for delegates to it) must NOT re-ask in Auto mode.
/// Auto allows exec, and the gate already saw the input-blind native signal
/// when it chose Allow; forwarding the default per-input verdict (which just
/// restates native) would override the gate and ask for every bash call — a
/// regression. The forward fires only for a strict escalation (per-input
/// asks AND native did not), so the worktree-exit remove still asks while
/// bash auto-runs.
#[test]
fn test_auto_allows_native_exec() {
    // Named "bash" so side_effect_for maps it to Exec (Auto Allows exec).
    struct NativeExecTool;
    impl Tool for NativeExecTool {
        fn name(&self) -> &str {
            "bash"
        }
        fn description(&self) -> &str {
            ""
        }
        fn input_schema(&self) -> Value {
            Value::Object(Map::new())
        }
        fn execute(&self, _ctx: ToolCtx, _input: Value) -> PFut<'_, Result<Value, ToolError>> {
            Box::pin(async { Ok(Value::Null) })
        }
        fn is_read_only(&self) -> bool {
            false
        }
        fn is_destructive(&self) -> bool {
            true
        }
        fn requires_approval(&self) -> bool {
            true
        }
        // requires_approval_for NOT overridden: the trait default delegates
        // to requires_approval, so per-input also returns true.
    }
    let gate = Arc::new(DefaultModeGate::with_mode(PermissionMode::Auto));
    let tool: Arc<dyn Tool> = Arc::new(GuardedTool::new(Arc::new(NativeExecTool), gate));
    // Input-blind: Auto allows exec -> Allow -> false (bash auto-runs).
    assert!(!tool.requires_approval());
    // Input-aware: the default per-input verdict restates native; the gate
    // already allowed despite native, so forwarding must NOT re-ask.
    assert!(
        !tool.requires_approval_for(&Value::Null),
        "a native-approval exec tool must auto-run in Auto mode, not re-ask"
    );
    // Exercise the trait surface so no NativeExecTool method is an
    // uncovered stub (execute goes through the Allow path: Auto + exec).
    assert_eq!(tool.name(), "bash");
    assert_eq!(tool.description(), "");
    let _schema = tool.input_schema();
    let _exec = block_on(tool.execute(ToolCtx::new("t"), Value::Null));
}
