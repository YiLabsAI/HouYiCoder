//! Cross-tool unit test: every file-touch tool activates paths-gated
//! skills by intent before the file op. Lives at the tools module level
//! because it spans Read, Write, Edit, and MultiEdit together.

use std::sync::Arc;

use houyicoder_api::sandbox::SandboxSession;
use houyicoder_api::tool::{Tool, ToolCtx};
use houyicoder_async::PFut;
use houyicoder_context::{ExecConfig, ExecResult, SandboxError};
use serde_json::json;

use super::{EditTool, MultiEditTool, ReadTool, WriteTool};
use crate::agent::conditional_activation::ConditionalSkillActivator;

/// A stub session that satisfies read/write without touching disk.
struct StubSession;
impl SandboxSession for StubSession {
    fn exec_with_config(
        &self,
        _command: &str,
        _config: ExecConfig,
    ) -> PFut<'_, Result<ExecResult, SandboxError>> {
        Box::pin(async { Err(SandboxError::Unsupported("test".into())) })
    }
    fn read_file(&self, _path: &str, _max_bytes: usize) -> PFut<'_, Result<Vec<u8>, SandboxError>> {
        Box::pin(async { Ok(Vec::new()) })
    }
    fn write_file(&self, _path: &str, _content: Vec<u8>) -> PFut<'_, Result<(), SandboxError>> {
        Box::pin(async { Ok(()) })
    }
    fn workspace_root(&self) -> Arc<std::path::Path> {
        Arc::from(std::path::PathBuf::from("/"))
    }
}

/// An activator that records the paths it is asked to activate on.
struct CountingActivator {
    calls: std::sync::Mutex<Vec<String>>,
}
impl ConditionalSkillActivator for CountingActivator {
    fn activate_for_paths(&self, file_paths: &[String]) -> Vec<String> {
        self.calls
            .lock()
            .unwrap()
            .extend(file_paths.iter().cloned());
        Vec::new()
    }
    fn is_active(&self, _name: &str) -> bool {
        false
    }
}

/// Every file-touch tool activates paths-gated skills by intent, before
/// the file op. The edit/multiedit calls error on the empty stub read
/// (no match), but activation has already run.
#[tokio::test]
async fn test_file_tools_activate() {
    let session: Arc<dyn SandboxSession> = Arc::new(StubSession);
    let activator = Arc::new(CountingActivator {
        calls: std::sync::Mutex::new(Vec::new()),
    });
    let a = activator.clone();
    let ctx = ToolCtx::new("c");
    ReadTool::new(session.clone())
        .with_activator(Some(a.clone()))
        .execute(ctx.clone(), json!({"path":"src/a.rs"}))
        .await
        .unwrap();
    WriteTool::new(session.clone())
        .with_activator(Some(a.clone()))
        .execute(ctx.clone(), json!({"path":"src/b.rs","content":"x"}))
        .await
        .unwrap();
    EditTool::new(session.clone())
        .with_activator(Some(a.clone()))
        .execute(
            ctx.clone(),
            json!({"path":"src/c.rs","old_string":"x","new_string":"y"}),
        )
        .await
        .ok();
    MultiEditTool::new(session)
        .with_activator(Some(a))
        .execute(
            ctx,
            json!({"path":"src/d.rs","edits":[{"old_string":"x","new_string":"y"}]}),
        )
        .await
        .ok();
    let calls = activator.calls.lock().unwrap().clone();
    assert_eq!(
        calls,
        vec!["src/a.rs", "src/b.rs", "src/c.rs", "src/d.rs"],
        "each tool activated on its path: {calls:?}"
    );
}

/// A stub session backed by an in-memory HashMap so edit/multiedit can
/// exercise their success paths without a real sandbox.
struct ContentStubSession {
    files: std::sync::Mutex<std::collections::HashMap<String, Vec<u8>>>,
}

impl ContentStubSession {
    fn new() -> Self {
        Self {
            files: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }
    fn with(path: &str, content: &[u8]) -> Self {
        let s = Self::new();
        s.files
            .lock()
            .unwrap()
            .insert(path.to_string(), content.to_vec());
        s
    }
}

impl SandboxSession for ContentStubSession {
    fn exec_with_config(
        &self,
        _command: &str,
        _config: ExecConfig,
    ) -> PFut<'_, Result<ExecResult, SandboxError>> {
        Box::pin(async { Err(SandboxError::Unsupported("test".into())) })
    }
    fn read_file(&self, path: &str, max_bytes: usize) -> PFut<'_, Result<Vec<u8>, SandboxError>> {
        let files = self.files.lock().unwrap();
        let content = files.get(path).cloned().unwrap_or_default();
        Box::pin(async move { Ok(content.into_iter().take(max_bytes).collect()) })
    }
    fn write_file(&self, path: &str, content: Vec<u8>) -> PFut<'_, Result<(), SandboxError>> {
        self.files.lock().unwrap().insert(path.to_string(), content);
        Box::pin(async { Ok(()) })
    }
    fn workspace_root(&self) -> Arc<std::path::Path> {
        Arc::from(std::path::PathBuf::from("/"))
    }
}

#[tokio::test]
async fn test_read_returns_content() {
    let session = Arc::new(ContentStubSession::with("a.rs", b"fn main() {}"));
    let r = ReadTool::new(session)
        .execute(ToolCtx::new("c"), json!({"path":"a.rs"}))
        .await
        .unwrap();
    assert_eq!(r["content"], "fn main() {}");
    assert_eq!(r["truncated"], false);
}

#[tokio::test]
async fn test_read_errors_missing_path() {
    let session: Arc<dyn SandboxSession> = Arc::new(ContentStubSession::new());
    let err = ReadTool::new(session)
        .execute(ToolCtx::new("c"), json!({}))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("path"));
}

#[tokio::test]
async fn test_read_zero_max_bytes() {
    let session = Arc::new(ContentStubSession::with("a.rs", b"data"));
    let err = ReadTool::new(session)
        .execute(ToolCtx::new("c"), json!({"path":"a.rs","max_bytes":0}))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("max_bytes"));
}

#[tokio::test]
async fn test_write_creates_file() {
    let session: Arc<dyn SandboxSession> = Arc::new(ContentStubSession::new());
    let r = WriteTool::new(session.clone())
        .execute(
            ToolCtx::new("c"),
            json!({"path":"new.rs","content":"hello"}),
        )
        .await
        .unwrap();
    assert_eq!(r["bytes"], 5);
    assert_eq!(r["lines"], 1);
}

#[tokio::test]
async fn test_write_skips_when_unchanged() {
    let session = Arc::new(ContentStubSession::with("a.rs", b"hello"));
    let r = WriteTool::new(session)
        .execute(
            ToolCtx::new("c"),
            json!({"path":"a.rs","content":"hello","write_if_unchanged":true}),
        )
        .await
        .unwrap();
    assert_eq!(r["unchanged"], true);
}

#[tokio::test]
async fn test_write_errors_missing_content() {
    let session: Arc<dyn SandboxSession> = Arc::new(ContentStubSession::new());
    let err = WriteTool::new(session)
        .execute(ToolCtx::new("c"), json!({"path":"a.rs"}))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("content"));
}

#[tokio::test]
async fn test_edit_replace_diff() {
    let session = Arc::new(ContentStubSession::with("a.rs", b"fn foo() { 1 }"));
    let r = EditTool::new(session.clone())
        .execute(
            ToolCtx::new("c"),
            json!({"path":"a.rs","old_string":"1","new_string":"2"}),
        )
        .await
        .unwrap();
    assert_eq!(r["occurrences_replaced"], 1);
    let diff = r["diff"].as_str().unwrap();
    assert!(
        diff.contains("-fn foo() { 1 }"),
        "diff should show old line: {diff}"
    );
    assert!(
        diff.contains("+fn foo() { 2 }"),
        "diff should show new line: {diff}"
    );
    let files = session.files.lock().unwrap();
    assert_eq!(files.get("a.rs").unwrap(), b"fn foo() { 2 }");
}

#[tokio::test]
async fn test_edit_errors_not_found() {
    let session = Arc::new(ContentStubSession::with("a.rs", b"fn foo() {}"));
    let err = EditTool::new(session)
        .execute(
            ToolCtx::new("c"),
            json!({"path":"a.rs","old_string":"xyz","new_string":"abc"}),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("not found"));
}

#[tokio::test]
async fn test_edit_errors_missing_path() {
    let session: Arc<dyn SandboxSession> = Arc::new(ContentStubSession::new());
    let err = EditTool::new(session)
        .execute(
            ToolCtx::new("c"),
            json!({"old_string":"x","new_string":"y"}),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("path"));
}

#[tokio::test]
async fn test_multiedit_applies_batch() {
    let session = Arc::new(ContentStubSession::with(
        "a.rs",
        b"fn foo() { 1 }\nfn bar() { 3 }\n",
    ));
    let r = MultiEditTool::new(session.clone())
        .execute(
            ToolCtx::new("c"),
            json!({"path":"a.rs","edits":[
                {"old_string":"1","new_string":"2"},
                {"old_string":"3","new_string":"4"}
            ]}),
        )
        .await
        .unwrap();
    assert_eq!(r["edits_applied"], 2);
    let files = session.files.lock().unwrap();
    assert_eq!(
        files.get("a.rs").unwrap(),
        b"fn foo() { 2 }\nfn bar() { 4 }\n"
    );
}

#[tokio::test]
async fn test_multiedit_errors_empty_edits() {
    let session: Arc<dyn SandboxSession> = Arc::new(ContentStubSession::with("a.rs", b"data"));
    let err = MultiEditTool::new(session)
        .execute(ToolCtx::new("c"), json!({"path":"a.rs","edits":[]}))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("non-empty"));
}

#[tokio::test]
async fn test_multiedit_errors_missing_path() {
    let session: Arc<dyn SandboxSession> = Arc::new(ContentStubSession::new());
    let err = MultiEditTool::new(session)
        .execute(
            ToolCtx::new("c"),
            json!({"edits":[{"old_string":"x","new_string":"y"}]}),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("path"));
}
