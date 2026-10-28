//! GuardedTool wraps a Tool and routes every call through the mode gate. It
//! implements Tool so the runner sees a plain dyn Tool — the core runner and
//! Tool trait stay untouched, the permission layer plugs in at registration.
//!
//! Decision mapping: requires_approval reports whether the gate asks about the
//! tool itself; requires_approval_for asks about one call with the model's
//! input and can also escalate on the tool's own per-input declaration. Allow
//! and Deny return false and execute re-checks the gate with the real input,
//! refusing on Deny or Ask. That re-check is where content, safety, and
//! compound rules fire, and it is fail-closed: a mode that tightened to Deny
//! between the ask and execution denies the call rather than running it stale.
//! A call that misses a required argument never reaches the human.
//!
//! execute_authorized is the resume-path bridge: the human already answered
//! Yes to the popup, so Ask proceeds (authorized) while Deny still blocks.
//! Here the human result is the authorization itself, consumed directly
//! without re-asking.

use std::sync::Arc;

use houyicoder_api::tool::{Tool, ToolCtx};
use houyicoder_async::PFut;
use houyicoder_protocol::extension::ToolError;
use houyicoder_protocol::frontend::permission::AskReason;
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
        self.approval_reason(input).is_some()
    }
    fn approval_reason(&self, input: &Value) -> Option<AskReason> {
        // The gate judges the call once, here, and the loop carries this
        // answer to the card: a second evaluation after the run parked could
        // see a mode or rule that changed in between and describe a call the
        // gate no longer asks about. A call whose input misses required
        // arguments is not put to anyone — the tool layer reports the schema
        // error and the model corrects itself.
        if !self.inner.invalid_required_arguments(input).is_empty() {
            return None;
        }
        let req = ToolRequest {
            tool_name: self.inner.name(),
            input: Some(input),
            is_destructive: self.inner.is_destructive(),
            is_read_only: self.inner.is_read_only(),
            native_requires_approval: self.inner.requires_approval(),
        };
        match self.gate.decide(&req) {
            Decision::Ask(reason) => Some(AskReason::from(&reason)),
            // The gate does not model a tool's per-input native approval, so
            // the inner per-input verdict is forwarded on Allow — but only as
            // a strict escalation (per-input asks AND native did not), so the
            // default (which delegates to native) does not re-ask
            // native-approval exec tools the gate already allowed. Deny wins
            // (None -> inline execute + fail-closed at the enforcement point).
            Decision::Allow(_) if !self.inner.requires_approval() => {
                Tool::approval_reason(self.inner.as_ref(), input)
            }
            Decision::Allow(_) | Decision::Deny(_) => None,
        }
    }
    fn execute(&self, _ctx: ToolCtx, input: Value) -> PFut<'_, Result<Value, ToolError>> {
        let inner = self.inner.clone();
        let gate = self.gate.clone();
        Box::pin(async move {
            // A call that fails its tool's required-argument check cannot run,
            // so it is answered with that schema error rather than the gate's
            // verdict. Asking would spend a human on a call that fails either
            // way, and the model corrects itself from the error.
            let invalid = inner.invalid_required_arguments(&input);
            if !invalid.is_empty() {
                return Err(ToolError::InvalidInput(format!(
                    "{}: {} required",
                    inner.name(),
                    invalid.join(", ")
                )));
            }
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
mod tests;
