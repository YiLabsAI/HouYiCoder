//! Tests for the file write journal: both sides of a write are stored before
//! the target is touched, every refusal leaves the target alone, and a write
//! that lands without its committed record is not reported as a failure.

use std::env::temp_dir;
use std::fs::{create_dir_all, read_dir, read_to_string, remove_dir_all, write};
use std::io::Error as IoError;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::id;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use houyicoder_api::sandbox::SandboxSession;
use houyicoder_async::PFut;
use houyicoder_context::session_class::FILE_TRANSACTION_DIR;
use houyicoder_context::{
    BlockHash, ContextBackend, ContextError, ExecConfig, ExecResult, SandboxError, SessionId,
};
use houyicoder_memory::LocalFileBackend;
use serde_json::Value;

use super::record::{COMMITTED_FILE, PREPARED_FILE};
use super::{FileTransaction, WriteRefusal};

/// One test's own sessions tree and workspace, removed when the test ends.
struct Scratch {
    root: PathBuf,
    sessions: PathBuf,
    workspace: PathBuf,
}

impl Scratch {
    fn new(name: &str) -> Self {
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

    fn file(&self, name: &str) -> PathBuf {
        self.workspace.join(name)
    }

    fn backend(&self) -> Arc<dyn ContextBackend> {
        Arc::new(LocalFileBackend::new(self.sessions.clone()))
    }

    fn stub(&self) -> Arc<Stub> {
        Arc::new(Stub::new(&self.workspace, &self.sessions))
    }

    fn transaction(&self) -> FileTransaction {
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
struct Observed {
    prepared: Arc<AtomicUsize>,
    committed: Arc<AtomicUsize>,
    wrote: Arc<AtomicBool>,
}

/// A sandbox over the scratch workspace. It keeps the trait's path rules and
/// file operations, and records the record directory as the write begins: the
/// journal's whole promise is that the prepared record is on disk first.
struct Stub {
    workspace: Arc<Path>,
    sessions: PathBuf,
    observed: Observed,
    refuse_write: bool,
}

impl Stub {
    fn new(workspace: &Path, sessions: &Path) -> Self {
        Self {
            workspace: Arc::from(workspace),
            sessions: sessions.to_path_buf(),
            observed: Observed::default(),
            refuse_write: false,
        }
    }

    fn refusing(workspace: &Path, sessions: &Path) -> Self {
        Self {
            refuse_write: true,
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
        Box::pin(async move {
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
fn count_records(root: &Path, name: &str) -> usize {
    find_record(root, name).len()
}

fn find_record(root: &Path, name: &str) -> Vec<PathBuf> {
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
fn record(root: &Path, name: &str) -> Value {
    let found = find_record(root, name);
    assert_eq!(found.len(), 1, "exactly one {name} under the sessions root");
    let body = read_to_string(&found[0]).expect("read the record");
    serde_json::from_str(&body).expect("parse the record")
}

/// The bytes a record's block name holds.
async fn stored(backend: &Arc<dyn ContextBackend>, image: &Value) -> Vec<u8> {
    let hash = image["block_ref"].as_str().expect("a stored block");
    backend
        .block_get(&BlockHash(hash.to_string()))
        .await
        .expect("read the stored block")
}

#[tokio::test]
async fn test_write_records_both_sides() {
    let scratch = Scratch::new("both-sides");
    let backend = scratch.backend();
    let stub = scratch.stub();
    write(scratch.file("a.txt"), "old").expect("seed the target");

    scratch
        .transaction()
        .write(
            Some(SessionId::new()),
            stub.as_ref(),
            "a.txt",
            b"new".to_vec(),
        )
        .await
        .expect("write");

    assert_eq!(read_to_string(scratch.file("a.txt")).expect("read"), "new");
    let prepared = record(&scratch.sessions, PREPARED_FILE);
    assert_eq!(prepared["schema"], 1);
    assert_eq!(prepared["target"], "a.txt");
    assert_eq!(prepared["before"]["present"], true);
    assert_eq!(prepared["before"]["bytes"], 3);
    assert_eq!(stored(&backend, &prepared["before"]).await, b"old");
    assert_eq!(stored(&backend, &prepared["expected_after"]).await, b"new");

    let committed = record(&scratch.sessions, COMMITTED_FILE);
    assert_eq!(committed["target"], "a.txt");
    assert_eq!(committed["after"]["bytes"], 3);
    assert_eq!(stored(&backend, &committed["after"]).await, b"new");
}

#[tokio::test]
async fn test_prepared_record_precedes_write() {
    let scratch = Scratch::new("prepared-first");
    let stub = scratch.stub();
    write(scratch.file("a.txt"), "old").expect("seed the target");

    scratch
        .transaction()
        .write(
            Some(SessionId::new()),
            stub.as_ref(),
            "a.txt",
            b"new".to_vec(),
        )
        .await
        .expect("write");

    assert!(stub.observed.wrote.load(Ordering::SeqCst), "the write ran");
    assert_eq!(
        stub.observed.prepared.load(Ordering::SeqCst),
        1,
        "the prepared record was on disk when the write ran"
    );
    assert_eq!(
        stub.observed.committed.load(Ordering::SeqCst),
        0,
        "the committed record follows the write"
    );
}

#[tokio::test]
async fn test_new_file_records_absent() {
    let scratch = Scratch::new("new-file");
    let stub = scratch.stub();

    scratch
        .transaction()
        .write(
            Some(SessionId::new()),
            stub.as_ref(),
            "fresh.txt",
            b"hello".to_vec(),
        )
        .await
        .expect("write");

    let prepared = record(&scratch.sessions, PREPARED_FILE);
    assert_eq!(prepared["before"]["present"], false);
    assert!(
        prepared["before"].get("block_ref").is_none(),
        "no bytes recorded"
    );
    assert_eq!(
        read_to_string(scratch.file("fresh.txt")).expect("read"),
        "hello"
    );
}

#[tokio::test]
async fn test_oversized_before_kept_unbacked() {
    let scratch = Scratch::new("oversized");
    let stub = scratch.stub();
    let old = "x".repeat(20);
    write(scratch.file("big.txt"), &old).expect("seed the target");

    scratch
        .transaction()
        .with_backup_ceiling(10)
        .write(
            Some(SessionId::new()),
            stub.as_ref(),
            "big.txt",
            b"new".to_vec(),
        )
        .await
        .expect("write");

    let before = &record(&scratch.sessions, PREPARED_FILE)["before"];
    assert_eq!(before["present"], true, "the file was there");
    assert_eq!(before["bytes"], 20);
    assert!(
        before.get("block_ref").is_none(),
        "the record states the previous bytes were not kept"
    );
    assert_eq!(
        read_to_string(scratch.file("big.txt")).expect("read"),
        "new"
    );
}

#[tokio::test]
async fn test_unreadable_target_refused() {
    let scratch = Scratch::new("unreadable");
    let stub = scratch.stub();
    create_dir_all(scratch.file("folder")).expect("a directory in the way");

    let refusal = scratch
        .transaction()
        .write(
            Some(SessionId::new()),
            stub.as_ref(),
            "folder",
            b"new".to_vec(),
        )
        .await
        .expect_err("a directory cannot be backed up");

    assert!(
        matches!(refusal, WriteRefusal::Unreadable { .. }),
        "got {refusal}"
    );
    assert!(
        !stub.observed.wrote.load(Ordering::SeqCst),
        "the target was not touched"
    );
    assert!(
        find_record(&scratch.sessions, PREPARED_FILE).is_empty(),
        "a refused write leaves no record"
    );
}

#[tokio::test]
async fn test_escape_refused_before_record() {
    let scratch = Scratch::new("escape");
    let stub = scratch.stub();

    let refusal = scratch
        .transaction()
        .write(
            Some(SessionId::new()),
            stub.as_ref(),
            "../outside.txt",
            b"new".to_vec(),
        )
        .await
        .expect_err("a path outside the workspace is refused");

    assert!(
        matches!(refusal, WriteRefusal::Target { .. }),
        "got {refusal}"
    );
    assert!(
        !stub.observed.wrote.load(Ordering::SeqCst),
        "the target was not touched"
    );
}

#[tokio::test]
async fn test_record_failure_refuses_write() {
    let scratch = Scratch::new("no-record");
    let stub = scratch.stub();
    write(scratch.file("a.txt"), "old").expect("seed the target");
    let session = SessionId::new();
    let blocked = scratch
        .sessions
        .join(session.to_string())
        .join(FILE_TRANSACTION_DIR);
    create_dir_all(blocked.parent().expect("session directory")).expect("session directory");
    write(&blocked, "not a directory").expect("block the record directory");

    let refusal = scratch
        .transaction()
        .write(Some(session), stub.as_ref(), "a.txt", b"new".to_vec())
        .await
        .expect_err("a write without a record is refused");

    assert!(
        matches!(refusal, WriteRefusal::Record { .. }),
        "got {refusal}"
    );
    assert!(
        !stub.observed.wrote.load(Ordering::SeqCst),
        "the target was not touched"
    );
    assert_eq!(read_to_string(scratch.file("a.txt")).expect("read"), "old");
}

#[tokio::test]
async fn test_write_failure_keeps_prepared() {
    let scratch = Scratch::new("write-failure");
    let stub = Arc::new(Stub::refusing(&scratch.workspace, &scratch.sessions));

    let refusal = scratch
        .transaction()
        .write(
            Some(SessionId::new()),
            stub.as_ref(),
            "a.txt",
            b"new".to_vec(),
        )
        .await
        .expect_err("the write itself fails");

    assert!(
        matches!(refusal, WriteRefusal::Write { .. }),
        "got {refusal}"
    );
    assert_eq!(
        find_record(&scratch.sessions, PREPARED_FILE).len(),
        1,
        "the prepared record stays for a recovery to read"
    );
    assert!(
        find_record(&scratch.sessions, COMMITTED_FILE).is_empty(),
        "nothing was committed"
    );
}

#[tokio::test]
async fn test_no_session_passes_through() {
    let scratch = Scratch::new("no-session");
    let stub = scratch.stub();

    scratch
        .transaction()
        .write(None, stub.as_ref(), "a.txt", b"new".to_vec())
        .await
        .expect("write");

    assert_eq!(read_to_string(scratch.file("a.txt")).expect("read"), "new");
    assert!(
        find_record(&scratch.sessions, PREPARED_FILE).is_empty(),
        "there is no session to record under"
    );
}

#[test]
fn test_refusal_names_its_cause() {
    let cases = [
        (
            WriteRefusal::Target {
                path: "a.txt".into(),
                error: SandboxError::PathTraversal("escapes".into()),
            },
            "resolve the write target a.txt",
        ),
        (
            WriteRefusal::Unreadable {
                path: PathBuf::from("a.txt"),
                error: IoError::new(ErrorKind::PermissionDenied, "denied"),
            },
            "read the previous a.txt",
        ),
        (
            WriteRefusal::Backup {
                path: "a.txt".into(),
                error: ContextError::Io,
            },
            "keep the previous bytes of a.txt",
        ),
        (
            WriteRefusal::Record {
                path: PathBuf::from("prepared.json"),
                reason: "no space".into(),
            },
            "write the record prepared.json",
        ),
        (
            WriteRefusal::Write {
                path: "a.txt".into(),
                error: SandboxError::Io("full".into()),
            },
            "write a.txt",
        ),
    ];
    for (refusal, expected) in cases {
        assert!(
            refusal.to_string().starts_with(expected),
            "{refusal} names what failed"
        );
    }
}
