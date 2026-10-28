//! The pieces the journal tests share: a scratch store and workspace, a
//! sandbox over them that records what the record directory held as a write
//! began, and readers for the records on disk.

use std::env::temp_dir;
use std::fs::{create_dir_all, read_dir, read_to_string, remove_dir_all, write};
use std::path::{Path, PathBuf};
use std::process::id;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use houyicoder_api::sandbox::SandboxSession;
use houyicoder_async::PFut;
use houyicoder_context::session_class::{FILE_TRANSACTION_DIR, session_dir};
use houyicoder_context::{
    BlockHash, ContextBackend, ExecConfig, ExecResult, SandboxError, SessionId,
};
use houyicoder_memory::LocalFileBackend;
use serde_json::Value;
use tokio::task::yield_now;

use super::FileTransaction;
use super::record::{COMMITTED_FILE, PREPARED_FILE};

/// One test's own sessions tree and workspace, removed when the test ends.
pub(super) struct Scratch {
    root: PathBuf,
    pub(super) sessions: PathBuf,
    pub(super) workspace: PathBuf,
}

impl Scratch {
    pub(super) fn new(name: &str) -> Self {
        let root = temp_dir().join(format!("file-transaction-{name}-{}", id()));
        remove_dir_all(&root).ok();
        create_dir_all(root.join("sessions")).expect("sessions directory");
        create_dir_all(root.join("workspace")).expect("workspace directory");
        let workspace = dunce::canonicalize(root.join("workspace")).expect("canonical workspace");
        Self {
            sessions: root.join("sessions"),
            root,
            workspace,
        }
    }

    pub(super) fn file(&self, name: &str) -> PathBuf {
        self.workspace.join(name)
    }

    pub(super) fn backend(&self) -> Arc<dyn ContextBackend> {
        Arc::new(LocalFileBackend::new(self.sessions.clone()))
    }

    pub(super) fn stub(&self) -> Arc<Stub> {
        Arc::new(Stub::new(&self.workspace, &self.sessions))
    }

    pub(super) fn transaction(&self) -> FileTransaction {
        FileTransaction::new(self.backend())
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        remove_dir_all(&self.root).ok();
    }
}

/// What the record directory held at the moment the write ran.
#[derive(Clone, Default)]
pub(super) struct Observed {
    pub(super) prepared: Arc<AtomicUsize>,
    pub(super) committed: Arc<AtomicUsize>,
    pub(super) wrote: Arc<AtomicBool>,
}

/// A sandbox over the scratch workspace. It keeps the trait's path rules and
/// file operations, and records the record directory as the write begins: the
/// journal's whole promise is that the prepared record is on disk first.
pub(super) struct Stub {
    workspace: Arc<Path>,
    sessions: PathBuf,
    pub(super) observed: Observed,
    refuse_write: bool,
    yields: bool,
}

impl Stub {
    pub(super) fn new(workspace: &Path, sessions: &Path) -> Self {
        Self {
            workspace: Arc::from(workspace),
            sessions: sessions.to_path_buf(),
            observed: Observed::default(),
            refuse_write: false,
            yields: false,
        }
    }

    pub(super) fn refusing(workspace: &Path, sessions: &Path) -> Self {
        Self {
            refuse_write: true,
            ..Self::new(workspace, sessions)
        }
    }

    /// A sandbox whose write lets another task run first, so two writes that
    /// are not ordered by a lock overlap on purpose.
    pub(super) fn yielding(workspace: &Path, sessions: &Path) -> Self {
        Self {
            yields: true,
            ..Self::new(workspace, sessions)
        }
    }
}

impl SandboxSession for Stub {
    fn exec_with_config(
        &self,
        _command: &str,
        _config: ExecConfig,
    ) -> PFut<'_, Result<ExecResult, SandboxError>> {
        Box::pin(async { Err(SandboxError::Unsupported("no execution".into())) })
    }

    fn write_file(&self, path: &str, content: Vec<u8>) -> PFut<'_, Result<(), SandboxError>> {
        self.observed.prepared.store(
            count_records(&self.sessions, PREPARED_FILE),
            Ordering::SeqCst,
        );
        self.observed.committed.store(
            count_records(&self.sessions, COMMITTED_FILE),
            Ordering::SeqCst,
        );
        self.observed.wrote.store(true, Ordering::SeqCst);
        let target = self.resolve_write(path);
        let refuse = self.refuse_write;
        let yields = self.yields;
        Box::pin(async move {
            if yields {
                yield_now().await;
            }
            if refuse {
                return Err(SandboxError::Io("no space left".into()));
            }
            let target = target?;
            if let Some(parent) = target.parent() {
                create_dir_all(parent)?;
            }
            write(&target, content)?;
            Ok(())
        })
    }

    fn workspace_root(&self) -> Arc<Path> {
        self.workspace.clone()
    }
}

/// How many record files of one name are on disk under the sessions root.
pub(super) fn count_records(root: &Path, name: &str) -> usize {
    find_record(root, name).len()
}

pub(super) fn find_record(root: &Path, name: &str) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(sessions) = read_dir(root) else {
        return found;
    };
    for session in sessions.flatten() {
        let Ok(operations) = read_dir(session.path().join(FILE_TRANSACTION_DIR)) else {
            continue;
        };
        for operation in operations.flatten() {
            let candidate = operation.path().join(name);
            if candidate.is_file() {
                found.push(candidate);
            }
        }
    }
    found
}

/// The one record of this name, parsed.
pub(super) fn record(root: &Path, name: &str) -> Value {
    let found = find_record(root, name);
    assert_eq!(found.len(), 1, "exactly one {name} under the sessions root");
    let body = read_to_string(&found[0]).expect("read the record");
    serde_json::from_str(&body).expect("parse the record")
}

/// The bytes a record's block name holds.
pub(super) async fn stored(backend: &Arc<dyn ContextBackend>, image: &Value) -> Vec<u8> {
    let hash = image["block_ref"].as_str().expect("a stored block");
    backend
        .block_get(&BlockHash(hash.to_string()))
        .await
        .expect("read the stored block")
}

/// The operation name the one record of this kind sits under.
pub(super) fn operation_of(root: &Path, name: &str) -> String {
    let found = find_record(root, name);
    assert_eq!(found.len(), 1, "exactly one {name} under the sessions root");
    found[0]
        .parent()
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .expect("the write name is text")
        .to_string()
}

/// A session id for a test that needs one.
pub(super) fn session() -> SessionId {
    SessionId::new()
}

/// The directory holding the recorded writes of one session.
pub(super) fn transactions_of(sessions: &Path, session: SessionId) -> PathBuf {
    session_dir(sessions, session).join(FILE_TRANSACTION_DIR)
}

/// Record one write of a.txt over an earlier version, and hand back the
/// sandbox it ran through.
pub(super) async fn seed_write(scratch: &Scratch, session: SessionId) -> Arc<Stub> {
    let stub = scratch.stub();
    write(scratch.file("a.txt"), "old").expect("seed the target");
    scratch
        .transaction()
        .write(Some(session), stub.as_ref(), "a.txt", b"new".to_vec())
        .await
        .expect("write");
    stub
}

/// Rewrite the one prepared record in place, so a test can present a record
/// the write path would not produce. Returns the operation it lives under.
pub(super) fn edit_prepared(root: &Path, edit: impl FnOnce(&mut Value)) -> String {
    let found = find_record(root, PREPARED_FILE);
    assert_eq!(found.len(), 1, "exactly one prepared record to edit");
    let path = found[0].clone();
    let body = read_to_string(&path).expect("read the record");
    let mut value: Value = serde_json::from_str(&body).expect("parse the record");
    edit(&mut value);
    write(
        &path,
        serde_json::to_vec(&value).expect("encode the record"),
    )
    .expect("write the record");
    operation_of(root, PREPARED_FILE)
}
