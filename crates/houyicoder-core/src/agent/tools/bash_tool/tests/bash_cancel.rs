//! Tests for this module.

use super::super::*;
use houyicoder_context::{ExecConfig, ExecResult, SandboxError};
use std::env::temp_dir;
use std::path::Path;
use std::time::{Duration, Instant};
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

/// A sandbox whose exec hangs for a long time, like a sleep loop.
struct HangingExec;

impl SandboxSession for HangingExec {
    fn exec_with_config(
        &self,
        _c: &str,
        _cfg: ExecConfig,
    ) -> PFut<'_, Result<ExecResult, SandboxError>> {
        Box::pin(async {
            sleep(Duration::from_millis(200)).await;
            Ok(ExecResult {
                stdout: "done".to_string(),
                stderr: String::new(),
                exit_code: Some(0),
            })
        })
    }
    fn workspace_root(&self) -> Arc<Path> {
        Arc::from(temp_dir())
    }
}

/// Esc must not wait out a long command: with the run token cancelled
/// mid-exec, the tool returns the interrupted outcome promptly instead
/// of blocking until the command finishes. Guards the cancel select arm.
#[tokio::test]
async fn test_cancel_returns_fast() {
    let tool = BashTool::new(Arc::new(HangingExec));
    let token = CancellationToken::new();
    let ctx = ToolCtx::new("c1").with_cancel(token.clone());
    let handle = tokio::spawn(async move {
        tool.execute(ctx, serde_json::json!({"command": "sleep 10"}))
            .await
    });
    sleep(Duration::from_millis(20)).await;
    let start = Instant::now();
    token.cancel();
    let out = handle.await.expect("join").expect("bash result");
    let elapsed = start.elapsed();
    assert!(
        out.get("error").and_then(|e| e.as_str()) == Some("interrupted by user"),
        "cancelled bash reports interrupted, got {out}"
    );
    assert!(
        elapsed < Duration::from_millis(500),
        "cancel must return fast, took {elapsed:?}"
    );
}

/// Without cancellation the same hanging exec runs to completion: the
/// cancel arm must be inert when no token fires.
#[tokio::test]
async fn test_uncancelled_completes() {
    let tool = BashTool::new(Arc::new(HangingExec));
    let token = CancellationToken::new();
    let ctx = ToolCtx::new("c1").with_cancel(token);
    let out = tool
        .execute(ctx, serde_json::json!({"command": "sleep 0"}))
        .await
        .expect("bash result");
    assert_eq!(out["stdout"], "done", "plain run completes: {out}");
}
