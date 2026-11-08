mod background;

mod cancel;

mod deadline;

mod output_limit;

mod capture;

mod shell;

mod timeout;

use houyicoder_api::sandbox::SandboxSession;
use houyicoder_api::tool::Tool;
use houyicoder_async::PFut;
use houyicoder_context::{ExecConfig, ExecResult, SandboxError};
use serde_json::json;
use std::path::Path;
use std::sync::Arc;

use super::output::{BASH_MAX_OUTPUT_CHARS, bound_bash_output, fold_notice};
use super::{BashTool, MAX_TIMEOUT_MS, wall_budget_ms};

/// A session that never runs anything: the schema question does not need one.
struct NoExec;

impl SandboxSession for NoExec {
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

/// The per-call wall budget: the schema advertises a range, and the value the
/// backend receives stays inside it whatever the caller sent.
#[test]
fn test_wall_budget_clamped() {
    let default = ExecConfig::default().wall_timeout_ms;
    assert_eq!(wall_budget_ms(&json!({})), default, "omitted keeps default");
    assert_eq!(
        wall_budget_ms(&json!({"timeout_ms": 300_000})),
        300_000,
        "a declared budget is used as sent"
    );
    assert_eq!(
        wall_budget_ms(&json!({"timeout_ms": 0})),
        1,
        "zero would mean no budget at all, so it floors at one millisecond"
    );
    assert_eq!(
        wall_budget_ms(&json!({"timeout_ms": MAX_TIMEOUT_MS + 1})),
        MAX_TIMEOUT_MS,
        "a larger budget is capped rather than waiting forever"
    );
    assert_eq!(
        wall_budget_ms(&json!({"timeout_ms": -5})),
        default,
        "a negative value is not a budget"
    );
}

/// The tool has to advertise the parameter, or the model cannot discover
/// the budget and falls back to polling with sleep. The description names it
/// in its input line, where a reader looks before the schema.
#[test]
fn test_schema_includes_timeout() {
    let tool = BashTool::new(Arc::new(NoExec));
    let schema = tool.input_schema();
    let param = &schema["properties"]["timeout_ms"];
    assert_eq!(param["type"], "integer");
    assert_eq!(param["maximum"], MAX_TIMEOUT_MS);
    assert_eq!(schema["required"], json!(["command"]));
    assert!(
        tool.description()
            .contains("{command: string, timeout_ms?: number}"),
        "the input line names the budget: {}",
        tool.description()
    );
    // The prose states the default and the cap in words the model reads as
    // numbers. Keep them tied to the values the tool enforces, or a raised
    // default leaves a description that promises a budget it no longer has.
    let default_secs = ExecConfig::default().wall_timeout_ms / 1000;
    assert!(
        tool.description()
            .contains(&format!("{default_secs} seconds")),
        "the default budget is stated as enforced: {}",
        tool.description()
    );
    assert!(
        tool.description().contains(&MAX_TIMEOUT_MS.to_string()),
        "the cap is stated as enforced: {}",
        tool.description()
    );
}

/// A notice about the call itself rides above the command's own stderr, and
/// stands alone when the command printed none.
#[test]
fn test_notice_folded_above_stderr() {
    let notice = Some("undo unavailable".to_string());
    assert_eq!(
        fold_notice(notice.clone(), "boom".into()),
        "undo unavailable\nboom"
    );
    assert_eq!(fold_notice(notice, String::new()), "undo unavailable");
    assert_eq!(fold_notice(None, "boom".into()), "boom");
}

/// A caller that sends the shorter name gets the budget it asked for. The
/// name is not the model's habit to change mid-run: a budget that is read
/// as missing falls back to the default and the long command is stopped
/// once more, which is the failure this parameter exists to prevent.
#[test]
fn test_wall_budget_accepts_timeout() {
    assert_eq!(
        wall_budget_ms(&json!({"timeout": 300_000})),
        300_000,
        "the shorter name declares the same budget"
    );
    assert_eq!(
        wall_budget_ms(&json!({"timeout": MAX_TIMEOUT_MS + 1})),
        MAX_TIMEOUT_MS,
        "the shorter name is clamped like the documented one"
    );
    assert_eq!(
        wall_budget_ms(&json!({"timeout_ms": 4000, "timeout": 900_000})),
        4000,
        "the documented name wins when both are present"
    );
}

/// Output over the cap with no captured stream is bounded to a head+tail stub
/// and tells the model the rest was not kept, rather than naming a workspace
/// file the model would have to re-run a command to read.
#[test]
fn test_truncation_without_capture() {
    let stdout = "x".repeat(BASH_MAX_OUTPUT_CHARS + 1);
    let (out, stderr) = bound_bash_output(stdout, String::new(), None, &[], false);
    assert!(
        out.contains("Output truncated"),
        "the stub says truncated: {out}"
    );
    assert!(
        out.contains("not kept"),
        "the stub says the rest was not kept: {out}"
    );
    assert!(
        !out.contains("saved to") && !out.contains(".log") && !out.contains('/'),
        "no workspace file path is named: {out}"
    );
    assert!(
        stderr.is_empty(),
        "stderr is empty when nothing was kept: {stderr}"
    );
}
