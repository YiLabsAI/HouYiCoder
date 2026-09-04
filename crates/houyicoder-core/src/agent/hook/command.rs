//! The external-process hook: a CommandHook implements the Hook trait by
//! spawning a configured command through the ProcessLauncher port, piping
//! the hook context as JSON to its stdin, and parsing the verdict JSON the
//! command writes to stdout. The clippy spawn ban routes every spawn
//! through ProcessLauncher; a hook command is a trusted user-configured
//! spawn (no kernel fence), but every spawn is audited through the
//! chokepoint so an external command the engine executes leaves a trace.
//!
//! The Hook trait is synchronous; ProcessLauncher spawn is synchronous
//! and hands back live stdio pipes for an interactive spawn. The hook
//! writes the context JSON to stdin, reads the verdict JSON from stdout,
//! and parses it. Blocking I/O sits inside evaluate, which the
//! HookRegistry dispatches on a dedicated thread when a timeout is set
//! (the fast mechanical-rule path); a plain command hook is fast enough
//! that the dispatch thread absorbs the block.
//!
//! The hook context types are runtime types without Serialize derives, so
//! a HookContextJson projection carries the payload over the pipe (the
//! project keeps wire projections separate from core runtime types). The
//! verdict the command returns is a small tagged JSON.

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use serde::{Deserialize, Serialize};

use houyicoder_api::launcher::{ProcessLauncher, SpawnPolicy, SpawnRequest};

use super::filter;
use super::registry::{HookId, HookRegistry};
use super::{Hook, HookContext, HookError, HookEvent, HookSource, HookVerdict};
/// An external-process hook. Spawns the configured program per evaluate,
/// pipes the hook context JSON to stdin, parses the verdict JSON from
/// stdout. The optional matcher and if_condition fields filter before
/// spawn: a hook whose matcher does not match the event's query string, or
/// whose if condition does not match the tool name and input, returns
/// Allow without spawning (the hook is skipped, not failed).
pub struct CommandHook {
    name: String,
    events: Vec<HookEvent>,
    program: String,
    args: Vec<String>,
    launcher: Arc<dyn ProcessLauncher>,
    source: HookSource,
    matcher: Option<String>,
    matcher_regex: Option<regex::Regex>,
    if_condition: Option<String>,
    if_regex: Option<regex::Regex>,
    timeout: Option<std::time::Duration>,
    once: bool,
    fired: AtomicBool,
    hook_reg: Option<Arc<HookRegistry>>,
    hook_id: OnceLock<HookId>,
}

impl CommandHook {
    /// Build a command hook. The launcher is shared with the composition
    /// root (the same launcher the sandbox uses, or a plain trusted one
    /// for a host-process hook command).
    pub fn new(
        name: impl Into<String>,
        events: Vec<HookEvent>,
        program: impl Into<String>,
        args: Vec<String>,
        launcher: Arc<dyn ProcessLauncher>,
        source: HookSource,
    ) -> Self {
        Self {
            name: name.into(),
            events,
            program: program.into(),
            args,
            launcher,
            source,
            matcher: None,
            matcher_regex: None,
            if_condition: None,
            if_regex: None,
            timeout: None,
            once: false,
            fired: AtomicBool::new(false),
            hook_reg: None,
            hook_id: OnceLock::new(),
        }
    }

    /// Attach a matcher pattern. The hook is skipped (returns Allow)
    /// when the event's query string does not match. The pattern is
    /// compiled to a regex once at build time so the hot path does not
    /// recompile per fire. Returns self for chaining.
    pub fn with_matcher(mut self, matcher: impl Into<String>) -> Self {
        let m = matcher.into();
        self.matcher_regex = filter::compile_matcher(&m);
        self.matcher = Some(m);
        self
    }

    /// Attach an if condition (permission-rule syntax). The hook is
    /// skipped when the tool name and input do not satisfy the rule.
    /// The glob pattern is compiled to a regex once at build time.
    /// Returns self for chaining.
    pub fn with_if_condition(mut self, condition: impl Into<String>) -> Self {
        let c = condition.into();
        self.if_regex = filter::compile_if_pattern(&c);
        self.if_condition = Some(c);
        self
    }

    /// Attach a per-hook timeout. None means the registry default
    /// applies. Returns self for chaining.
    pub fn with_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Mark this hook as fire-once. The registry unregisters it after
    /// its first successful fire. Returns self for chaining.
    pub fn with_once(mut self) -> Self {
        self.once = true;
        self
    }

    /// Bind the registry so a once hook can self-unregister after its
    /// first fire. Must be called before registration when once is set,
    /// and bind_hook_id must be called right after register — if the
    /// id is never bound, the hook cannot self-unregister and the
    /// Arc cycle (Registry holds Arc<Hook>, Hook holds Arc<Registry>)
    /// leaks for the process lifetime.
    pub fn with_registry(mut self, reg: Arc<HookRegistry>) -> Self {
        self.hook_reg = Some(reg);
        self
    }

    /// Bind the registry-assigned id so the hook can self-unregister
    /// after a once fire. Called by the registrar right after
    /// registration; a concurrent dispatch that fires before this is
    /// set skips the unregister (the AtomicBool still blocks a second
    /// spawn).
    pub fn bind_hook_id(&self, id: HookId) {
        let _ = self.hook_id.set(id);
    }

    /// Spawn the command, pipe the context JSON, parse the verdict. Split
    /// from evaluate so the once gate and self-unregister wrap it without
    /// duplicating the spawn logic.
    fn evaluate_inner(&self, ctx: &HookContext) -> Result<HookVerdict, HookError> {
        let payload = HookContextJson::from_context(ctx);
        let payload_json = serde_json::to_string(&payload).map_err(|e| HookError::ConfigError {
            detail: format!("hook context encode: {e}"),
        })?;
        let req = SpawnRequest::new(&self.program)
            .with_args(&self.args)
            .interactive();
        // A user-configured hook command is a trusted spawn (no kernel
        // fence: the hook is a program the operator chose to wire). Every
        // hook spawn is audited through the launcher chokepoint so an
        // external command the engine executes leaves a structured trace,
        // regardless of source.
        let policy = SpawnPolicy::default().audited();
        let mut child = self
            .launcher
            .spawn(req, policy)
            .map_err(|e| HookError::ProcessError {
                hook_name: self.name.clone(),
                reason: e.to_string(),
            })?;
        let mut pipes = child.pipes.take().ok_or_else(|| HookError::ProcessError {
            hook_name: self.name.clone(),
            reason: "launcher returned no stdio pipes for an interactive spawn".into(),
        })?;
        if let Some(stdin) = pipes.stdin.as_mut() {
            stdin
                .write_all(payload_json.as_bytes())
                .map_err(|e| HookError::ProcessError {
                    hook_name: self.name.clone(),
                    reason: format!("stdin write: {e}"),
                })?;
            stdin.flush().ok();
        }
        // Drop the stdin handle so the child sees EOF and exits.
        pipes.stdin.take();
        let stdout = pipes
            .stdout
            .as_mut()
            .ok_or_else(|| HookError::ProcessError {
                hook_name: self.name.clone(),
                reason: "launcher returned no stdout pipe".into(),
            })?;
        let mut buf = String::new();
        stdout
            .read_to_string(&mut buf)
            .map_err(|e| HookError::ProcessError {
                hook_name: self.name.clone(),
                reason: format!("stdout read: {e}"),
            })?;
        let mut stderr_buf = String::new();
        if let Some(stderr) = pipes.stderr.as_mut() {
            drop(stderr.read_to_string(&mut stderr_buf));
        }
        // Wait for the child so its exit code is available. The stdout
        // read already blocked to EOF so the child has exited; wait()
        // resolves immediately. pollster::block_on is fine in this sync
        // evaluate (the dispatch path runs evaluate on a dedicated thread
        // when a timeout is set; the fast path is in-process and equally
        // sync).
        let exit_code = pollster::block_on(child.wait())
            .ok()
            .and_then(|e| e.exit_code);
        // Parse the verdict: try the JSON verdict shape first (the
        // structured contract), then fall back to the exit-code contract
        // (a shell-script-style hook that writes no JSON — exit 2 = Deny
        // with stderr as the reason, exit 0 = Allow, other exit codes =
        // a non-blocking error). The exit-code fallback is what makes a
        // plain shell script a valid gate hook without forcing the model
        // to speak JSON.
        let trimmed = buf.trim();
        if trimmed.starts_with('{') {
            let verdict: VerdictJson =
                serde_json::from_str(trimmed).map_err(|e| HookError::InvalidVerdict {
                    hook_name: self.name.clone(),
                    detail: format!("verdict decode: {e} (got {})", buf.trim()),
                })?;
            Ok(verdict.to_hook_verdict(&self.name))
        } else {
            Ok(exit_code_to_verdict(
                exit_code,
                &buf,
                &stderr_buf,
                &self.name,
            ))
        }
    }
}

impl Hook for CommandHook {
    fn name(&self) -> &str {
        &self.name
    }
    fn events(&self) -> &[HookEvent] {
        &self.events
    }
    fn source(&self) -> HookSource {
        self.source.clone()
    }
    fn timeout(&self) -> Option<std::time::Duration> {
        self.timeout
    }
    fn once(&self) -> bool {
        self.once
    }
    fn evaluate(&self, ctx: &HookContext) -> Result<HookVerdict, HookError> {
        if let Some(m) = &self.matcher
            && !filter::matcher_passes_compiled(ctx, m, self.matcher_regex.as_ref())
        {
            return Ok(HookVerdict::Allow);
        }
        if let Some(rule) = &self.if_condition
            && !filter::if_rule_passes_compiled(ctx, rule, self.if_regex.as_ref())
        {
            return Ok(HookVerdict::Allow);
        }
        // once: only the dispatch that wins the compare_exchange spawns.
        // A lost race returns Allow without spawning, so two concurrent
        // dispatches never both run the command.
        if self.once
            && self
                .fired
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            return Ok(HookVerdict::Allow);
        }
        let result = self.evaluate_inner(ctx);
        if self.once {
            if result.is_ok() {
                // The one shot was spent. Unregister so later dispatches
                // skip the hook entirely; the flag stays set so an
                // in-flight dispatch racing the removal still declines.
                // Dispatch releases the read lock before evaluate, so
                // taking the write lock here cannot deadlock.
                if let Some(&id) = self.hook_id.get()
                    && let Some(reg) = &self.hook_reg
                {
                    reg.unregister(id);
                }
            } else {
                // The attempt never produced a verdict, so it did not
                // consume the one shot. Release the gate for the next
                // event rather than silently retiring a hook the user
                // asked to run once and which has not yet run.
                self.fired.store(false, Ordering::Release);
            }
        }
        result
    }
}

/// Translate the shell-style exit-code contract into a HookVerdict. The
/// structured-JSON verdict path handles hooks that write a verdict object;
/// this handles hooks that only use exit codes (the shell-script-friendly
/// contract). exit 0 = Allow, exit 2 = Deny (stderr carries the reason the
/// model sees so it can self-correct), any other exit code = Observe (a
/// non-blocking error surfaced as an observation note rather than a block).
/// None exit code (signal kill) also maps to Observe — a misconfigured hook
/// must not brick the run.
fn exit_code_to_verdict(
    exit_code: Option<i32>,
    stdout: &str,
    stderr: &str,
    hook_name: &str,
) -> HookVerdict {
    match exit_code {
        Some(0) => HookVerdict::Allow,
        Some(2) => {
            let reason = if !stderr.is_empty() {
                stderr.trim().to_string()
            } else if !stdout.is_empty() {
                stdout.trim().to_string()
            } else {
                format!("blocked by hook {hook_name}")
            };
            HookVerdict::Deny(reason)
        }
        Some(code) => {
            // Non-blocking error: surface the stderr as an observation so
            // the user sees the misconfiguration without the run bricking.
            let note = if !stderr.is_empty() {
                stderr.trim().to_string()
            } else {
                format!("hook {hook_name} exited {code}")
            };
            HookVerdict::Observe(note)
        }
        None => HookVerdict::Observe(format!("hook {hook_name} killed by signal")),
    }
}

/// The wire projection of a HookContext over the command hook stdin pipe.
/// Core runtime types stay free of Serialize derives; this struct is the
/// external shape a hook command reads.
#[derive(Debug, Clone, Serialize)]
struct HookContextJson {
    event: String,
    session: String,
    tool_name: Option<String>,
    input: Option<serde_json::Value>,
    result: Option<String>,
    error: Option<String>,
}

impl HookContextJson {
    fn from_context(ctx: &HookContext) -> Self {
        let event = format!("{:?}", ctx.event);
        let mut tool_name = None;
        let mut input = None;
        let mut result = None;
        let mut error = None;
        match &ctx.payload {
            super::HookPayload::PreToolUse {
                tool_name: t,
                input: i,
                ..
            } => {
                tool_name = Some(t.clone());
                input = Some(i.clone());
            }
            super::HookPayload::PostToolUse {
                tool_name: t,
                input: i,
                result: r,
            } => {
                tool_name = Some(t.clone());
                input = Some(i.clone());
                result = Some(r.output.clone());
            }
            super::HookPayload::PostToolUseFailure {
                tool_name: t,
                error: e,
            } => {
                tool_name = Some(t.clone());
                error = Some(e.clone());
            }
            _ => {}
        }
        Self {
            event,
            session: ctx.session.to_string(),
            tool_name,
            input,
            result,
            error,
        }
    }
}

/// The verdict JSON a hook command writes to stdout. Unknown verdict
/// strings map to Allow (the non-blocking default) so a misbehaving hook
/// cannot accidentally block the run.
#[derive(Debug, Clone, Deserialize)]
struct VerdictJson {
    verdict: String,
    #[serde(default)]
    reason: Option<String>,
    /// For a Trigger verdict, the event to fire downstream.
    #[serde(default)]
    event: Option<String>,
}

impl VerdictJson {
    fn to_hook_verdict(&self, hook_name: &str) -> HookVerdict {
        let reason = self.reason.clone().unwrap_or_default();
        match self.verdict.as_str() {
            "allow" => HookVerdict::Allow,
            "deny" => HookVerdict::Deny(reason),
            "feedback" => HookVerdict::Feedback(reason),
            "observe" => HookVerdict::Observe(reason),
            "inject" => HookVerdict::Inject(reason),
            "ask" => HookVerdict::Ask(reason),
            "trigger" => {
                let ev = self.event.as_deref().unwrap_or("");
                match parse_event(ev) {
                    Some(e) => HookVerdict::Trigger(e),
                    // An unknown event name is a hook-author bug, not a
                    // pass. Observe keeps the run non-blocking AND records
                    // the misconfiguration where the user can see it.
                    None => HookVerdict::Observe(format!(
                        "hook {hook_name}: trigger verdict with unknown event '{ev}', ignored"
                    )),
                }
            }
            other => HookVerdict::Observe(format!(
                "hook {hook_name}: unknown verdict '{other}', ignored"
            )),
        }
    }
}

pub fn parse_event(s: &str) -> Option<HookEvent> {
    Some(match s {
        "PreToolUse" => HookEvent::PreToolUse,
        "PostToolUse" => HookEvent::PostToolUse,
        "PostToolUseFailure" => HookEvent::PostToolUseFailure,
        "SessionStart" => HookEvent::SessionStart,
        "SessionEnd" => HookEvent::SessionEnd,
        "Setup" => HookEvent::Setup,
        "UserPromptSubmit" => HookEvent::UserPromptSubmit,
        "Stop" => HookEvent::Stop,
        "StopFailure" => HookEvent::StopFailure,
        "Notification" => HookEvent::Notification,
        "PreCompact" => HookEvent::PreCompact,
        "PostCompact" => HookEvent::PostCompact,
        "PreSelect" => HookEvent::PreSelect,
        "InstructionsLoaded" => HookEvent::InstructionsLoaded,
        "CwdChanged" => HookEvent::CwdChanged,
        "FileChanged" => HookEvent::FileChanged,
        "ConfigChange" => HookEvent::ConfigChange,
        "SubagentStart" => HookEvent::SubagentStart,
        "SubagentStop" => HookEvent::SubagentStop,
        "PermissionRequest" => HookEvent::PermissionRequest,
        "PermissionDenied" => HookEvent::PermissionDenied,
        "TeammateIdle" => HookEvent::TeammateIdle,
        "TaskCreated" => HookEvent::TaskCreated,
        "TaskCompleted" => HookEvent::TaskCompleted,
        "Elicitation" => HookEvent::Elicitation,
        "ElicitationResult" => HookEvent::ElicitationResult,
        "WorktreeCreate" => HookEvent::WorktreeCreate,
        "WorktreeRemove" => HookEvent::WorktreeRemove,
        _ => return None,
    })
}

#[cfg(test)]
#[path = "exit_code_tests.rs"]
mod exit_code_tests;

#[cfg(test)]
#[path = "command_tests.rs"]
mod command_tests;
