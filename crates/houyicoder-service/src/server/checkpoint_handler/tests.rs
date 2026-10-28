//! The sandbox verbs answer from what the host measured: the fence the sandbox
//! port reports, every write the journal recorded, and the bytes a restore put
//! back. Each test reads the reply the client would receive.

use std::env::temp_dir;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::id;
use std::sync::Arc;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use futures::StreamExt;
use futures::channel::mpsc;
use houyicoder_api::sandbox::FenceStatus;
use houyicoder_api::sandbox::SandboxSession;
use houyicoder_async::PFut;
use houyicoder_context::ContextBackend;
use houyicoder_context::ExecConfig;
use houyicoder_context::ExecResult;
use houyicoder_context::SandboxError;
use houyicoder_context::SessionId;
use houyicoder_context::session_class::{FILE_TRANSACTION_DIR, session_dir};
use houyicoder_core::agent::Runner;
use houyicoder_core::agent::ToolRegistry;
use houyicoder_core::agent::runner_config::RunnerConfig;
use houyicoder_core::snapshot::file_transaction::FileTransaction;
use houyicoder_memory::InMemoryBackend;
use houyicoder_memory::LocalFileBackend;
use houyicoder_permission::DefaultModeGate;
use houyicoder_protocol::envelope::RequestEnvelope;
use houyicoder_protocol::envelope::RequestId;
use houyicoder_protocol::envelope::ResponsePayload;
use houyicoder_protocol::envelope::ServerFrame;
use houyicoder_protocol::error::ErrorCategory;
use houyicoder_protocol::frontend::FrontendRequest;
use houyicoder_protocol::frontend::checkpoint::CheckpointState;
use houyicoder_protocol::frontend::checkpoint::FenceState;
use houyicoder_protocol::frontend::checkpoint::RestoreView;
use houyicoder_provider::FakeProvider;
use houyicoder_session::SessionStore;

use crate::server::Server;
use crate::server::frame_carrier::FrameCarrier;

/// The workspace path the tests write, relative to the scratch root.
const TARGET: &str = "notes.txt";
/// What the target holds before the recorded write.
const BEFORE: &str = "the earlier text\n";
/// What the recorded write leaves.
const AFTER: &str = "the later text\n";

/// A workspace whose bytes live in a scratch directory. The journal records
/// and restores through it, so a test reads the same files the reply reports.
struct ScratchSandbox {
    root: PathBuf,
}

impl SandboxSession for ScratchSandbox {
    fn exec_with_config(
        &self,
        _command: &str,
        _config: ExecConfig,
    ) -> PFut<'_, Result<ExecResult, SandboxError>> {
        Box::pin(async move {
            Err(SandboxError::Unsupported(
                "no execution in this stub".into(),
            ))
        })
    }
    fn workspace_root(&self) -> Arc<Path> {
        Arc::from(self.root.clone())
    }
    fn fence_status(&self) -> FenceStatus {
        FenceStatus::Enforced
    }
}

/// A scratch directory for one test, named so two tests running in parallel
/// never share it. The sandbox resolves paths against a canonical root, so the
/// directory is canonical too.
fn scratch(name: &str) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let dir = temp_dir().join(format!("houyi-sandbox-{name}-{}-{stamp}", id()));
    fs::create_dir_all(&dir).expect("scratch directory");
    dunce::canonicalize(&dir).expect("canonical scratch directory")
}

/// One workspace holding the earlier bytes, the store the journal records
/// into, and the session the host serves.
struct Scenario {
    workspace: PathBuf,
    sessions: PathBuf,
    session: SessionId,
}

impl Scenario {
    fn new(name: &str) -> Self {
        let root = scratch(name);
        let workspace = root.join("workspace");
        let sessions = root.join("sessions");
        fs::create_dir_all(&workspace).expect("workspace");
        fs::write(workspace.join(TARGET), BEFORE).expect("the earlier bytes");
        Self {
            workspace,
            sessions,
            session: SessionId::new(),
        }
    }

    fn sandbox(&self) -> Arc<dyn SandboxSession> {
        Arc::new(ScratchSandbox {
            root: self.workspace.clone(),
        })
    }

    fn journal(&self) -> Arc<FileTransaction> {
        let backend: Arc<dyn ContextBackend> =
            Arc::new(LocalFileBackend::new(self.sessions.clone()));
        Arc::new(FileTransaction::new(backend))
    }

    /// Record one write over the earlier bytes, so the listing has a write to
    /// judge and a restore has bytes to put back.
    async fn write_over(&self, journal: &FileTransaction, sandbox: &dyn SandboxSession) {
        journal
            .write(
                Some(self.session),
                sandbox,
                TARGET,
                AFTER.as_bytes().to_vec(),
            )
            .await
            .expect("the journal records the write");
    }
}

/// A served host and the client end of its frames.
struct Host {
    server: Server,
    io: FrameCarrier,
    rx: mpsc::Receiver<String>,
    id: u64,
}

impl Host {
    /// A host over the scenario's session. The journal is the one a test
    /// recorded writes through, so the listing reads those writes; None when
    /// the session is meant to have none.
    fn new(scenario: &Scenario, sandbox: bool, journal: Option<Arc<FileTransaction>>) -> Self {
        let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
        let runner = Runner::new(
            store,
            Arc::new(FakeProvider::text("x")),
            ToolRegistry::new(),
            RunnerConfig::default(),
        );
        let mut server = Server::new(
            Arc::new(runner),
            scenario.session,
            Arc::new(DefaultModeGate::new()),
        );
        if sandbox {
            server = server.with_session(scenario.sandbox());
        }
        if let Some(journal) = journal {
            server = server.with_file_transaction(journal);
        }
        let (server_tx, client_rx) = mpsc::channel::<String>(8);
        let (_client_tx, server_rx) = mpsc::channel::<String>(8);
        Self {
            server,
            io: FrameCarrier::new(server_tx, server_rx),
            rx: client_rx,
            id: 0,
        }
    }

    /// Dispatch one request and answer the reply the client would read.
    async fn ask(&mut self, payload: FrontendRequest) -> ResponsePayload {
        self.id += 1;
        let req_id = RequestId(self.id);
        self.server
            .dispatch(&mut self.io, RequestEnvelope::new(req_id, payload))
            .await
            .expect("the host answers");
        loop {
            let line = self.rx.next().await.expect("the host sent a frame");
            let frame: ServerFrame = serde_json::from_str(&line).expect("frame decodes");
            if let ServerFrame::Response(envelope) = frame
                && envelope.req_id == req_id
            {
                return envelope.payload;
            }
        }
    }
}

/// The boundaries answer names the fence the port reports and the root it
/// resolves paths against.
#[tokio::test]
async fn test_boundaries_report_the_root() {
    let scenario = Scenario::new("boundaries");
    let mut host = Host::new(&scenario, true, None);
    let reply = host.ask(FrontendRequest::SandboxBoundaries).await;
    let ResponsePayload::SandboxBoundaries(view) = reply else {
        panic!("expected the boundaries, got {reply:?}");
    };
    assert_eq!(view.fence, FenceState::Enforced);
    assert_eq!(view.workspace_root, scenario.workspace.to_string_lossy());
    assert_eq!(
        view.writable_roots,
        vec![scenario.workspace.to_string_lossy().to_string()]
    );
}

/// The listing reports the write the session recorded, with the state the
/// target holds now and both images the record kept.
#[tokio::test]
async fn test_checkpoints_list_recorded_writes() {
    let scenario = Scenario::new("checkpoints");
    let sandbox = scenario.sandbox();
    let journal = scenario.journal();
    scenario.write_over(&journal, sandbox.as_ref()).await;
    let mut host = Host::new(&scenario, true, Some(journal));
    let reply = host.ask(FrontendRequest::SandboxCheckpoints).await;
    let ResponsePayload::SandboxCheckpoints(list) = reply else {
        panic!("expected the checkpoints, got {reply:?}");
    };
    assert!(
        list.unreadable.is_empty(),
        "unreadable: {:?}",
        list.unreadable
    );
    assert_eq!(list.checkpoints.len(), 1);
    let checkpoint = &list.checkpoints[0];
    assert_eq!(checkpoint.target, TARGET);
    assert_eq!(checkpoint.state, CheckpointState::Applied);
    assert!(checkpoint.before.present);
    assert_eq!(checkpoint.after.bytes, Some(AFTER.len() as u64));
}

/// A restore names a listed write and leaves the bytes that write replaced on
/// disk, so the reply's outcome is something the user can check themselves.
#[tokio::test]
async fn test_restore_returns_earlier_bytes() {
    let scenario = Scenario::new("restore");
    let sandbox = scenario.sandbox();
    let journal = scenario.journal();
    scenario.write_over(&journal, sandbox.as_ref()).await;
    let mut host = Host::new(&scenario, true, Some(journal));
    let reply = host.ask(FrontendRequest::SandboxCheckpoints).await;
    let ResponsePayload::SandboxCheckpoints(list) = reply else {
        panic!("expected the checkpoints, got {reply:?}");
    };
    let operation = list.checkpoints[0].operation.clone();
    let reply = host
        .ask(FrontendRequest::SandboxRestore { operation })
        .await;
    let ResponsePayload::SandboxRestored(restored) = reply else {
        panic!("expected the restore, got {reply:?}");
    };
    assert_eq!(restored.outcome, RestoreView::Restored);
    let now = fs::read_to_string(scenario.workspace.join(TARGET)).expect("the target reads back");
    assert_eq!(now, BEFORE);
}

/// A session whose store keeps no journal says so, so the pane states the
/// missing part instead of an empty list.
#[tokio::test]
async fn test_refusal_names_journal() {
    let scenario = Scenario::new("no-journal");
    let mut host = Host::new(&scenario, true, None);
    let reply = host.ask(FrontendRequest::SandboxCheckpoints).await;
    let ResponsePayload::Error(error) = reply else {
        panic!("expected a refusal, got {reply:?}");
    };
    assert_eq!(error.category, ErrorCategory::InvalidRequest);
    assert!(error.message.contains("write journal"), "{}", error.message);
}

/// A session with no sandbox has no fence to describe and no root to resolve
/// a restore against, so both verbs refuse instead of answering an empty view.
#[tokio::test]
async fn test_refusal_names_sandbox() {
    let scenario = Scenario::new("no-sandbox");
    let mut host = Host::new(&scenario, false, Some(scenario.journal()));
    let reply = host.ask(FrontendRequest::SandboxBoundaries).await;
    let ResponsePayload::Error(error) = reply else {
        panic!("expected a refusal, got {reply:?}");
    };
    assert_eq!(error.category, ErrorCategory::InvalidRequest);
    assert!(error.message.contains("sandbox"), "{}", error.message);
    let reply = host
        .ask(FrontendRequest::SandboxRestore {
            operation: "x".into(),
        })
        .await;
    let ResponsePayload::Error(error) = reply else {
        panic!("expected a refusal, got {reply:?}");
    };
    assert!(error.message.contains("sandbox"), "{}", error.message);
}

/// A journal whose record directory cannot be read answers with the failure
/// rather than reporting that the session wrote nothing.
#[tokio::test]
async fn test_failure_reports_the_cause() {
    let scenario = Scenario::new("damaged");
    let sandbox = scenario.sandbox();
    let journal = scenario.journal();
    scenario.write_over(&journal, sandbox.as_ref()).await;
    // Replace the record directory with a file: the listing now fails at the
    // first entry instead of finding no writes.
    let records = session_dir(&scenario.sessions, scenario.session).join(FILE_TRANSACTION_DIR);
    fs::remove_dir_all(&records).expect("the record directory");
    fs::write(&records, b"not a directory").expect("a file where the records were");
    let mut host = Host::new(&scenario, true, Some(journal));
    let reply = host.ask(FrontendRequest::SandboxCheckpoints).await;
    let ResponsePayload::Error(error) = reply else {
        panic!("expected a failure, got {reply:?}");
    };
    assert_eq!(error.category, ErrorCategory::Internal);
}
