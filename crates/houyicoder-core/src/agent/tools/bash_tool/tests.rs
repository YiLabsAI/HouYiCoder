mod bash_cancel;

mod bash_bound;

use std::path::Path;
use std::sync::Arc;

use houyicoder_api::sandbox::SandboxSession;
use houyicoder_api::tool::Tool;
use houyicoder_async::PFut;
use houyicoder_context::{ExecConfig, ExecResult, SandboxError};
use serde_json::json;

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

/// The schema has to advertise the parameter, or the model cannot discover
/// the budget and falls back to polling with sleep.
#[test]
fn test_schema_advertises_timeout() {
    let schema = BashTool::new(Arc::new(NoExec)).input_schema();
    let param = &schema["properties"]["timeout_ms"];
    assert_eq!(param["type"], "integer");
    assert_eq!(param["maximum"], MAX_TIMEOUT_MS);
    assert_eq!(schema["required"], json!(["command"]));
}
