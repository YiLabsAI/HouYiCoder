//! The /worktrees verbs across the connection: the client names a directory,
//! the host answers in the pane's own terms, and none of the three starts a
//! model turn.
#![expect(
    clippy::disallowed_methods,
    reason = "test setup spawn, not model-driven"
)]

mod common;

use std::env::temp_dir;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;
use std::process::id;
use std::sync::Arc;

use futures::channel::mpsc;
use houyicoder_api::provider::ModelProvider;
use houyicoder_api::sandbox::SandboxSession;
use houyicoder_api::sandbox::WorktreeFenceGuard;
use houyicoder_api::session::SessionLog;
use houyicoder_async::PFut;
use houyicoder_context::DirEntry;
use houyicoder_context::ExecConfig;
use houyicoder_context::ExecResult;
use houyicoder_context::SandboxError;
use houyicoder_context::SessionId;
use houyicoder_core::agent::Runner;
use houyicoder_core::agent::ToolRegistry;
use houyicoder_core::agent::runner_config::RunnerConfig;
use houyicoder_core::agent::worktree::session::get_or_create_worktree;
use houyicoder_core::agent::worktree_controller::WorktreeController;
use houyicoder_memory::InMemoryBackend;
use houyicoder_permission::DefaultModeGate;
use houyicoder_protocol::envelope::ClientFrame;
use houyicoder_protocol::envelope::RequestEnvelope;
use houyicoder_protocol::envelope::RequestId;
use houyicoder_protocol::envelope::ResponsePayload;
use houyicoder_protocol::envelope::ServerFrame;
use houyicoder_protocol::error::ProtocolError;
use houyicoder_protocol::frontend::FrontendRequest;
use houyicoder_protocol::frontend::worktree::WorktreeRemoveOutcome;
use houyicoder_protocol::handshake::Hello;
use houyicoder_provider::FakeProvider;
use houyicoder_service::server::Server;
use houyicoder_session::SessionStore;
use tokio::task::JoinHandle;

use crate::common::{pair, recv_frame, recv_hello, send_frame};

/// A sandbox whose fence always succeeds. The controller only calls
/// narrow_to_worktree and active_exec_count; the rest exist to satisfy the
/// trait.
struct StubSandbox;

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

/// A throwaway repository with one empty commit, so a linked worktree has a
/// HEAD to branch from.
fn make_repo(tag: &str) -> PathBuf {
    let dir = temp_dir().join(format!("houyi-service-wt-{}-{tag}", id()));
    drop(fs::remove_dir_all(&dir));
    fs::create_dir_all(&dir).expect("mkdir the repository");
    let run = |args: &[&str]| {
        Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("git runs");
    };
    run(&["init", "-q"]);
    run(&[
        "-c",
        "user.email=t@t",
        "-c",
        "user.name=t",
        "commit",
        "--allow-empty",
        "-q",
        "-m",
        "init",
    ]);
    dir
}

/// A host sharing one repository with the controller it answers through, or a
/// host with no repository at all. The provider holds no response, so a model
/// call would panic: a verb that answered by starting a turn cannot pass.
fn build_server(repo: Option<&Path>) -> Server {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let log: Arc<dyn SessionLog> = store.clone();
    let mut runner = Runner::new(
        store,
        Arc::new(FakeProvider::new(Vec::new())) as Arc<dyn ModelProvider>,
        ToolRegistry::new(),
        RunnerConfig::default(),
    );
    if let Some(repo) = repo {
        let controller = Arc::new(WorktreeController::new(
            repo.to_path_buf(),
            repo.join(".git"),
            Arc::new(StubSandbox),
            log,
            SessionId::new(),
        ));
        runner = runner.with_worktree_controller(Some(controller));
    }
    Server::new(
        Arc::new(runner),
        SessionId::new(),
        Arc::new(DefaultModeGate::new()),
    )
}

/// Serve one host to one client with the handshake done, so a test sends its
/// request and reads the answer.
async fn open(
    repo: Option<&Path>,
) -> (
    mpsc::Sender<String>,
    mpsc::Receiver<String>,
    JoinHandle<Result<(), ProtocolError>>,
) {
    let (server_io, mut tx, mut rx) = pair();
    let server = build_server(repo);
    let handle = tokio::spawn(async move { server.serve(server_io).await });
    send_frame(&mut tx, &Hello::local()).await;
    let _ = recv_hello(&mut rx).await;
    (tx, rx, handle)
}

/// Send a request and answer with its reply, skipping any event frames the
/// host sent first.
async fn ask(
    tx: &mut mpsc::Sender<String>,
    rx: &mut mpsc::Receiver<String>,
    id: RequestId,
    payload: FrontendRequest,
) -> ResponsePayload {
    send_frame(tx, &ClientFrame::Request(RequestEnvelope::new(id, payload))).await;
    loop {
        if let ServerFrame::Response(envelope) = recv_frame(rx).await
            && envelope.req_id == id
        {
            return envelope.payload;
        }
    }
}

/// Previewing and removing answer in the pane's own terms, and the removal
/// takes the tree apart.
#[tokio::test]
async fn test_worktree_removal_round_trip() {
    let repo = make_repo("round-trip");
    let tree = get_or_create_worktree(&repo, "feat-a").expect("create a managed worktree");
    let tree_path = tree.worktree_path.to_string_lossy().into_owned();
    let (mut tx, mut rx, handle) = open(Some(&repo)).await;

    let preview = ask(
        &mut tx,
        &mut rx,
        RequestId(2),
        FrontendRequest::WorktreePreview {
            path: tree_path.clone(),
        },
    )
    .await;
    let token = match preview {
        ResponsePayload::WorktreePreview(view) => {
            assert_eq!(view.name, "feat-a");
            assert_eq!(view.path, tree_path);
            assert!(!view.token.is_empty(), "a removal needs a token to name");
            view.token
        }
        other => panic!("expected a preview answer, got {other:?}"),
    };

    let removed = ask(
        &mut tx,
        &mut rx,
        RequestId(3),
        FrontendRequest::WorktreeRemove {
            path: tree_path,
            token,
            delete_unmerged_branch: false,
        },
    )
    .await;
    match removed {
        ResponsePayload::WorktreeRemoved(outcome) => {
            assert_eq!(outcome, WorktreeRemoveOutcome::Removed);
        }
        other => panic!("expected a removal answer, got {other:?}"),
    }
    assert!(
        !tree.worktree_path.exists(),
        "the removal took the tree apart"
    );
    // Closing the client end is what ends the serve loop this test started.
    drop(tx);
    drop(handle.await);
    fs::remove_dir_all(&repo).expect("drop the repository");
}

/// Entering answers with the tree the session now works in.
#[tokio::test]
async fn test_enter_answer_names_tree() {
    let repo = make_repo("enter");
    let tree = get_or_create_worktree(&repo, "feat-b").expect("create a managed worktree");
    let tree_path = tree.worktree_path.to_string_lossy().into_owned();
    let (mut tx, mut rx, handle) = open(Some(&repo)).await;

    let answer = ask(
        &mut tx,
        &mut rx,
        RequestId(1),
        FrontendRequest::WorktreeEnter {
            path: tree_path.clone(),
        },
    )
    .await;

    match answer {
        ResponsePayload::WorktreeEntered(view) => {
            assert_eq!(view.path, tree_path, "the answer names the tree entered");
            assert_eq!(view.branch, tree.worktree_branch);
        }
        other => panic!("expected an enter answer, got {other:?}"),
    }
    // Closing the client end is what ends the serve loop this test started.
    drop(tx);
    drop(handle.await);
    fs::remove_dir_all(&repo).expect("drop the repository");
}

/// Without a repository the host says why it cannot act, on every verb.
#[tokio::test]
async fn test_requests_refuse_without_repository() {
    let (mut tx, mut rx, handle) = open(None).await;
    let path = "/some/repo".to_string();
    let requests = [
        FrontendRequest::WorktreePreview { path: path.clone() },
        FrontendRequest::WorktreeEnter { path: path.clone() },
        FrontendRequest::WorktreeRemove {
            path,
            token: "token".to_string(),
            delete_unmerged_branch: false,
        },
    ];

    for (index, request) in requests.into_iter().enumerate() {
        let answer = ask(&mut tx, &mut rx, RequestId(index as u64 + 1), request).await;
        match answer {
            ResponsePayload::Error(error) => {
                assert!(
                    error.message.contains("manages no worktrees"),
                    "the refusal names the missing repository: {}",
                    error.message
                );
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }
    // Closing the client end is what ends the serve loop this test started.
    drop(tx);
    drop(handle.await);
}

/// A directory this product did not create is refused, and it survives.
#[tokio::test]
async fn test_preview_refuses_unmanaged() {
    let repo = make_repo("unmanaged");
    let outside = repo.join("hand-made");
    fs::create_dir_all(&outside).expect("a directory outside the managed root");
    let (mut tx, mut rx, handle) = open(Some(&repo)).await;

    let answer = ask(
        &mut tx,
        &mut rx,
        RequestId(1),
        FrontendRequest::WorktreePreview {
            path: outside.to_string_lossy().into_owned(),
        },
    )
    .await;

    match answer {
        ResponsePayload::Error(error) => {
            assert!(!error.message.is_empty(), "the refusal names a reason");
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert!(outside.exists(), "the directory is left alone");
    // Closing the client end is what ends the serve loop this test started.
    drop(tx);
    drop(handle.await);
    fs::remove_dir_all(&repo).expect("drop the repository");
}
