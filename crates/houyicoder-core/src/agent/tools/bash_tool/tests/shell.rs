//! Tests for the command a host with a translated shell receives.

use super::super::BashTool;
use super::NoExec;
use houyicoder_api::sandbox::SandboxSession;
use houyicoder_api::shell::Shell;
use houyicoder_api::tool::Tool;
use houyicoder_async::PFut;
use houyicoder_context::{ExecConfig, ExecResult, SandboxError};
use serde_json::json;
use std::path::Path;
use std::sync::Arc;

/// A session reporting the given shell.
struct ShellSession(Shell);

impl SandboxSession for ShellSession {
    fn shell(&self) -> Shell {
        self.0
    }

    fn exec_with_config(
        &self,
        _command: &str,
        _config: ExecConfig,
    ) -> PFut<'_, Result<ExecResult, SandboxError>> {
        Box::pin(async { Err(SandboxError::Unsupported("test session".into())) })
    }

    fn workspace_root(&self) -> Arc<Path> {
        Arc::from(Path::new("/nonexistent"))
    }
}

/// A Git Bash host runs the command with the POSIX null device. The rest of
/// the call is the caller's and passes through untouched, so a budget or a
/// field added later survives the translation.
#[test]
fn test_git_bash_translates_null() {
    let tool = BashTool::new(Arc::new(ShellSession(Shell::GitBash)));
    let effective = tool
        .resolved_input(&json!({"command": "echo hi >nul", "timeout_ms": 5000}))
        .expect("a null redirect is translated");
    assert_eq!(effective["command"], "echo hi >/dev/null");
    assert_eq!(
        effective["timeout_ms"], 5000,
        "the rest of the call is untouched"
    );
}

/// A command interpreter host reads nul as its own null device, so the
/// command runs as written and no translated input exists.
#[test]
fn test_cmd_shell_keeps_command() {
    let tool = BashTool::new(Arc::new(ShellSession(Shell::Cmd)));
    assert!(
        tool.resolved_input(&json!({"command": "echo hi >nul"}))
            .is_none(),
        "the command interpreter already understands the redirect"
    );
}

/// A unix host reads nul as an ordinary file name, so the command runs as
/// written and no translated input exists.
#[test]
fn test_posix_shell_keeps_command() {
    let tool = BashTool::new(Arc::new(NoExec));
    assert!(
        tool.resolved_input(&json!({"command": "echo hi >nul"}))
            .is_none(),
        "a POSIX shell has nothing to translate"
    );
    assert!(
        tool.resolved_input(&json!({"command": "echo hi"}))
            .is_none(),
        "a command with no redirect has nothing to translate"
    );
    assert!(
        tool.resolved_input(&json!({})).is_none(),
        "a call without a command is answered by the schema check"
    );
}
