//! Tests for this module.

use super::super::output::{BASH_MAX_OUTPUT_CHARS, bound_bash_output, tail_chars};
use super::super::*;
use houyicoder_api::capture::CaptureStopReason;
use std::path::Path;

#[test]
fn test_bound_under_cap_passes() {
    let (o, e) = bound_bash_output("hello".into(), "world".into(), None, &[]);
    assert_eq!(o, "hello");
    assert_eq!(e, "world");
}

/// Over the cap with no captured stream, the model gets a head+tail stub that
/// says the rest was not kept. No workspace file is named, so the model is not
/// pointed at a path it would need another command to read.
#[test]
fn test_over_cap_without_capture() {
    let big = "a".repeat(BASH_MAX_OUTPUT_CHARS + 1);
    let (o, e) = bound_bash_output(big.clone(), String::new(), None, &[]);
    assert!(
        o.contains("Output truncated"),
        "truncation marker missing: {o}"
    );
    assert!(
        o.contains("not kept"),
        "the stub says the rest was not kept: {o}"
    );
    assert!(
        !o.contains("saved to") && !o.contains('/'),
        "no workspace file path is named: {o}"
    );
    assert_eq!(e, "", "stderr is empty when nothing was kept");
    assert!(
        o.len() < big.len() / 2,
        "the stub is far smaller than the original"
    );
}

/// A kept capture is the copy the model reads back, so the truncation names
/// both streams and leaves no file behind in the workspace.
#[test]
fn test_over_cap_names_kept() {
    let big = "a".repeat(BASH_MAX_OUTPUT_CHARS + 1);
    let (o, e) = bound_bash_output(
        big.clone(),
        String::new(),
        None,
        &["call-1:stdout".to_string(), "call-1:stderr".to_string()],
    );
    assert!(o.contains("call-1:stdout"), "names the stdout stream: {o}");
    assert!(o.contains("call-1:stderr"), "names the stderr stream: {o}");
    assert!(
        !o.contains("saved to") && !o.contains('/'),
        "a kept capture needs no workspace file: {o}"
    );
    assert_eq!(e, "");
}

/// A command stopped at its budget keeps only what it printed before the
/// stop, and the note says so instead of promising the whole output.
#[test]
fn test_bound_stopped_says_prefix() {
    let big = "a".repeat(BASH_MAX_OUTPUT_CHARS + 1);
    let (o, _) = bound_bash_output(
        big,
        String::new(),
        Some(CaptureStopReason::WallBudget),
        &["call-1".to_string()],
    );
    assert!(
        o.contains("the command was stopped") && o.contains("only the prefix"),
        "a stopped command keeps a prefix: {o}"
    );
}

#[test]
fn test_tail_chars_no_newline() {
    let big = "a".repeat(10_000);
    assert_eq!(tail_chars(&big, 100).len(), 100);
    assert_eq!(tail_chars(&big, 20_000).len(), 10_000);
    assert_eq!(tail_chars(&big, 100).chars().next(), Some('a'));
}

use std::sync::atomic::{AtomicUsize, Ordering};

use houyicoder_context::{ExecConfig, ExecResult, SandboxError};
use std::env::temp_dir;

struct DiscoverStub {
    services: Vec<String>,
    scans: Arc<AtomicUsize>,
}

impl DiscoverStub {
    fn new(services: Vec<String>) -> (Self, Arc<AtomicUsize>) {
        let scans = Arc::new(AtomicUsize::new(0));
        (
            Self {
                services,
                scans: Arc::clone(&scans),
            },
            scans,
        )
    }
}

impl SandboxSession for DiscoverStub {
    fn exec_with_config(
        &self,
        _c: &str,
        _cfg: ExecConfig,
    ) -> PFut<'_, Result<ExecResult, SandboxError>> {
        Box::pin(async {
            Ok(ExecResult {
                stdout: String::new(),
                stderr: "Operation not permitted".to_string(),
                exit_code: Some(1),
            })
        })
    }
    fn workspace_root(&self) -> Arc<Path> {
        Arc::from(temp_dir())
    }
    fn discover_authorizable(&self) -> Vec<String> {
        self.scans.fetch_add(1, Ordering::Relaxed);
        self.services.clone()
    }
}

#[tokio::test]
async fn test_discover_on_denial() {
    let (s, _scans) = DiscoverStub::new(vec!["com.citrolabs.x".into()]);
    assert!(s.workspace_root().to_path_buf().exists());
    let session: Arc<dyn SandboxSession> = Arc::new(s);
    let r = discover_authorizable_if_failed(false, "Operation not permitted", session).await;
    assert_eq!(r, vec!["com.citrolabs.x".to_string()]);
}

#[tokio::test]
async fn test_discover_skips_plain_fail() {
    let (stub, _scans) = DiscoverStub::new(vec!["com.citrolabs.x".into()]);
    let session: Arc<dyn SandboxSession> = Arc::new(stub);
    let r = discover_authorizable_if_failed(false, "command not found", session).await;
    assert!(r.is_empty());
}

#[tokio::test]
async fn test_discover_skips_success() {
    let (stub, _scans) = DiscoverStub::new(vec!["com.citrolabs.x".into()]);
    let session: Arc<dyn SandboxSession> = Arc::new(stub);
    let r = discover_authorizable_if_failed(true, "", session).await;
    assert!(r.is_empty());
}

#[tokio::test]
async fn test_no_skill_skips_scan() {
    let (stub, scans) = DiscoverStub::new(vec!["com.citrolabs.x".into()]);
    let tool = BashTool::new(Arc::new(stub));
    let output = tool
        .execute(ToolCtx::new("test"), json!({"command": "fail"}))
        .await
        .expect("bash result");
    assert_eq!(scans.load(Ordering::Relaxed), 0);
    assert!(output.get("authorizable_services").is_none());
}

#[tokio::test]
async fn test_active_skill_scans() {
    let (stub, scans) = DiscoverStub::new(vec!["com.citrolabs.x".into()]);
    let active = Arc::new(Mutex::new(Some("ego-browser".to_string())));
    let tool = BashTool::new(Arc::new(stub)).with_active_skill(Some(active));
    let output = tool
        .execute(ToolCtx::new("test"), json!({"command": "fail"}))
        .await
        .expect("bash result");
    assert_eq!(scans.load(Ordering::Relaxed), 1);
    assert_eq!(output["authorizable_services"][0], "com.citrolabs.x");
}

/// The tool declares no way to run outside the fence. Its inputs are this
/// property list, so the assertion is an exact set: any field added later
/// that could turn the sandbox off fails here rather than shipping as an
/// escape hatch the model can choose.
#[test]
fn test_schema_no_sandbox_escape() {
    let tool = BashTool::new(Arc::new(DiscoverStub {
        services: Vec::new(),
        scans: Arc::new(AtomicUsize::new(0)),
    }));
    let schema = tool.input_schema();
    let properties = schema["properties"].as_object().expect("property map");
    let names: Vec<&str> = properties.keys().map(String::as_str).collect();
    assert_eq!(names, ["command", "timeout_ms"], "unexpected input fields");
    let text = schema.to_string().to_ascii_lowercase();
    assert!(
        !text.contains("sandbox") && !text.contains("dangerously"),
        "no input field may disable the fence: {text}"
    );
}

/// The tool text names the scratch variable and says the shared temp root is
/// refused. A model that never reads it keeps writing to /tmp and loses the
/// command to the fence.
#[test]
fn test_description_names_tmpdir() {
    let tool = BashTool::new(Arc::new(DiscoverStub {
        services: Vec::new(),
        scans: Arc::new(AtomicUsize::new(0)),
    }));
    let text = tool.description();
    assert!(text.contains("$TMPDIR"), "names the scratch var: {text}");
    assert!(text.contains("/tmp"), "names the refused path: {text}");
}
