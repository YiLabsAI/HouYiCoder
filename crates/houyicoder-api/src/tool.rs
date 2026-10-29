//! The Tool behavior port. The behavior error type (ToolError) and tool call
//! shapes live in the protocol wire module; this module holds the behavior
//! contract that references them. Concrete tool implementations stay in the
//! engine; only the trait descends here so the permission layer depends
//! downward, not back into the engine.

pub mod progress;

use houyicoder_async::{CancellationToken, PFut};
use houyicoder_context::SessionId;
use houyicoder_protocol::extension::ToolError;
use houyicoder_protocol::frontend::permission::{AskReason, AskSource};
use serde_json::Value;
use std::collections::HashSet;
use std::sync::Arc;

use self::progress::ToolProgressReporter;
use crate::capture::ResultCaptureSource;
use crate::hook_fire::HookFire;
use crate::spawn::{AgentIdentity, SpawnHandle};

/// Per-call context a tool receives alongside its input. Owned (no lifetime
/// parameter) so the object-safe Tool trait does not roll two lifetimes into
/// one: the token and the sink are cheap to clone (CancellationToken is Clone;
/// the sink is behind an Arc), and ownership keeps the future Send regardless
/// of how a tool moves them. Non-exhaustive so a future field (a workspace
/// path, an agent identity) lands without reworking every call site.
///
/// The agent loop constructs a ToolCtx per dispatch: call_id is the
/// provider-minted tool-call id, cancel is Some when the loop arms a token
/// (RunCancel / Ctrl-C), progress is Some when a host sink is wired. For
/// non-interactive runs and tests, ToolCtx::new mints one with both None; a
/// tool that ignores ctx pays nothing.
#[derive(Clone)]
#[non_exhaustive]
pub struct ToolCtx {
    /// The provider-minted tool-call id this dispatch answers.
    pub call_id: String,
    /// Cooperative cancellation; None when the dispatch is non-cancellable.
    pub cancel: Option<CancellationToken>,
    /// Host progress sink; None when no host surfaces progress.
    pub progress: Option<Arc<dyn ToolProgressReporter>>,
    /// The session the dispatch runs under; None when the dispatch is not
    /// bound to a session (non-interactive runs, tests). A tool that needs the
    /// raw log (the conversation recall tool replays it) reads this to pick the
    /// session; a tool that ignores ctx pays nothing.
    pub session_id: Option<SessionId>,
    /// The identity of the agent whose turn is running; None when the
    /// dispatch is not agent-aware (a non-interactive run, a test). The
    /// agent tool reads depth to pass into the spawn recursion guard and
    /// subagent_type for the fork-recursion check; a tool that ignores it
    /// pays nothing.
    pub agent_identity: Option<AgentIdentity>,
    /// The spawn port the agent tool calls to launch a child; None when the
    /// dispatch cannot spawn (a non-interactive run, a test, or any tool
    /// that is not the agent tool). The agent loop threads it at call time
    /// the way it threads the progress sink; a tool that does not spawn
    /// pays nothing.
    pub spawn_handle: Option<Arc<dyn SpawnHandle>>,
    /// Agent types a deny rule blocks for this dispatch. The agent tool
    /// reads this after registry lookup to tell a denial from an unknown
    /// type. Computed where permission rules live and threaded here, so the
    /// engine never depends on the permission layer. Defaults to empty; a
    /// tool that ignores it pays nothing.
    pub denied_agents: Arc<HashSet<String>>,
    /// The hook-fire seam a service-layer boundary (the agent tool driving a
    /// sync spawn) calls to fire subagent lifecycle hooks. None when no hooks
    /// are wired (a test, a non-interactive run, or the depth-1-only v0 child
    /// that carries no registry); the caller treats fire as a no-op then.
    /// Threaded per call the way the spawn handle is, so a future child with
    /// its own registry fires correctly without re-wiring.
    pub hook_fire: Option<Arc<dyn HookFire>>,
    /// The port that starts a durable capture of the call's raw output; None
    /// when the host has no store to publish to. A command-running tool asks
    /// it for one capture per dispatch, so the bytes above the context bound
    /// survive somewhere a reader can reach them.
    pub result_capture: Option<Arc<dyn ResultCaptureSource>>,
}

impl ToolCtx {
    /// A minimal context carrying only the call id: no cancellation, no
    /// progress sink, no session, no agent identity, no spawn handle. Used
    /// by non-interactive runs, tests, and as the base the agent loop
    /// extends with with_cancel / with_progress / with_session /
    /// with_agent_identity / with_spawn_handle.
    pub fn new(call_id: impl Into<String>) -> Self {
        Self {
            call_id: call_id.into(),
            cancel: None,
            progress: None,
            session_id: None,
            agent_identity: None,
            spawn_handle: None,
            denied_agents: Arc::new(HashSet::new()),
            hook_fire: None,
            result_capture: None,
        }
    }

    /// Attach a cancellation token the tool can await.
    pub fn with_cancel(mut self, token: CancellationToken) -> Self {
        self.cancel = Some(token);
        self
    }

    /// Attach a host progress sink the tool reports through.
    pub fn with_progress(mut self, reporter: Arc<dyn ToolProgressReporter>) -> Self {
        self.progress = Some(reporter);
        self
    }

    /// Bind the dispatch to a session a session-bound tool (the conversation
    /// recall tool) reads to replay the raw log. The agent loop sets this at
    /// every dispatch site where the session is in scope.
    pub fn with_session(mut self, session: SessionId) -> Self {
        self.session_id = Some(session);
        self
    }

    /// Attach the running agent's identity, set on dispatches inside a
    /// spawned child's turn.
    pub fn with_agent_identity(mut self, identity: AgentIdentity) -> Self {
        self.agent_identity = Some(identity);
        self
    }

    /// Attach the spawn port the agent tool calls to launch a child.
    pub fn with_spawn_handle(mut self, handle: Arc<dyn SpawnHandle>) -> Self {
        self.spawn_handle = Some(handle);
        self
    }

    /// Attach the denied-agent set the agent tool consults after registry
    /// lookup to surface a deny rule distinctly from an unknown type.
    pub fn with_denied_agents(mut self, denied: Arc<HashSet<String>>) -> Self {
        self.denied_agents = denied;
        self
    }

    /// Attach the hook-fire seam the agent tool calls at a sync spawn
    /// boundary (SubagentStart before the child run, SubagentStop before the
    /// return). None on dispatches that cannot fire (no registry wired).
    pub fn with_hook_fire(mut self, fire: Arc<dyn HookFire>) -> Self {
        self.hook_fire = Some(fire);
        self
    }

    /// Attach the port that starts a durable capture of a call's raw output.
    /// The engine sets it where it sets the session, so every dispatched call
    /// can keep the bytes its result was bounded from.
    pub fn with_result_capture(mut self, source: Arc<dyn ResultCaptureSource>) -> Self {
        self.result_capture = Some(source);
        self
    }
}

/// A capability the model can invoke. Fail-closed defaults: a tool is
/// sequential, mutating, destructive, and approval-free unless it says
/// otherwise. Object-safe (PFut) so the registry holds Arc<dyn Tool> and a
/// sandbox or capability wrapper swaps in transparently.
///
/// execute takes the per-call ToolCtx plus owned input JSON (the model
/// argument object) and returns a JSON value the loop wraps into a ToolResult
/// event. An Err becomes the error payload the model sees; the loop continues.
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn input_schema(&self) -> Value;

    /// Run the tool. The result becomes the ToolResult payload; an Err becomes
    /// an error payload (the model sees it, the loop continues).
    fn execute(&self, ctx: ToolCtx, input: Value) -> PFut<'_, Result<Value, ToolError>>;

    /// The input the host will actually run, when a translation stands
    /// between the model's proposal and execution. The engine resolves it
    /// before each judging point — when the call is planned, and again when an
    /// approved call resumes from the log — so the verdict, the approval card,
    /// and the execution all address the same text; the transcript keeps the
    /// model's own call. None means the input runs as written.
    fn resolved_input(&self, input: &Value) -> Option<Value> {
        let _ = input;
        None
    }

    /// Run the tool after a human approved it on the resume path. Default
    /// delegates to execute (non-guarded tools have no gate to skip). A
    /// guarded tool overrides this to honor the human decision: the gate
    /// ask was already answered yes at the popup, so it proceeds past ask
    /// while a deny still blocks (safety and content rules fire at the
    /// enforcement point even after approval). Callers without human
    /// authority must use execute, not this; execute under ask is a misuse
    /// that errors fail-closed.
    fn execute_authorized(&self, ctx: ToolCtx, input: Value) -> PFut<'_, Result<Value, ToolError>> {
        self.execute(ctx, input)
    }

    /// True when this tool may run concurrently with adjacent concurrency-safe
    /// calls. Defaults to is_read_only — a read-only tool cannot conflict with
    /// another read-only tool, so it is safe to batch in parallel. Override to
    /// false for a read-only tool that nonetheless must run alone (rare), or to
    /// true for a mutating tool that is internally safe (also rare).
    fn is_concurrency_safe(&self) -> bool {
        self.is_read_only()
    }

    /// True when this tool does not mutate external state (read-only). Default
    /// false (fail-closed: assume it mutates).
    fn is_read_only(&self) -> bool {
        false
    }

    /// True when this tool effects are hard to reverse (delete, overwrite,
    /// network send). Default true (fail-closed: assume destructive). Drives
    /// the approval gate when requires_approval is overridden.
    fn is_destructive(&self) -> bool {
        true
    }

    /// True when the loop must pause for human approval before executing. The
    /// loop collects approval-requiring calls into an interruption and
    /// returns; the caller approves or rejects, then resume executes the
    /// approved subset. Default false; override for destructive tools.
    fn requires_approval(&self) -> bool {
        false
    }

    /// Input-aware approval pre-check: same contract as requires_approval,
    /// but with the real call input so content/safety rules that depend on
    /// the input (a glob pattern hitting a protected path, a bash command
    /// touching .git) fire at the pre-check, not only at the enforcement
    /// point. The loop calls this with the model's input so it routes an
    /// Ask-gated call to the approval flow instead of inline execute (which
    /// would re-check, hit Ask, and fail-closed). Default delegates to the
    /// input-blind requires_approval (non-guarded tools have no gate).
    fn requires_approval_for(&self, input: &Value) -> bool {
        let _ = input;
        self.requires_approval()
    }

    /// Why this call needs a human verdict, when it does, in the form the
    /// approval card renders. The loop asks once per call and carries the
    /// answer to the frontend, so the card shows the cause the gate found
    /// instead of a second evaluation that may disagree with it. A tool
    /// without a gate has nothing finer to report than its own declaration,
    /// which is what the default supplies; the guarded wrapper overrides this
    /// with the verdict its gate produced.
    fn approval_reason(&self, input: &Value) -> Option<AskReason> {
        self.requires_approval_for(input).then(|| AskReason {
            source: AskSource::ToolNative,
            validator: "tool_native".to_string(),
            detail: format!("{} asks before this call runs", self.name()),
            containment_note: None,
        })
    }

    /// The required arguments this input does not supply in a usable form:
    /// absent, null, an empty string, or a value of a different JSON type than
    /// the schema declares, each named with its declared type. An empty result
    /// means the call can run.
    ///
    /// A model reply cut at the token cap streams its arguments mid-object and
    /// arrives empty or partial, so this is how a caller tells a call the model
    /// meant to make from the fragment a cut left behind. A call that fails
    /// this check cannot run, and asking a human to approve it spends their
    /// attention on a call that is already over. A tool that declares no
    /// required arguments accepts whatever it is given.
    fn invalid_required_arguments(&self, input: &Value) -> Vec<String> {
        let schema = self.input_schema();
        let Some(required) = schema.get("required").and_then(Value::as_array) else {
            return Vec::new();
        };
        let describe = |name: &str| match declared_type(&schema, name) {
            Some(kind) => format!("{name} ({kind})"),
            None => name.to_string(),
        };
        let empty_usable = self.empty_usable_arguments();
        let required = required.iter().filter_map(Value::as_str);
        let Some(object) = input.as_object() else {
            // An input that is not an object supplies none of them.
            return required.map(describe).collect();
        };
        required
            .filter(|name| {
                !is_usable(
                    object.get(*name),
                    declared_type(&schema, name),
                    empty_usable.contains(name),
                )
            })
            .map(describe)
            .collect()
    }

    /// Required string arguments this tool accepts as empty. A field named here
    /// is empty on purpose when the caller clears text, so it is a usable value
    /// rather than the shape a cut reply leaves behind. Default: none, because
    /// an empty command or path is never a call the model meant to make.
    fn empty_usable_arguments(&self) -> &'static [&'static str] {
        &[]
    }
}

/// Whether a required argument's value is one the call can use: present, not
/// null, an empty string only when the tool accepts that form here, and of the
/// JSON type the schema declares when it declares one.
fn is_usable(value: Option<&Value>, declared: Option<&str>, empty_usable: bool) -> bool {
    let Some(value) = value else {
        return false;
    };
    match declared {
        Some("string") => value
            .as_str()
            .is_some_and(|text| empty_usable || !text.is_empty()),
        Some("integer") => value.is_i64() || value.is_u64(),
        Some("number") => value.is_number(),
        Some("boolean") => value.is_boolean(),
        Some("object") => value.is_object(),
        Some("array") => value.is_array(),
        _ => !value.is_null(),
    }
}

/// The JSON type a property's schema declares, when it declares one.
fn declared_type<'a>(schema: &'a Value, name: &str) -> Option<&'a str> {
    schema.get("properties")?.get(name)?.get("type")?.as_str()
}

/// A source of tools the composition root assembles into the registry. The
/// built-in tools implement this as one provider constructed with the
/// runtime sandbox and gate; an external crate implements it to inject its
/// own tools. The composition root gathers every provider and registers
/// each tool, so adding a tool set is adding a provider, not editing the
/// registry call list. Concrete Tool implementations stay in the engine;
/// only this assembly contract descends to the ports layer so an external
/// crate depends downward, never back into the engine.
pub trait ToolProvider: Send + Sync {
    /// Human-readable name for diagnostics: which provider contributed a tool.
    fn name(&self) -> &str;

    /// The tools this provider contributes, as owned trait objects the
    /// composition root registers. Constructed fresh per call so a provider
    /// can wire runtime params (sandbox, gate) into its tools.
    fn tools(&self) -> Vec<Arc<dyn Tool>>;
}

#[cfg(test)]
mod tests;
