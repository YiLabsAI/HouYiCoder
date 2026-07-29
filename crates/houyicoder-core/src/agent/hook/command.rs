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
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use houyicoder_api::launcher::{ProcessLauncher, SpawnPolicy, SpawnRequest};

use super::{Hook, HookContext, HookError, HookEvent, HookSource, HookVerdict};
/// An external-process hook. Spawns the configured program per evaluate,
/// pipes the hook context JSON to stdin, parses the verdict JSON from
/// stdout.
pub struct CommandHook {
    name: String,
    events: Vec<HookEvent>,
    program: String,
    args: Vec<String>,
    launcher: Arc<dyn ProcessLauncher>,
    source: HookSource,
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
    fn evaluate(&self, ctx: &HookContext) -> Result<HookVerdict, HookError> {
        let payload = HookContextJson::from_context(ctx);
        let payload_json = serde_json::to_string(&payload).map_err(|e| HookError::ConfigError {
            detail: format!("hook context encode: {e}"),
        })?;
        let req = SpawnRequest::new(&self.program)
            .with_args(&self.args)
            .interactive();
        // A user-configured hook command is a trusted spawn (no kernel fence:
        // the hook is a program the operator chose to wire). Every hook spawn
        // is audited through the launcher chokepoint so an external command the
        // engine executes leaves a structured trace, regardless of source.
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
        // Wait for the child so its exit code is available. The stdout read
        // already blocked to EOF so the child has exited; wait() resolves
        // immediately. pollster::block_on is fine in this sync evaluate
        // (the dispatch path runs evaluate on a dedicated thread when a
        // timeout is set; the fast path is in-process and equally sync).
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
mod tests {
    use super::*;
    use houyicoder_api::launcher::{LauncherChild, LauncherExit, SpawnError, StdioPipes};
    use houyicoder_context::SessionId;

    /// A stub launcher whose command returns a canned verdict JSON on
    /// stdout. The pipes are in-memory handles so the CommandHook
    /// write-stdin / read-stdout path exercises end-to-end.
    struct StubLauncher {
        stdout: String,
    }
    impl ProcessLauncher for StubLauncher {
        fn spawn(
            &self,
            _req: SpawnRequest,
            _policy: SpawnPolicy,
        ) -> Result<LauncherChild, SpawnError> {
            let stdout_buf = self.stdout.clone().into_bytes();
            let stdout: Box<dyn std::io::Read + Send> = Box::new(std::io::Cursor::new(stdout_buf));
            let stdin: Box<dyn std::io::Write + Send> = Box::new(std::io::sink());
            let pipes = StdioPipes {
                stdin: Some(stdin),
                stdout: Some(stdout),
                stderr: None,
            };
            Ok(LauncherChild::with_pipes(
                None,
                pipes,
                Box::pin(async move {
                    Ok(LauncherExit {
                        exit_code: Some(0),
                        stdout: None,
                        stderr: None,
                    })
                }),
            ))
        }
    }

    pub(crate) fn ctx_pre_tool_use() -> HookContext {
        HookContext {
            event: HookEvent::PreToolUse,
            payload: super::super::HookPayload::PreToolUse {
                tool_name: "recordable".into(),
                input: serde_json::json!({}),
                backfilled_input: None,
            },
            session: SessionId::new(),
        }
    }

    #[tokio::test]
    async fn test_deny_verdict_round_trips() {
        let launcher = Arc::new(StubLauncher {
            stdout: r#"{"verdict":"deny","reason":"blocked by command hook"}"#.into(),
        }) as Arc<dyn ProcessLauncher>;
        let hook = CommandHook::new(
            "cmd-deny",
            vec![HookEvent::PreToolUse],
            "echo",
            vec![],
            launcher,
            HookSource::Project,
        );
        let v = hook.evaluate(&ctx_pre_tool_use()).expect("evaluate");
        match v {
            HookVerdict::Deny(r) => assert_eq!(r, "blocked by command hook"),
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_allow_verdict_round_trips() {
        let launcher = Arc::new(StubLauncher {
            stdout: r#"{"verdict":"allow"}"#.into(),
        }) as Arc<dyn ProcessLauncher>;
        let hook = CommandHook::new(
            "cmd-allow",
            vec![HookEvent::PreToolUse],
            "echo",
            vec![],
            launcher,
            HookSource::Project,
        );
        let v = hook.evaluate(&ctx_pre_tool_use()).expect("evaluate");
        assert!(matches!(v, HookVerdict::Allow));
    }

    #[tokio::test]
    async fn test_unknown_verdict_not_allowed() {
        let launcher = Arc::new(StubLauncher {
            stdout: r#"{"verdict":"bogus"}"#.into(),
        }) as Arc<dyn ProcessLauncher>;
        let hook = CommandHook::new(
            "cmd-bogus",
            vec![HookEvent::PreToolUse],
            "echo",
            vec![],
            launcher,
            HookSource::Project,
        );
        let v = hook.evaluate(&ctx_pre_tool_use()).expect("evaluate");
        match v {
            HookVerdict::Observe(note) => assert!(
                note.contains("unknown verdict"),
                "observe names the misconfiguration: {note}"
            ),
            other => panic!("expected Observe, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_malformed_json_hook_error() {
        // stdout that starts with { but fails to parse is a malformed
        // verdict object, not the exit-code contract. The hook tried to
        // speak JSON and got it wrong, so an InvalidVerdict error surfaces
        // (the model can see which hook misconfigured itself). Plain
        // non-JSON stdout (no leading {) falls through to the exit-code
        // contract instead.
        let launcher = Arc::new(StubLauncher {
            stdout: "{not valid json".into(),
        }) as Arc<dyn ProcessLauncher>;
        let hook = CommandHook::new(
            "cmd-bad",
            vec![HookEvent::PreToolUse],
            "echo",
            vec![],
            launcher,
            HookSource::Project,
        );
        let err = hook
            .evaluate(&ctx_pre_tool_use())
            .expect_err("malformed json");
        assert!(matches!(err, HookError::InvalidVerdict { .. }));
        // Trait accessors (cover the name/events/source surface).
        assert_eq!(hook.name(), "cmd-bad");
        assert_eq!(hook.events(), &[HookEvent::PreToolUse]);
        assert_eq!(hook.source(), HookSource::Project);
    }

    fn ctx_post_tool_use() -> HookContext {
        HookContext {
            event: HookEvent::PostToolUse,
            payload: super::super::HookPayload::PostToolUse {
                tool_name: "recordable".into(),
                input: serde_json::json!({"x": 1}),
                result: super::super::ToolResult {
                    output: "ok".into(),
                },
            },
            session: SessionId::new(),
        }
    }

    fn ctx_post_tool_use_failure() -> HookContext {
        HookContext {
            event: HookEvent::PostToolUseFailure,
            payload: super::super::HookPayload::PostToolUseFailure {
                tool_name: "recordable".into(),
                error: "boom".into(),
            },
            session: SessionId::new(),
        }
    }

    #[tokio::test]
    async fn test_post_tool_use_payload() {
        // The PostToolUse branch of from_context carries the result field.
        let launcher = Arc::new(StubLauncher {
            stdout: r#"{"verdict":"allow"}"#.into(),
        }) as Arc<dyn ProcessLauncher>;
        let hook = CommandHook::new(
            "cmd-post",
            vec![HookEvent::PostToolUse],
            "echo",
            vec![],
            launcher,
            HookSource::Project,
        );
        let v = hook.evaluate(&ctx_post_tool_use()).expect("evaluate");
        assert!(matches!(v, HookVerdict::Allow));
    }

    #[tokio::test]
    async fn test_post_tool_use_failure() {
        // The PostToolUseFailure branch of from_context carries the error.
        let launcher = Arc::new(StubLauncher {
            stdout: r#"{"verdict":"allow"}"#.into(),
        }) as Arc<dyn ProcessLauncher>;
        let hook = CommandHook::new(
            "cmd-fail",
            vec![HookEvent::PostToolUseFailure],
            "echo",
            vec![],
            launcher,
            HookSource::Project,
        );
        let v = hook
            .evaluate(&ctx_post_tool_use_failure())
            .expect("evaluate");
        assert!(matches!(v, HookVerdict::Allow));
    }

    #[tokio::test]
    async fn test_trigger_verdict_round_trips() {
        // A Trigger verdict with a known event maps to HookVerdict::Trigger.
        let launcher = Arc::new(StubLauncher {
            stdout: r#"{"verdict":"trigger","event":"PreCompact"}"#.into(),
        }) as Arc<dyn ProcessLauncher>;
        let hook = CommandHook::new(
            "cmd-trigger",
            vec![HookEvent::PreToolUse],
            "echo",
            vec![],
            launcher,
            HookSource::Project,
        );
        let v = hook.evaluate(&ctx_pre_tool_use()).expect("evaluate");
        assert!(matches!(v, HookVerdict::Trigger(HookEvent::PreCompact)));
    }

    #[tokio::test]
    async fn test_trigger_unknown_event_observed() {
        let launcher = Arc::new(StubLauncher {
            stdout: r#"{"verdict":"trigger","event":"NotAnEvent"}"#.into(),
        }) as Arc<dyn ProcessLauncher>;
        let hook = CommandHook::new(
            "cmd-trigger-bad",
            vec![HookEvent::PreToolUse],
            "echo",
            vec![],
            launcher,
            HookSource::Project,
        );
        let v = hook.evaluate(&ctx_pre_tool_use()).expect("evaluate");
        match v {
            HookVerdict::Observe(note) => assert!(
                note.contains("unknown event"),
                "observe names the misconfiguration: {note}"
            ),
            other => panic!("expected Observe, got {other:?}"),
        }
    }

    /// A launcher that refuses to spawn, so the spawn-error arm runs.
    struct FailingLauncher;
    impl ProcessLauncher for FailingLauncher {
        fn spawn(
            &self,
            _req: SpawnRequest,
            _policy: SpawnPolicy,
        ) -> Result<LauncherChild, SpawnError> {
            Err(SpawnError::Io("stub refuses spawn".into()))
        }
    }

    /// A launcher that records the policy it was handed and returns a canned
    /// allow verdict, so a test can assert the spawn policy the hook built.
    struct PolicyRecordingLauncher {
        policy: std::sync::Mutex<Option<SpawnPolicy>>,
        stdout: String,
    }
    impl ProcessLauncher for PolicyRecordingLauncher {
        fn spawn(
            &self,
            _req: SpawnRequest,
            policy: SpawnPolicy,
        ) -> Result<LauncherChild, SpawnError> {
            *self.policy.lock().unwrap() = Some(policy);
            let stdout: Box<dyn std::io::Read + Send> =
                Box::new(std::io::Cursor::new(self.stdout.clone().into_bytes()));
            let stdin: Box<dyn std::io::Write + Send> = Box::new(std::io::sink());
            let pipes = StdioPipes {
                stdin: Some(stdin),
                stdout: Some(stdout),
                stderr: None,
            };
            Ok(LauncherChild::with_pipes(
                None,
                pipes,
                Box::pin(async move {
                    Ok(LauncherExit {
                        exit_code: Some(0),
                        stdout: None,
                        stderr: None,
                    })
                }),
            ))
        }
    }

    #[tokio::test]
    async fn test_hook_spawn_policy_audited() {
        // Every hook-command spawn must carry audit=true so an external command
        // the engine executes leaves a structured trace through the launcher
        // chokepoint, regardless of the hook's source.
        let launcher = Arc::new(PolicyRecordingLauncher {
            policy: std::sync::Mutex::new(None),
            stdout: r#"{"verdict":"allow"}"#.into(),
        });
        let policy_slot = Arc::clone(&launcher);
        let hook = CommandHook::new(
            "cmd-audit",
            vec![HookEvent::PreToolUse],
            "echo",
            vec![],
            launcher as Arc<dyn ProcessLauncher>,
            HookSource::User,
        );
        drop(hook.evaluate(&ctx_pre_tool_use()).expect("evaluate"));
        let captured = policy_slot
            .policy
            .lock()
            .unwrap()
            .clone()
            .expect("spawn ran");
        assert!(captured.audit, "hook spawn must be audited");
    }

    #[tokio::test]
    async fn test_spawn_failure_hook_error() {
        let launcher = Arc::new(FailingLauncher) as Arc<dyn ProcessLauncher>;
        let hook = CommandHook::new(
            "cmd-nospawn",
            vec![HookEvent::PreToolUse],
            "echo",
            vec![],
            launcher,
            HookSource::Project,
        );
        let err = hook.evaluate(&ctx_pre_tool_use()).expect_err("spawn fails");
        assert!(matches!(err, HookError::ProcessError { .. }));
    }
}
