//! Shared helpers for the service integration-test binaries.
//!
//! Binaries that also act as a child process run harness-free through
//! run_tests, and the ACP processes they speak to live in acp_agent. The
//! MCP server the tool tests drive is its own module beside this one,
//! included by path from the binary that owns it.
//! Each tests-binary does mod common; + imports the frame plumbing so the
//! wire-contract tests share ONE copy of the stub-runner + channel pair +
//! send/recv helpers instead of duplicating them per binary. Cargo treats
//! tests/common/mod.rs as a helper module (NOT a separate test binary),
//! so it compiles into each binary that includes it.
//!
//! Not every helper is used by every binary (server_contract has its own
//! stub_runner with a specific canned reply; permission_contract uses the
//! generic one). A module-level dead-code allow keeps the unused-in-some-
//! binary helpers from gating the build.

#![allow(dead_code)] // test fixtures; used by some test binaries, unused from others

use std::env::current_exe;
use std::env::temp_dir;
use std::fs::copy;
use std::fs::create_dir_all;
use std::fs::read_dir;
use std::fs::remove_dir_all;
use std::panic::catch_unwind;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::exit;
use std::process::id;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use futures::SinkExt;
use futures::StreamExt;
use futures::channel::mpsc;
use houyicoder_api::provider::ModelProvider;
use houyicoder_api::sandbox::{SandboxSession, WorktreeFenceGuard};
use houyicoder_async::PFut;
use houyicoder_context::{DirEntry, ExecConfig, ExecResult, SandboxError, SessionId};
use houyicoder_core::agent::runner_config::RunnerConfig;
use houyicoder_core::agent::{Runner, ToolRegistry};
use houyicoder_memory::InMemoryBackend;
use houyicoder_protocol::envelope::ServerFrame;
use houyicoder_protocol::framing::encode;
use houyicoder_protocol::handshake::Hello;
use houyicoder_provider::FakeProvider;
use houyicoder_service::server::FrameCarrier;
use houyicoder_session::SessionStore;
use std::time::Duration;
use tokio::runtime::Runtime;
use tokio::time::timeout;

/// A minimal runner over a stub provider so a run completes in one turn (or
/// never drives the runner, for permission verbs that do not).
pub fn stub_runner() -> (Arc<Runner>, SessionId) {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let session = SessionId::new();
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::text("test"));
    let runner = Runner::new(
        store,
        provider,
        ToolRegistry::new(),
        RunnerConfig::default(),
    );
    (Arc::new(runner), session)
}

/// The duplex channel pair a test client + the server share.
pub fn pair() -> (FrameCarrier, mpsc::Sender<String>, mpsc::Receiver<String>) {
    let (client_tx, server_rx) = mpsc::channel::<String>(8);
    let (server_tx, client_rx) = mpsc::channel::<String>(8);
    (
        FrameCarrier::new(server_tx, server_rx),
        client_tx,
        client_rx,
    )
}

/// Pull the next raw frame line the server sent, trailing newline stripped.
pub async fn recv_line(rx: &mut mpsc::Receiver<String>) -> String {
    let line = rx.next().await.expect("server sent a frame");
    line.strip_suffix('\n').unwrap_or(&line).to_string()
}

/// Decode the next server frame (post-handshake) as a typed ServerFrame.
pub async fn recv_frame(rx: &mut mpsc::Receiver<String>) -> ServerFrame {
    serde_json::from_str(&recv_line(rx).await).expect("frame decodes as a server frame")
}

/// recv_frame wrapped in a timeout so a drain loop that would hang (the
/// server never ships the expected frame, but the channel stays open — the
/// same shape as a session-mismatch hang) fails as a clear assertion
/// instead of a CI timeout. Panics with the hang cause on timeout.
pub async fn recv_frame_within(rx: &mut mpsc::Receiver<String>, dur: Duration) -> ServerFrame {
    match timeout(dur, recv_frame(rx)).await {
        Ok(frame) => frame,
        Err(_) => panic!(
            "timed out after {:?} waiting for a server frame; the server hung \
             (a concurrent-control frame was not handled — check session-id \
             match + the serve select! path)",
            dur
        ),
    }
}

/// Decode the next server frame as a typed Hello (handshake only).
pub async fn recv_hello(rx: &mut mpsc::Receiver<String>) -> Hello {
    serde_json::from_str(&recv_line(rx).await).expect("frame decodes as hello")
}

/// Send a typed frame from the client to the server, newline-terminated.
pub async fn send_frame(tx: &mut mpsc::Sender<String>, msg: &impl serde::Serialize) {
    let mut frame = encode(msg).expect("encode");
    if !frame.ends_with('\n') {
        frame.push('\n');
    }
    tx.send(frame).await.expect("client sends frame");
}

/// A sandbox whose fence always succeeds. The worktree controller only calls
/// narrow_to_worktree and active_exec_count; the rest exist so the trait is
/// satisfied.
pub struct StubSandbox;

impl SandboxSession for StubSandbox {
    fn exec_with_config(
        &self,
        _command: &str,
        _config: ExecConfig,
    ) -> PFut<'_, Result<ExecResult, SandboxError>> {
        Box::pin(async move {
            Ok(ExecResult {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: Some(0),
            })
        })
    }
    fn read_file(&self, _path: &str, _max: usize) -> PFut<'_, Result<Vec<u8>, SandboxError>> {
        Box::pin(async move { Ok(Vec::new()) })
    }
    fn write_file(&self, _path: &str, _content: Vec<u8>) -> PFut<'_, Result<(), SandboxError>> {
        Box::pin(async move { Ok(()) })
    }
    fn list_dir(&self, _path: &str) -> PFut<'_, Result<Vec<DirEntry>, SandboxError>> {
        Box::pin(async move { Ok(Vec::new()) })
    }
    fn path_exists(&self, _path: &str) -> PFut<'_, Result<bool, SandboxError>> {
        Box::pin(async move { Ok(false) })
    }
    fn workspace_root(&self) -> Arc<Path> {
        Arc::from(PathBuf::from("/"))
    }
    fn narrow_to_worktree(
        &self,
        _worktree: &Path,
        _git_common_dir: &Path,
    ) -> Result<WorktreeFenceGuard, SandboxError> {
        Ok(WorktreeFenceGuard::new(Box::new(|| Ok(()))))
    }
    fn active_exec_count(&self) -> usize {
        0
    }
}

/// A throwaway git repository with one empty commit, so a linked worktree has
/// a HEAD to branch from.
///
/// One template lives for the test binary's lifetime and each test takes a
/// file copy, because process creation is the expensive part of the fixture
/// and five tests would otherwise cost fifteen spawns.
pub fn make_git_repo(tag: &str) -> PathBuf {
    static TEMPLATE: OnceLock<PathBuf> = OnceLock::new();
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let template = TEMPLATE.get_or_init(|| {
        let dir = temp_dir().join(format!("houyi-service-wt-template-{}", id()));
        drop(remove_dir_all(&dir));
        create_dir_all(&dir).expect("create the template dir");
        // No template directory of its own: a developer's global git template
        // can carry hooks and symlinks that would run in every fixture.
        for args in [
            ["-c", "init.templateDir=", "init", "-q"].as_slice(),
            ["config", "user.email", "t@t"].as_slice(),
            ["config", "user.name", "t"].as_slice(),
            ["commit", "--allow-empty", "-m", "init", "-q"].as_slice(),
        ] {
            git(&dir, args);
        }
        dir
    });
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = temp_dir().join(format!("houyi-service-wt-{tag}-{}-{n}", id()));
    drop(remove_dir_all(&dir));
    copy_tree(template, &dir);
    dir
}

/// Copy a directory tree. The template holds only small files, so a plain
/// recursive copy is faster than any spawn and needs no extra dependency.
fn copy_tree(from: &Path, to: &Path) {
    create_dir_all(to).expect("create the copy dir");
    for entry in read_dir(from).expect("read the template") {
        let entry = entry.expect("template entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("template file type").is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            copy(entry.path(), &target).expect("copy a template file");
        }
    }
}

/// Run one git command against the template repository. This prepares a
/// directory; it is not an engine spawn.
#[expect(
    clippy::disallowed_methods,
    reason = "fixture setup, not a model-driven spawn"
)]
fn git(repo: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("git runs");
    assert!(status.success(), "git {args:?} succeeds");
}

/// Run this binary's tests without a test harness. A child process spawned
/// from current_exe must never emit harness banners on stdout, because that
/// stdout is the protocol stream; so these binaries run their own list and
/// report failures the way a harness would.
pub fn run_tests(tests: &[(&str, fn())]) {
    let harness = HarnessArgs::parse();
    if harness.list {
        for (name, _) in tests {
            println!("{name}: test");
        }
        return;
    }
    let selected: Vec<(&str, fn())> = tests
        .iter()
        .filter(|(name, _)| harness.wants(name))
        .copied()
        .collect();
    let tests = selected.as_slice();
    let mut failed = 0;
    for (name, test) in tests {
        match catch_unwind(test) {
            Ok(()) => println!("test {name} ... ok"),
            Err(_) => {
                failed += 1;
                println!("test {name} ... FAILED");
            }
        }
    }
    println!("{} passed, {failed} failed", tests.len() - failed);
    if failed > 0 {
        exit(1);
    }
}

/// The command that re-runs this binary in one child mode. The test binary
/// is the fixture, not an engine spawn.
#[expect(
    clippy::disallowed_methods,
    reason = "fixture setup, not a model-driven spawn"
)]
pub fn child_command(mode_env: &str) -> Command {
    let mut cmd = Command::new(current_exe().expect("the test binary path"));
    cmd.env(mode_env, "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    cmd
}

/// A boxed test future, so a hand-run list can name async tests.
pub type BoxTestFuture = Pin<Box<dyn std::future::Future<Output = ()> + Send>>;
/// One named async test in a hand-run list.
pub type BoxedTest = (&'static str, fn() -> BoxTestFuture);

/// Run this binary's async tests without a test harness. A child process
/// spawned from current_exe must never emit harness banners on stdout,
/// because that stdout is the protocol stream.
pub fn run_async_tests(runtime: &Runtime, tests: &[BoxedTest]) {
    let harness = HarnessArgs::parse();
    if harness.list {
        for (name, _) in tests {
            println!("{name}: test");
        }
        return;
    }
    let selected: Vec<BoxedTest> = tests
        .iter()
        .filter(|(name, _)| harness.wants(name))
        .copied()
        .collect();
    let tests = selected.as_slice();
    let mut failed = 0;
    for (name, test) in tests {
        match catch_unwind(|| runtime.block_on(test())) {
            Ok(()) => println!("test {name} ... ok"),
            Err(_) => {
                failed += 1;
                println!("test {name} ... FAILED");
            }
        }
    }
    println!("{} passed, {failed} failed", tests.len() - failed);
    if failed > 0 {
        exit(1);
    }
}

/// The subset of the harness protocol a runner needs from a harness-free
/// binary: a list pass that names the tests, and an exact filter that runs
/// one of them.
struct HarnessArgs {
    list: bool,
    exact: Option<String>,
}

impl HarnessArgs {
    fn parse() -> Self {
        let args: Vec<String> = std::env::args().collect();
        let list = args.iter().any(|arg| arg == "--list");
        let exact = args
            .iter()
            .position(|arg| arg == "--exact")
            .and_then(|at| args.get(at + 1))
            .cloned();
        Self { list, exact }
    }

    fn wants(&self, name: &str) -> bool {
        self.exact.as_deref().is_none_or(|wanted| wanted == name)
    }
}
