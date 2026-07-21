//! GuardedTool wraps a Tool and routes every call through the mode gate. It
//! implements Tool so the runner sees a plain dyn Tool — the core runner and
//! Tool trait stay untouched, the permission layer plugs in at registration.
//!
//! Decision mapping: requires_approval returns true iff the gate says Ask (the
//! runner then pauses for a human). Allow and Deny both return false so the
//! runner calls execute inline; execute re-checks the gate with the real input
//! and refuses on Deny or Ask (Ask here is a misuse guard — the inline path
//! only runs when the gate said Allow or Deny, so a plain execute hitting Ask
//! means a caller skipped the resume contract; fail-closed). The re-check with
//! the real input is what lets content, safety, and compound rules fire at the
//! enforcement point; it is fail-closed: if the mode tightened to Deny between
//! the ask and execution, the call is denied rather than running stale.
//!
//! execute_authorized is the resume-path bridge: the human already answered
//! Yes to the popup, so Ask proceeds (authorized) while Deny still blocks.
//! Here the human result is the authorization itself, consumed directly
//! without re-asking.

use std::sync::Arc;

use houyicoder_api::tool::{Tool, ToolCtx};
use houyicoder_async::PFut;
use houyicoder_protocol::extension::ToolError;
use serde_json::Value;

use crate::decision::{Decision, Outcome};
use crate::gate::ModeGate;
use crate::mode::ToolRequest;

pub struct GuardedTool<T: Tool> {
    inner: Arc<T>,
    gate: Arc<dyn ModeGate>,
}

impl<T: Tool> GuardedTool<T> {
    pub fn new(inner: Arc<T>, gate: Arc<dyn ModeGate>) -> Self {
        Self { inner, gate }
    }

    fn request_for(inner: &T) -> ToolRequest<'_> {
        ToolRequest {
            tool_name: inner.name(),
            input: None,
            is_destructive: inner.is_destructive(),
            is_read_only: inner.is_read_only(),
            native_requires_approval: inner.requires_approval(),
        }
    }
}

impl<T: Tool> Tool for GuardedTool<T> {
    fn name(&self) -> &str {
        self.inner.name()
    }
    fn description(&self) -> &str {
        self.inner.description()
    }
    fn input_schema(&self) -> Value {
        self.inner.input_schema()
    }
    fn is_concurrency_safe(&self) -> bool {
        self.inner.is_concurrency_safe()
    }
    fn is_read_only(&self) -> bool {
        self.inner.is_read_only()
    }
    fn is_destructive(&self) -> bool {
        self.inner.is_destructive()
    }
    fn requires_approval(&self) -> bool {
        let req = Self::request_for(&self.inner);
        self.gate.decide(&req).outcome() == Outcome::Ask
    }
    fn requires_approval_for(&self, input: &Value) -> bool {
        // Route an Ask-gated call to the approval flow at the pre-check. The
        // gate does not model a tool's per-input native approval, so forward
        // the inner per-input verdict on Allow — but only as a strict
        // escalation (per-input asks AND native did not), so the default
        // (which delegates to native) does not re-ask native-approval exec
        // tools the gate already allowed. Deny wins (false -> inline-execute
        // + fail-closed at the enforcement point).
        let req = ToolRequest {
            tool_name: self.inner.name(),
            input: Some(input),
            is_destructive: self.inner.is_destructive(),
            is_read_only: self.inner.is_read_only(),
            native_requires_approval: self.inner.requires_approval(),
        };
        match self.gate.decide(&req) {
            Decision::Ask(_) => true,
            Decision::Allow(_) => {
                self.inner.requires_approval_for(input) && !self.inner.requires_approval()
            }
            Decision::Deny(_) => false,
        }
    }
    fn execute(&self, _ctx: ToolCtx, input: Value) -> PFut<'_, Result<Value, ToolError>> {
        let inner = self.inner.clone();
        let gate = self.gate.clone();
        Box::pin(async move {
            // Re-check with the real input so content, safety, and compound
            // rules fire at the enforcement point. Ask here is a misuse guard:
            // the inline path only runs when the gate said Allow or Deny, so a
            // plain execute hitting Ask means a caller skipped the resume
            // contract — fail-closed rather than silently proceeding.
            let req = ToolRequest {
                tool_name: inner.name(),
                input: Some(&input),
                is_destructive: inner.is_destructive(),
                is_read_only: inner.is_read_only(),
                native_requires_approval: inner.requires_approval(),
            };
            match gate.decide(&req) {
                Decision::Allow(_) => inner.execute(_ctx, input).await,
                Decision::Ask(_) => Err(ToolError::Failed("tool requires approval".into())),
                Decision::Deny(reason) => Err(ToolError::Failed(format!(
                    "denied by mode: {}",
                    reason.detail
                ))),
            }
        })
    }
    fn execute_authorized(
        &self,
        _ctx: ToolCtx,
        input: Value,
    ) -> PFut<'_, Result<Value, ToolError>> {
        let inner = self.inner.clone();
        let gate = self.gate.clone();
        Box::pin(async move {
            // The human answered Yes to the popup (gate said Ask at the ask
            // point). Re-check with the real input only to enforce a Deny that
            // tightened in since (safety/content rules at the enforcement
            // point); Ask is now authorized, so proceed rather than error.
            let req = ToolRequest {
                tool_name: inner.name(),
                input: Some(&input),
                is_destructive: inner.is_destructive(),
                is_read_only: inner.is_read_only(),
                native_requires_approval: inner.requires_approval(),
            };
            match gate.decide(&req) {
                Decision::Allow(_) | Decision::Ask(_) => inner.execute(_ctx, input).await,
                Decision::Deny(reason) => Err(ToolError::Failed(format!(
                    "denied by mode: {}",
                    reason.detail
                ))),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gate::DefaultModeGate;
    use crate::mode::PermissionMode;
    use crate::rule::{Effect, Rule};
    use houyicoder_protocol::extension::ToolError;
    use pollster::block_on;

    /// A destructive tool that records whether execute ran. Tracks the shape
    /// of the real bash/edit tools (destructive + requires approval) so the
    /// gate decisions exercise the real code path.
    struct DangerTool {
        ran: std::sync::Mutex<bool>,
    }
    impl Tool for DangerTool {
        fn name(&self) -> &str {
            "danger"
        }
        fn description(&self) -> &str {
            ""
        }
        fn input_schema(&self) -> Value {
            Value::Object(serde_json::Map::new())
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
            ran: std::sync::Mutex::new(false),
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
            ran: std::sync::Mutex::new(false),
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
            ran: std::sync::Mutex::new(false),
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

    /// A read-only tool whose input touches a protected path (a glob of
    /// .git/) must escalate to Ask at the INPUT-AWARE pre-check — the
    /// input-blind requires_approval misses it (safety_check sees no content
    /// with input=None), so the runner would inline-execute and fail-closed.
    /// This is the pre-check / re-check asymmetry fix.
    #[test]
    fn test_precheck_catches_protected_path() {
        struct GlobTool;
        impl Tool for GlobTool {
            fn name(&self) -> &str {
                "glob"
            }
            fn description(&self) -> &str {
                ""
            }
            fn input_schema(&self) -> Value {
                Value::Object(serde_json::Map::new())
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
        // Input-aware pre-check: the pattern hits .git/ -> safety Ask ->
        // true (routes to the approval flow, not inline execute).
        let input = serde_json::json!({"pattern": ".git/config"});
        assert!(
            tool.requires_approval_for(&input),
            "glob of a protected path must escalate to Ask at the pre-check"
        );
        // Exercise the trait surface the gate reads so no GlobTool method
        // is left as an uncovered stub.
        assert_eq!(tool.name(), "glob");
        assert_eq!(tool.description(), "");
        let _schema = tool.input_schema();
        let _exec = block_on(tool.execute(ToolCtx::new("t"), Value::Null));
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
                Value::Object(serde_json::Map::new())
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
            ran: std::sync::Mutex::new(false),
        });
        let ask_gate = Arc::new(DefaultModeGate::with_mode(PermissionMode::Auto));
        let ask_tool: Arc<dyn Tool> = Arc::new(GuardedTool::new(ask_inner, ask_gate));
        assert!(ask_tool.requires_approval_for(&Value::Null));

        // Deny arm: a deny rule wins; requires_approval_for returns false even
        // when the inner tool would ask, so the runner inline-executes + the
        // enforcement re-check fail-closes (no misleading card for a deny).
        let deny_inner = Arc::new(DangerTool {
            ran: std::sync::Mutex::new(false),
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
                Value::Object(serde_json::Map::new())
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
}
