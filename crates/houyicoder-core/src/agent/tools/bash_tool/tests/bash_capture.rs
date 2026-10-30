//! Tests for the capture path: the tool names the blocks holding the raw
//! output its bounded result dropped, says so when the capture failed or
//! stopped the command, and leaves the result unchanged otherwise.

use std::env;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicI64;

use houyicoder_api::capture::{
    CallEnd, CaptureError, CaptureStop, CaptureStopReason, CaptureVerdict, CapturedStream,
    CapturedStreams, RawOutputReceiver, ResultCapture, ResultCaptureSource,
};
use houyicoder_api::sandbox::SandboxSession;
use houyicoder_api::session::SessionLog;
use houyicoder_api::tool::{Tool, ToolCtx};
use houyicoder_async::PFut;
use houyicoder_context::{BlockHash, ExecConfig, ExecResult, SandboxError, SessionId};
use houyicoder_memory::InMemoryBackend;
use houyicoder_session::SessionStore;
use serde_json::json;

use super::super::BashTool;
use crate::agent::tools::SessionResultCapture;

/// A session that prints a fixed payload through the capture path and answers
/// with the same bytes as the command's result.
struct LargeOutputSession {
    payload: Vec<u8>,
    root: Arc<Path>,
}

impl LargeOutputSession {
    /// A session whose workspace cannot hold a spill file, so the spill path
    /// falls back to the in-memory stub.
    fn new(payload: Vec<u8>) -> Self {
        Self {
            payload,
            root: Arc::from(Path::new("/nonexistent")),
        }
    }

    /// A session whose workspace is a real directory, so a spill file is
    /// written and the note beside it can be read.
    fn spilling(payload: Vec<u8>, root: &Path) -> Self {
        Self {
            payload,
            root: Arc::from(root),
        }
    }
}

impl SandboxSession for LargeOutputSession {
    fn exec_with_config(
        &self,
        _command: &str,
        _config: ExecConfig,
    ) -> PFut<'_, Result<ExecResult, SandboxError>> {
        Box::pin(async {
            Err(SandboxError::Unsupported(
                "this session only answers captured exec".into(),
            ))
        })
    }
    fn workspace_root(&self) -> Arc<Path> {
        Arc::clone(&self.root)
    }
    fn exec_streaming_captured(
        &self,
        _command: &str,
        _lines: Arc<AtomicI64>,
        _config: ExecConfig,
        receiver: Option<Arc<dyn RawOutputReceiver>>,
    ) -> PFut<'_, Result<ExecResult, SandboxError>> {
        let payload = self.payload.clone();
        Box::pin(async move {
            if let Some(receiver) = receiver {
                for chunk in payload.chunks(4096) {
                    receiver.receive(CapturedStream::Stdout, chunk);
                }
            }
            Ok(ExecResult {
                stdout: String::from_utf8(payload).expect("the test prints text"),
                stderr: String::new(),
                exit_code: Some(0),
            })
        })
    }
}

fn payload() -> Vec<u8> {
    (0..70_000).map(|i| b'a' + (i % 26) as u8).collect()
}

/// The model gets a bounded view, and the block the result names holds every
/// byte the command printed, including the part the bound dropped.
#[test]
fn test_capture_names_full_output() {
    let log: Arc<dyn SessionLog> = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let printed = payload();
    let tool = BashTool::new(Arc::new(LargeOutputSession::new(printed.clone())));
    let ctx = ToolCtx::new("call-1")
        .with_session(SessionId::new())
        .with_result_capture(Arc::new(SessionResultCapture::new(log.clone())));

    let output = pollster::block_on(tool.execute(ctx, json!({"command": "print"})))
        .expect("the command answers");
    let bounded = output["stdout"].as_str().expect("stdout is a string");
    assert!(
        bounded.len() < printed.len(),
        "the model view stays bounded"
    );
    let hash = output["raw_capture"]["stdout"]["block_ref"]
        .as_str()
        .expect("the result names the block holding the full stdout");
    let bytes = pollster::block_on(log.backend().block_get(&BlockHash(hash.to_string())))
        .expect("the named block is readable");
    assert_eq!(bytes, printed, "the capture kept every byte printed");
    assert!(
        output["raw_capture"].get("stop_reason").is_none(),
        "the command ended on its own, so nothing stopped the capture"
    );

    let record_hash = output["raw_capture"]["record"]["block_ref"]
        .as_str()
        .expect("the result names the record block");
    let record = pollster::block_on(log.backend().block_get(&BlockHash(record_hash.to_string())))
        .expect("the record is readable");
    let record: serde_json::Value = serde_json::from_slice(&record).expect("the record is JSON");
    assert_eq!(record["call"], "call-1");
    assert_eq!(record["tool"], "bash");
    assert_eq!(record["exit"], 0, "the record binds how the command ended");
}

/// Without a capture source the answer carries no marker: there is no block
/// to name and the store is never touched.
#[test]
fn test_absent_capture_keeps_result() {
    let tool = BashTool::new(Arc::new(LargeOutputSession::new(payload())));
    let ctx = ToolCtx::new("call-1").with_session(SessionId::new());
    let output = pollster::block_on(tool.execute(ctx, json!({"command": "print"})))
        .expect("the command answers");
    assert!(
        output.get("raw_capture").is_none(),
        "no capture source, no block to name"
    );
    assert!(output["stdout"].is_string());
}

/// Output the model receives whole gains no block: it would be a second copy
/// of the result the tool already returns.
#[test]
fn test_short_output_skips_block() {
    let log: Arc<dyn SessionLog> = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let tool = BashTool::new(Arc::new(LargeOutputSession::new(b"short output".to_vec())));
    let ctx = ToolCtx::new("call-1")
        .with_session(SessionId::new())
        .with_result_capture(Arc::new(SessionResultCapture::new(log)));
    let output = pollster::block_on(tool.execute(ctx, json!({"command": "print"})))
        .expect("the command answers");
    assert_eq!(output["stdout"], "short output");
    assert!(
        output.get("raw_capture").is_none(),
        "nothing was dropped, so nothing is kept"
    );
}

/// A backend that does not read its own pipes captures nothing. The output a
/// result carries after the command ended is the bounded copy, not the
/// stream: keeping it would be a second copy, and it could neither stop a
/// producer nor be called raw. So the default path names no block.
#[test]
fn test_default_path_captures_nothing() {
    struct PlainSession {
        payload: Vec<u8>,
    }

    impl SandboxSession for PlainSession {
        fn exec_with_config(
            &self,
            _command: &str,
            _config: ExecConfig,
        ) -> PFut<'_, Result<ExecResult, SandboxError>> {
            let payload = self.payload.clone();
            Box::pin(async move {
                Ok(ExecResult {
                    stdout: String::from_utf8(payload).expect("the test prints text"),
                    stderr: String::new(),
                    exit_code: Some(0),
                })
            })
        }
        fn workspace_root(&self) -> Arc<Path> {
            Arc::from(Path::new("/nonexistent"))
        }
    }

    let log: Arc<dyn SessionLog> = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let tool = BashTool::new(Arc::new(PlainSession { payload: payload() }));
    let ctx = ToolCtx::new("call-1")
        .with_session(SessionId::new())
        .with_result_capture(Arc::new(SessionResultCapture::new(log)));
    let output = pollster::block_on(tool.execute(ctx, json!({"command": "print"})))
        .expect("the command answers");
    assert!(
        output.get("raw_capture").is_none(),
        "a backend that keeps no stream names no block"
    );
}

/// A capture that cannot publish is named in the result. The model still
/// receives the bounded output; what it must not do is believe the raw above
/// the bound was kept.
#[test]
fn test_capture_failure_is_reported() {
    struct FailingCapture {
        stop: Option<CaptureStop>,
    }

    impl RawOutputReceiver for FailingCapture {
        fn receive(&self, _: CapturedStream, _: &[u8]) -> CaptureVerdict {
            CaptureVerdict::Continue
        }
    }

    impl ResultCapture for FailingCapture {
        fn stop(&self) -> Option<CaptureStop> {
            self.stop.clone()
        }
        fn finish(&self, _: CallEnd) -> PFut<'_, Result<Option<CapturedStreams>, CaptureError>> {
            Box::pin(async { Err(CaptureError::StreamWrite("the store is full".into())) })
        }
    }

    struct FailingSource {
        stop: Option<CaptureStop>,
    }

    impl ResultCaptureSource for FailingSource {
        fn begin(&self, _: SessionId, _: &str, _: &str) -> Option<Arc<dyn ResultCapture>> {
            Some(Arc::new(FailingCapture {
                stop: self.stop.clone(),
            }))
        }
    }

    let printed = payload();
    let tool = BashTool::new(Arc::new(LargeOutputSession::new(printed.clone())));
    let ctx = ToolCtx::new("call-1")
        .with_session(SessionId::new())
        .with_result_capture(Arc::new(FailingSource { stop: None }));
    let output = pollster::block_on(tool.execute(ctx, json!({"command": "print"})))
        .expect("a publication failure does not fail the call");
    assert_eq!(
        output["raw_capture"]["capture_failed"],
        "the raw output above the bound was not kept"
    );
    let bounded = output["stdout"].as_str().expect("stdout is a string");
    assert!(
        bounded.len() < printed.len(),
        "the model view stays bounded"
    );
}

/// A capture that stopped the command and then failed to publish still says
/// the command was stopped. The blocks are missing, so the bounded result is
/// all the model gets; claiming it is the whole output would be the same lie
/// the stop marker exists to prevent.
#[test]
fn test_failed_publish_keeps_stop() {
    struct FailingCapture;

    impl RawOutputReceiver for FailingCapture {
        fn receive(&self, _: CapturedStream, _: &[u8]) -> CaptureVerdict {
            CaptureVerdict::Continue
        }
    }

    impl ResultCapture for FailingCapture {
        fn stop(&self) -> Option<CaptureStop> {
            Some(CaptureStop {
                reason: CaptureStopReason::ByteBudget,
                stdout_bytes: 4096,
                stderr_bytes: 0,
            })
        }
        fn finish(&self, _: CallEnd) -> PFut<'_, Result<Option<CapturedStreams>, CaptureError>> {
            Box::pin(async { Err(CaptureError::StreamWrite("the store is full".into())) })
        }
    }

    struct FailingSource;

    impl ResultCaptureSource for FailingSource {
        fn begin(&self, _: SessionId, _: &str, _: &str) -> Option<Arc<dyn ResultCapture>> {
            Some(Arc::new(FailingCapture))
        }
    }

    let tool = BashTool::new(Arc::new(LargeOutputSession::new(payload())));
    let ctx = ToolCtx::new("call-1")
        .with_session(SessionId::new())
        .with_result_capture(Arc::new(FailingSource));
    let output = pollster::block_on(tool.execute(ctx, json!({"command": "print"})))
        .expect("a publication failure does not fail the call");
    assert_eq!(output["raw_capture"]["stop_reason"], "byte_budget");
    let stderr = output["stderr"].as_str().unwrap_or_default();
    assert!(
        stderr.contains("the capture budget was reached"),
        "the notice survives the failed publish: {stderr}"
    );
    let stdout = output["stdout"].as_str().expect("stdout is a string");
    assert!(
        stdout.contains(CaptureStopReason::ByteBudget.to_string().as_str()),
        "the stub names the stop instead of promising the full output: {stdout}"
    );
}

/// A capture that reaches its budget ends the command, and the result has to
/// say so. Without the notice the bounded view reads as a command that
/// finished, and the stub would promise output the file does not hold.
#[test]
fn test_budget_stop_marks_result() {
    let log: Arc<dyn SessionLog> = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let tool = BashTool::new(Arc::new(LargeOutputSession::new(payload())));
    let ctx = ToolCtx::new("call-1")
        .with_session(SessionId::new())
        .with_result_capture(Arc::new(SessionResultCapture::with_budget(log, 4096)));
    let output = pollster::block_on(tool.execute(ctx, json!({"command": "print"})))
        .expect("the command answers");
    assert_eq!(output["raw_capture"]["stop_reason"], "byte_budget");
    let stderr = output["stderr"].as_str().unwrap_or_default();
    assert!(
        stderr.contains("the capture budget was reached"),
        "the result says the command was stopped: {stderr}"
    );
    let stdout = output["stdout"].as_str().expect("stdout is a string");
    assert!(
        stdout.contains("the command was stopped"),
        "the stub does not claim the full output was saved: {stdout}"
    );
}

/// A command stopped at the capture budget keeps only the prefix it printed.
/// The note names the stop and the streams that were kept, so the model
/// neither goes looking for bytes that were never kept nor is pointed at a
/// stream that holds nothing.
#[test]
fn test_kept_note_names_stop() {
    let root = env::temp_dir().join(format!("houyi-spill-{}", SessionId::new()));
    let printed = payload();
    let log: Arc<dyn SessionLog> = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let tool = BashTool::new(Arc::new(LargeOutputSession::spilling(printed, &root)));
    let ctx = ToolCtx::new("call-1")
        .with_session(SessionId::new())
        .with_result_capture(Arc::new(SessionResultCapture::with_budget(log, 4096)));
    let output = pollster::block_on(tool.execute(ctx, json!({"command": "print"})))
        .expect("the command answers");
    let stdout = output["stdout"].as_str().expect("stdout is a string");
    assert!(
        stdout.contains("the command was stopped") && stdout.contains("only the prefix"),
        "the note names the stop: {stdout}"
    );
    assert!(
        stdout.contains("call-1:stdout"),
        "the note names the stream that was kept: {stdout}"
    );
    assert!(
        !stdout.contains("call-1:stderr"),
        "a stream with nothing in it is not offered: {stdout}"
    );
    assert!(
        !stdout.contains(&root.display().to_string()),
        "no workspace copy is offered when the capture holds the bytes: {stdout}"
    );
    assert!(
        !root.join(".houyicoder").exists(),
        "nothing is written to the workspace"
    );
    fs::remove_dir_all(&root).ok();
}

/// A command the wall budget stopped is recorded as truncated. The capture
/// itself stopped nothing, so without the end the caller reports the record
/// would claim the kept prefix was the whole output.
#[tokio::test]
async fn test_timeout_record_marks_truncated() {
    struct TimeoutSession;

    impl SandboxSession for TimeoutSession {
        fn exec_with_config(
            &self,
            _command: &str,
            _config: ExecConfig,
        ) -> PFut<'_, Result<ExecResult, SandboxError>> {
            Box::pin(async {
                Err(SandboxError::Unsupported(
                    "this session only answers captured exec".into(),
                ))
            })
        }
        fn workspace_root(&self) -> Arc<Path> {
            Arc::from(Path::new("/nonexistent"))
        }
        fn exec_streaming_captured(
            &self,
            _command: &str,
            _lines: Arc<AtomicI64>,
            _config: ExecConfig,
            receiver: Option<Arc<dyn RawOutputReceiver>>,
        ) -> PFut<'_, Result<ExecResult, SandboxError>> {
            Box::pin(async move {
                let printed = payload();
                if let Some(receiver) = receiver {
                    for chunk in printed.chunks(4096) {
                        receiver.receive(CapturedStream::Stdout, chunk);
                    }
                }
                Err(SandboxError::Timeout {
                    message: "wall-clock 1s exceeded".to_string(),
                    stdout: String::from_utf8(printed).expect("the test prints text"),
                    stderr: String::new(),
                })
            })
        }
    }

    let log: Arc<dyn SessionLog> = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let tool = BashTool::new(Arc::new(TimeoutSession));
    let ctx = ToolCtx::new("call-1")
        .with_session(SessionId::new())
        .with_result_capture(Arc::new(SessionResultCapture::new(log.clone())));
    let output = pollster::block_on(tool.execute(ctx, json!({"command": "print"})))
        .expect("a budget stop is a result, not a tool error");
    assert_eq!(output["timed_out"], true);
    assert_eq!(output["raw_capture"]["stop_reason"], "wall_budget");
    let record_hash = output["raw_capture"]["record"]["block_ref"]
        .as_str()
        .expect("the result names the record block");
    let record = pollster::block_on(log.backend().block_get(&BlockHash(record_hash.to_string())))
        .expect("the record is readable");
    let record: serde_json::Value = serde_json::from_slice(&record).expect("the record is JSON");
    assert_eq!(record["completeness"], "truncated");
    assert_eq!(record["stop_reason"], "wall_budget");
    assert!(record["exit"].is_null(), "nothing exited");
}
