//! The reclaim pass: what it keeps, what it removes, and what stops it.

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;
#[cfg(unix)]
use std::{
    ffi::OsStr,
    os::unix::{ffi::OsStrExt, fs::symlink},
};

use serde_json::Map;
use serde_json::Value;
use serde_json::json;

use houyicoder_context::BlockHash;
use houyicoder_context::CheckpointId;
use houyicoder_context::CheckpointManifest;
use houyicoder_context::ContextBackend;
use houyicoder_context::EventId;
use houyicoder_context::SessionEvent;
use houyicoder_context::SessionId;
use houyicoder_context::SessionLogEntry;

use super::CURSOR_NAME;
use super::CURSOR_SCHEMA;
use super::GC_DIR;
use super::MAX_RECORD;
use super::MarkRun;
use super::RECORD_BATCH;
use super::REFS_DIR;
use super::ReclaimError;
use super::ReclaimPolicy;
use super::ReclaimReport;
use super::ReclaimStop;
use super::ReferenceSource;
use super::Relist;
#[cfg(unix)]
use super::checkpoint_key;
use super::relist;
use super::source_fingerprint;
use crate::local_file::LocalFileBackend;

/// A pass whose lease window is empty, so age never keeps a block.
fn immediate() -> ReclaimPolicy {
    ReclaimPolicy {
        write_lease: Duration::ZERO,
        budget: Duration::from_secs(60),
    }
}

/// Why a pass stopped, for a test that expects it to.
fn stopped_with(outcome: Result<ReclaimReport, ReclaimStop>) -> ReclaimError {
    outcome.expect_err("the pass should have stopped").error
}

fn temp_root() -> PathBuf {
    let dir = env::temp_dir().join(format!("houyicoder_block_gc_{}", SessionId::new()));
    fs::create_dir_all(&dir).expect("create temp root");
    dir
}

fn put(backend: &LocalFileBackend, bytes: &[u8]) -> BlockHash {
    pollster::block_on(backend.block_put(bytes.to_vec())).expect("store block")
}

fn block_path(root: &Path, hash: &BlockHash) -> PathBuf {
    root.join(".cas")
        .join(&hash.0[..2])
        .join(format!("{}.bin", hash.0))
}

/// The block store is the pass's own input, so a test often needs a block
/// that is already older than any lease window: write the age onto it.
fn age(path: &Path, age: Duration) {
    let handle = OpenOptions::new()
        .write(true)
        .open(path)
        .expect("open block to age");
    handle
        .set_modified(SystemTime::now() - age)
        .expect("set block time");
}

fn session_log(root: &Path, session: SessionId) -> PathBuf {
    let dir = root.join(session.to_string());
    fs::create_dir_all(&dir).expect("create session dir");
    dir.join("log.jsonl")
}

fn append_ref(backend: &LocalFileBackend, session: SessionId, hash: &BlockHash) {
    let event = SessionLogEntry {
        id: EventId::new(),
        session,
        ts: 0,
        prev_hash: None,
        event: SessionEvent::ToolResult {
            call_id: "c1".into(),
            output: serde_json::json!({ "block_ref": hash.0, "preview": "..." }),
            duration_ms: 0,
        },
    };
    pollster::block_on(backend.append(event)).expect("append event");
}

/// Leave the cursor an earlier pass would have left: every source up to done
/// filed with the state it holds now, the given bucket sizes, and the sweep
/// position.
fn write_cursor(
    root: &Path,
    done: &str,
    sources: &[ReferenceSource],
    buckets: Value,
    swept: Option<&str>,
) {
    let workspace = root.join(".cas").join(GC_DIR);
    fs::create_dir_all(workspace.join(REFS_DIR)).expect("create workspace");
    let mut filed = Map::new();
    for source in sources
        .iter()
        .filter(|source| source.is_append_only() && source.key() <= done)
    {
        let meta = fs::metadata(source.path()).expect("stat source");
        let modified_ms = meta
            .modified()
            .expect("source mtime")
            .duration_since(UNIX_EPOCH)
            .expect("after the epoch")
            .as_millis();
        filed.insert(
            source.key().to_string(),
            json!({"modified_ms": modified_ms, "len": meta.len()}),
        );
    }
    let cursor = json!({
        "schema": CURSOR_SCHEMA,
        "done": done,
        "fingerprint": source_fingerprint(sources),
        "filed": filed,
        "buckets": buckets,
        "swept": swept,
    });
    fs::write(workspace.join(CURSOR_NAME), cursor.to_string()).expect("write cursor");
}

/// File addresses into the buckets an earlier pass would have left, and
/// return the sizes a cursor over them records.
fn plant_buckets(root: &Path, addresses: &[&BlockHash]) -> Value {
    let workspace = root.join(".cas").join(GC_DIR).join(REFS_DIR);
    fs::create_dir_all(&workspace).expect("create workspace");
    let mut lines: BTreeMap<String, String> = BTreeMap::new();
    for address in addresses {
        lines
            .entry(address.0[..2].to_string())
            .or_default()
            .push_str(&format!("{}\n", address.0));
    }
    let mut sizes = Map::new();
    for (shard, body) in lines {
        let path = workspace.join(&shard);
        let mut filed = fs::read_to_string(&path).unwrap_or_default();
        filed.push_str(&body);
        fs::write(&path, &filed).expect("write the bucket");
        sizes.insert(shard, json!(filed.len() as u64));
    }
    Value::Object(sizes)
}

/// A root that was never written holds no block to free, and a pass over it
/// leaves no store behind: the first maintenance run may come before any
/// session does.
#[test]
fn test_reclaim_skips_missing_root() {
    let parent = temp_root();
    let root = parent.join("absent");
    let backend = LocalFileBackend::new(root.clone());

    let report = backend
        .reclaim_orphan_blocks(&ReclaimPolicy::default())
        .expect("a missing store is not a failure");
    assert_eq!(report.removed, 0);
    assert!(report.complete, "the pass finished over an empty store");
    assert!(!root.exists(), "the pass did not create the store");
    fs::remove_dir_all(&parent).ok();
}

/// A block a session log names survives; a block nothing names does not.
#[test]
fn test_reclaim_keeps_referenced() {
    let root = temp_root();
    let backend = LocalFileBackend::new(root.clone());
    let session = SessionId::new();
    let referenced = put(&backend, b"keep me");
    append_ref(&backend, session, &referenced);
    let orphan = put(&backend, b"reclaim me");
    age(&block_path(&root, &referenced), Duration::from_secs(7200));
    age(&block_path(&root, &orphan), Duration::from_secs(7200));

    let report = backend
        .reclaim_orphan_blocks(&immediate())
        .expect("reclaim");
    assert_eq!(report.removed, 1, "only the unnamed block went");
    assert!(block_path(&root, &referenced).exists(), "named block stays");
    assert!(
        !block_path(&root, &orphan).exists(),
        "unnamed block is gone"
    );
    fs::remove_dir_all(&root).ok();
}

/// A block stored moments ago is kept even with no reference yet: the event
/// that names it may not have been appended, and a pass that deleted it
/// would leave the append pointing at nothing.
#[test]
fn test_reclaim_keeps_fresh_orphan() {
    let root = temp_root();
    let backend = LocalFileBackend::new(root.clone());
    let fresh = put(&backend, b"about to be named");

    let report = backend
        .reclaim_orphan_blocks(&ReclaimPolicy::default())
        .expect("reclaim");
    assert_eq!(report.removed, 0);
    assert_eq!(report.leased, 1, "the young block is held by its lease");
    assert!(block_path(&root, &fresh).exists());
    fs::remove_dir_all(&root).ok();
}

/// Storing bytes the store already holds is a claim on them: the caller is
/// about to append the event that names them. That refresh has to move the
/// block inside the lease window, or a pass running between the put and the
/// append frees bytes the appended event is about to reference.
#[test]
fn test_put_refreshes_lease() {
    let root = temp_root();
    let backend = LocalFileBackend::new(root.clone());
    let held = put(&backend, b"stored earlier");
    let path = block_path(&root, &held);
    age(&path, Duration::from_secs(7200));

    let again = put(&backend, b"stored earlier");
    assert_eq!(again, held, "the same bytes keep their address");

    let report = backend
        .reclaim_orphan_blocks(&ReclaimPolicy::default())
        .expect("reclaim");
    assert_eq!(report.removed, 0);
    assert_eq!(report.leased, 1, "the re-put moved it inside the lease");
    assert!(path.exists(), "the claimed block stays");
    fs::remove_dir_all(&root).ok();
}

/// A write cut off before its rename leaves a temp sibling on disk. No
/// reference can name it, so the pass reaps it and leaves the block the log
/// does name.
#[test]
fn test_reclaim_reaps_stale_tmp() {
    let root = temp_root();
    let backend = LocalFileBackend::new(root.clone());
    let session = SessionId::new();
    let held = put(&backend, b"keep me");
    append_ref(&backend, session, &held);
    let tmp = root
        .join(".cas")
        .join(&held.0[..2])
        .join(format!("{}.bin.tmp", held.0));
    fs::write(&tmp, b"stale temp").expect("write the interrupted temp");

    let report = backend
        .reclaim_orphan_blocks(&immediate())
        .expect("reclaim");
    assert_eq!(report.removed, 1, "the temp file counts as reclaimed");
    assert_eq!(report.blocks, 1, "only the named block is a stored block");
    assert!(!tmp.exists(), "the stale temp is gone");
    assert!(block_path(&root, &held).exists(), "the named block stays");
    fs::remove_dir_all(&root).ok();
}

/// A log line that does not parse and is not the log's last partial write
/// stops the pass: a bucket missing one address would delete a live block.
#[test]
fn test_reclaim_rejects_bad_log() {
    let root = temp_root();
    let backend = LocalFileBackend::new(root.clone());
    let orphan = put(&backend, b"survives a bad scan");
    age(&block_path(&root, &orphan), Duration::from_secs(7200));
    let log = session_log(&root, SessionId::new());
    fs::write(&log, "{\"id\":\"x\"}\nnot an event\n").expect("write log");

    let error = stopped_with(backend.reclaim_orphan_blocks(&immediate()));
    assert!(
        matches!(error, ReclaimError::Unreadable { .. }),
        "a corrupt line stops the pass: {error:?}"
    );
    assert!(block_path(&root, &orphan).exists(), "nothing was deleted");
    fs::remove_dir_all(&root).ok();
}

/// A log carrying one record past the read limit stops the pass rather than
/// being held whole in memory: the pass reads bounded records only.
#[test]
fn test_reclaim_rejects_oversized_record() {
    let root = temp_root();
    let backend = LocalFileBackend::new(root.clone());
    let orphan = put(&backend, b"survives an oversized record");
    age(&block_path(&root, &orphan), Duration::from_secs(7200));
    let log = session_log(&root, SessionId::new());
    fs::write(&log, vec![b'x'; MAX_RECORD as usize + 10]).expect("write the long record");

    let error = stopped_with(backend.reclaim_orphan_blocks(&immediate()));
    assert!(
        matches!(error, ReclaimError::Unreadable { .. }),
        "a record past the limit stops the pass: {error:?}"
    );
    assert!(block_path(&root, &orphan).exists(), "nothing was deleted");
    fs::remove_dir_all(&root).ok();
}

/// A session entry whose path cannot even be resolved is not a session the
/// pass may skip: skipping it would sweep without whatever references it
/// holds.
#[cfg(unix)]
#[test]
fn test_reclaim_rejects_unresolved_entry() {
    let root = temp_root();
    let backend = LocalFileBackend::new(root.clone());
    let orphan = put(&backend, b"survives an unresolved entry");
    age(&block_path(&root, &orphan), Duration::from_secs(7200));
    symlink("loop", root.join("loop")).expect("create the self-referencing entry");

    let error = stopped_with(backend.reclaim_orphan_blocks(&immediate()));
    assert!(
        matches!(error, ReclaimError::Unreadable { .. }),
        "an entry the pass cannot resolve stops it: {error:?}"
    );
    assert!(block_path(&root, &orphan).exists(), "nothing was deleted");
    fs::remove_dir_all(&root).ok();
}

/// A session whose log path is a directory is a store the pass cannot read,
/// so it stops rather than treating the session as holding no references.
#[test]
fn test_reclaim_rejects_log_dir() {
    let root = temp_root();
    let backend = LocalFileBackend::new(root.clone());
    let orphan = put(&backend, b"survives a broken layout");
    age(&block_path(&root, &orphan), Duration::from_secs(7200));
    let log = session_log(&root, SessionId::new());
    fs::create_dir_all(&log).expect("make the log path a directory");

    let error = stopped_with(backend.reclaim_orphan_blocks(&immediate()));
    assert!(matches!(error, ReclaimError::Unreadable { .. }));
    assert!(block_path(&root, &orphan).exists());
    fs::remove_dir_all(&root).ok();
}

/// A store path that exists and is not a directory is a store this pass
/// cannot read, not an absent one: reporting it as an empty finished pass
/// would hide a misconfigured root.
#[test]
fn test_reclaim_rejects_root_file() {
    let parent = temp_root();
    let root = parent.join("root-file");
    fs::write(&root, b"not a store").expect("write the root file");
    let backend = LocalFileBackend::new(root.clone());

    let error = stopped_with(backend.reclaim_orphan_blocks(&immediate()));
    assert!(
        matches!(error, ReclaimError::Unreadable { .. }),
        "a root that is not a directory stops the pass: {error:?}"
    );
    fs::remove_dir_all(&parent).ok();
}

/// A shard name that is a link is not a shard. Following it would let an
/// entry in the store root point the walk at a directory elsewhere, and the
/// pass would delete every block-shaped file there.
#[cfg(unix)]
#[test]
fn test_sweep_ignores_linked_shard() {
    let root = temp_root();
    let backend = LocalFileBackend::new(root.clone());
    let elsewhere = root.join("elsewhere");
    fs::create_dir_all(&elsewhere).expect("create the directory outside the store");
    let stranger = elsewhere.join(format!("{}.bin", "ab".repeat(32)));
    fs::write(&stranger, b"not this store's block").expect("write the outside file");
    age(&stranger, Duration::from_secs(7200));
    let cas = root.join(".cas");
    fs::create_dir_all(&cas).expect("create the store");
    symlink(&elsewhere, cas.join("ab")).expect("link a shard name outside the store");

    let report = backend
        .reclaim_orphan_blocks(&immediate())
        .expect("reclaim");

    assert!(report.complete, "the pass finishes");
    assert_eq!(
        report.blocks, 0,
        "a linked shard holds no block of this store"
    );
    assert!(stranger.exists(), "nothing outside the store was deleted");
    fs::remove_dir_all(&root).ok();
}

/// A stored manifest names event ids rather than block addresses, so the
/// pass reads it and files nothing from it. A manifest it failed to parse
/// would stop the reclaim for the whole store.
#[test]
fn test_reclaim_reads_manifest() {
    let root = temp_root();
    let backend = LocalFileBackend::new(root.clone());
    let session = SessionId::new();
    let held = put(&backend, b"named by the log");
    append_ref(&backend, session, &held);
    age(&block_path(&root, &held), Duration::from_secs(7200));
    let checkpoints = root.join(session.to_string()).join("checkpoints");
    fs::create_dir_all(&checkpoints).expect("create checkpoint dir");
    let manifest = CheckpointManifest {
        id: CheckpointId::new(),
        session,
        last_event: EventId::new(),
        summary: Some("folded the first two turns".into()),
        plan: Vec::new(),
        ts: 0,
    };
    let body = serde_json::to_string(&manifest).expect("serialize the manifest");
    fs::write(checkpoints.join("cp.json"), body).expect("write manifest");

    let report = backend
        .reclaim_orphan_blocks(&immediate())
        .expect("reclaim");
    assert_eq!(report.sources, 2, "the log and the manifest were both read");
    assert_eq!(report.removed, 0, "the log still names its block");
    assert!(block_path(&root, &held).exists());
    fs::remove_dir_all(&root).ok();
}

/// A fork copies the events it inherits into its own log, so the block
/// survives on the child's reference after the parent log is gone.
#[test]
fn test_reclaim_keeps_fork_block() {
    let root = temp_root();
    let backend = LocalFileBackend::new(root.clone());
    let parent = SessionId::new();
    let held = put(&backend, b"inherited by a fork");
    append_ref(&backend, parent, &held);
    age(&block_path(&root, &held), Duration::from_secs(7200));
    let parent_log = root.join(parent.to_string()).join("log.jsonl");
    let line = fs::read_to_string(&parent_log).expect("read parent log");
    let child = session_log(&root, SessionId::new());
    fs::write(&child, line).expect("copy the inherited event");
    fs::remove_dir_all(root.join(parent.to_string())).expect("drop the parent session");

    let report = backend
        .reclaim_orphan_blocks(&immediate())
        .expect("reclaim");
    assert_eq!(report.removed, 0);
    assert!(block_path(&root, &held).exists(), "the fork keeps it");
    fs::remove_dir_all(&root).ok();
}

/// An append cut short leaves a final line without its newline. That event
/// never landed, so it names nothing, and its presence must not stop the
/// pass for every other block in the store.
#[test]
fn test_reclaim_tolerates_torn_tail() {
    let root = temp_root();
    let backend = LocalFileBackend::new(root.clone());
    let session = SessionId::new();
    let held = put(&backend, b"named by a whole line");
    append_ref(&backend, session, &held);
    let orphan = put(&backend, b"collected");
    age(&block_path(&root, &held), Duration::from_secs(7200));
    age(&block_path(&root, &orphan), Duration::from_secs(7200));
    let log = root.join(session.to_string()).join("log.jsonl");
    let mut bytes = fs::read(&log).expect("read log");
    bytes.extend_from_slice(b"{\"id\":\"torn");
    fs::write(&log, bytes).expect("write the torn tail");

    let report = backend
        .reclaim_orphan_blocks(&immediate())
        .expect("reclaim");
    assert_eq!(report.torn_tail, 1, "the partial append is counted");
    assert_eq!(report.removed, 1);
    assert!(block_path(&root, &held).exists());
    assert!(!block_path(&root, &orphan).exists());
    fs::remove_dir_all(&root).ok();
}

/// A final event that lost only its newline is a whole value, so the block it
/// names survives. Reading it as a torn append would lose that address and
/// free a live block.
#[test]
fn test_tail_names_block() {
    let root = temp_root();
    let backend = LocalFileBackend::new(root.clone());
    let session = SessionId::new();
    let held = put(&backend, b"named by the last line");
    append_ref(&backend, session, &held);
    let log = root.join(session.to_string()).join("log.jsonl");
    let bytes = fs::read(&log).expect("read the log");
    assert!(bytes.ends_with(b"\n"), "the append is whole");
    fs::write(&log, &bytes[..bytes.len() - 1]).expect("strip the final newline");
    let orphan = put(&backend, b"collected");

    let report = backend
        .reclaim_orphan_blocks(&immediate())
        .expect("reclaim");
    assert_eq!(report.torn_tail, 0, "the line is whole without its newline");
    assert_eq!(report.removed, 1);
    assert!(
        block_path(&root, &held).exists(),
        "the last line names its block"
    );
    assert!(!block_path(&root, &orphan).exists());
    fs::remove_dir_all(&root).ok();
}

/// An empty line names nothing, so a log carrying one still yields the
/// references around it instead of stopping the pass.
#[test]
fn test_blank_line_skipped() {
    let root = temp_root();
    let backend = LocalFileBackend::new(root.clone());
    let session = SessionId::new();
    let held = put(&backend, b"named around the blank lines");
    append_ref(&backend, session, &held);
    let log = root.join(session.to_string()).join("log.jsonl");
    let mut bytes = fs::read(&log).expect("read the log");
    bytes.extend_from_slice(b"\n\n");
    fs::write(&log, bytes).expect("append the blank lines");
    let orphan = put(&backend, b"collected");

    let report = backend
        .reclaim_orphan_blocks(&immediate())
        .expect("a blank line is not a broken record");
    assert!(report.complete);
    assert_eq!(report.removed, 1);
    assert!(block_path(&root, &held).exists(), "the event still counts");
    assert!(!block_path(&root, &orphan).exists(), "the orphan went");
    fs::remove_dir_all(&root).ok();
}

/// Two passes over one store never run at once: the second skips instead of
/// sweeping with a mark the first is still building.
#[test]
fn test_reclaim_skips_locked() {
    let root = temp_root();
    let backend = LocalFileBackend::new(root.clone());
    let orphan = put(&backend, b"untouched by a skipped pass");
    age(&block_path(&root, &orphan), Duration::from_secs(7200));
    let workspace = root.join(".cas").join(".gc");
    fs::create_dir_all(&workspace).expect("create workspace");
    let holder = File::options()
        .create(true)
        .write(true)
        .truncate(false)
        .open(workspace.join("lock"))
        .expect("open lock");
    holder.try_lock().expect("hold the reclaim lock");

    let error = stopped_with(backend.reclaim_orphan_blocks(&immediate()));
    assert!(matches!(error, ReclaimError::Busy));
    assert!(block_path(&root, &orphan).exists());
    drop(holder);
    fs::remove_dir_all(&root).ok();
}

/// A pass that spends its budget before its first source deletes nothing; the
/// next pass starts the mark from the beginning and finishes the sweep.
#[test]
fn test_reclaim_budget_before_mark() {
    let root = temp_root();
    let backend = LocalFileBackend::new(root.clone());
    let session = SessionId::new();
    let held = put(&backend, b"named");
    append_ref(&backend, session, &held);
    let orphan = put(&backend, b"collected by the second pass");
    age(&block_path(&root, &held), Duration::from_secs(7200));
    age(&block_path(&root, &orphan), Duration::from_secs(7200));

    let spent = ReclaimPolicy {
        write_lease: Duration::ZERO,
        budget: Duration::ZERO,
    };
    let error = stopped_with(backend.reclaim_orphan_blocks(&spent));
    assert!(matches!(error, ReclaimError::Budget));
    assert!(
        block_path(&root, &orphan).exists(),
        "a spent pass deletes nothing"
    );

    let report = backend
        .reclaim_orphan_blocks(&immediate())
        .expect("reclaim");
    assert_eq!(report.removed, 1);
    assert!(block_path(&root, &held).exists());
    fs::remove_dir_all(&root).ok();
}

/// A cursor whose sources still hold the bytes it recorded and whose buckets
/// are on disk is resumed: the pass reads no source again, and it sweeps from
/// the references the earlier mark filed.
#[test]
fn test_reclaim_resumes_mark() {
    let root = temp_root();
    let backend = LocalFileBackend::new(root.clone());
    let session = SessionId::new();
    let held = put(&backend, b"filed by the earlier mark");
    append_ref(&backend, session, &held);
    let sources = backend.reference_sources().expect("list sources");
    assert_eq!(sources.len(), 1);
    let shard = &held.0[..2];
    let line = format!("{}\n", held.0);
    let workspace = root.join(".cas").join(GC_DIR).join(REFS_DIR);
    fs::create_dir_all(&workspace).expect("create workspace");
    fs::write(workspace.join(shard), &line).expect("write the bucket");
    write_cursor(
        &root,
        sources[0].key(),
        &sources,
        json!({shard: line.len() as u64}),
        None,
    );

    let orphan = put(&backend, b"freed from the resumed buckets");
    age(&block_path(&root, &held), Duration::from_secs(7200));
    age(&block_path(&root, &orphan), Duration::from_secs(7200));

    let report = backend
        .reclaim_orphan_blocks(&immediate())
        .expect("reclaim");
    assert_eq!(report.sources, 0, "the mark was resumed, not redone");
    assert_eq!(report.removed, 1);
    assert!(block_path(&root, &held).exists());
    fs::remove_dir_all(&root).ok();
}

/// A cursor claiming a finished mark over buckets that are not there is the
/// state a crash between the two removals used to leave. Trusting it would
/// sweep from an empty reference set and free every block a live log names.
#[test]
fn test_reclaim_ignores_stale_cursor() {
    let root = temp_root();
    let backend = LocalFileBackend::new(root.clone());
    let session = SessionId::new();
    let held = put(&backend, b"named by a live log");
    append_ref(&backend, session, &held);
    age(&block_path(&root, &held), Duration::from_secs(7200));

    let sources = backend.reference_sources().expect("list sources");
    assert_eq!(sources.len(), 1);
    write_cursor(
        &root,
        sources[0].key(),
        &sources,
        json!({ &held.0[..2]: 65 }),
        None,
    );

    let report = backend
        .reclaim_orphan_blocks(&immediate())
        .expect("reclaim");
    assert_eq!(report.removed, 0);
    assert!(
        block_path(&root, &held).exists(),
        "the pass re-marked instead of trusting a cursor its buckets do not back"
    );
    fs::remove_dir_all(&root).ok();
}

/// A source that appeared after an interrupted mark belongs to no cursor, so
/// the mark starts over: resuming would skip the new source's references.
#[test]
fn test_reclaim_reads_added_source() {
    let root = temp_root();
    let backend = LocalFileBackend::new(root.clone());
    let first = SessionId::new();
    let held = put(&backend, b"named by the first session");
    append_ref(&backend, first, &held);
    let sources = backend.reference_sources().expect("list sources");
    assert_eq!(sources.len(), 1);
    write_cursor(&root, sources[0].key(), &sources, json!({}), None);

    let second = SessionId::new();
    let later = put(&backend, b"named by the second session");
    append_ref(&backend, second, &later);
    age(&block_path(&root, &held), Duration::from_secs(7200));
    age(&block_path(&root, &later), Duration::from_secs(7200));

    let report = backend
        .reclaim_orphan_blocks(&immediate())
        .expect("reclaim");
    assert_eq!(report.sources, 2, "the added source was read");
    assert_eq!(report.removed, 0);
    assert!(block_path(&root, &held).exists());
    assert!(
        block_path(&root, &later).exists(),
        "the source added after the cursor was marked"
    );
    fs::remove_dir_all(&root).ok();
}

/// A mark left by a pass that ran earlier cannot be resumed over a log that
/// grew since: the appended event names a block the resumed sweep never
/// filed, and by now that block is older than the write lease. The pass
/// reads the log again instead, and the block survives.
#[test]
fn test_reclaim_restarts_grown_mark() {
    let root = temp_root();
    let backend = LocalFileBackend::new(root.clone());
    let session = SessionId::new();
    let earlier = put(&backend, b"named before the mark");
    append_ref(&backend, session, &earlier);

    let sources = backend.reference_sources().expect("list sources");
    assert_eq!(sources.len(), 1);
    write_cursor(&root, sources[0].key(), &sources, json!({}), None);

    let later = put(&backend, b"named after the mark");
    append_ref(&backend, session, &later);
    age(&block_path(&root, &earlier), Duration::from_secs(7200));
    age(&block_path(&root, &later), Duration::from_secs(7200));

    let report = backend
        .reclaim_orphan_blocks(&immediate())
        .expect("reclaim");
    assert_eq!(report.removed, 0);
    assert!(block_path(&root, &earlier).exists());
    assert!(
        block_path(&root, &later).exists(),
        "the event appended after the cursor was marked"
    );
    fs::remove_dir_all(&root).ok();
}

/// A log that grew is not resumed even when its modification time is put back
/// to the one the cursor recorded: the length no longer matches, and the
/// appended event names a block the resumed sweep never filed.
#[test]
fn test_reclaim_sees_length_change() {
    let root = temp_root();
    let backend = LocalFileBackend::new(root.clone());
    let session = SessionId::new();
    let earlier = put(&backend, b"named before the mark");
    append_ref(&backend, session, &earlier);
    let sources = backend.reference_sources().expect("list sources");
    assert_eq!(sources.len(), 1);
    write_cursor(&root, sources[0].key(), &sources, json!({}), None);
    let recorded = fs::metadata(sources[0].path())
        .expect("stat the log")
        .modified()
        .expect("log mtime");

    let later = put(&backend, b"named after the mark");
    append_ref(&backend, session, &later);
    let handle = OpenOptions::new()
        .write(true)
        .open(sources[0].path())
        .expect("open the log");
    handle.set_modified(recorded).expect("restore the time");
    age(&block_path(&root, &earlier), Duration::from_secs(7200));
    age(&block_path(&root, &later), Duration::from_secs(7200));

    let report = backend
        .reclaim_orphan_blocks(&immediate())
        .expect("reclaim");
    assert_eq!(report.sources, 1, "the grown log was read again");
    assert_eq!(report.removed, 0);
    assert!(block_path(&root, &later).exists());
    fs::remove_dir_all(&root).ok();
}

/// A manifest rewritten in place is read again on the next pass: the
/// addresses it holds may have moved, so a bucket filed from the old one
/// would keep the dropped address and free the added one.
#[test]
fn test_manifest_reread_updates() {
    let root = temp_root();
    let backend = LocalFileBackend::new(root.clone());
    let session = SessionId::new();
    let held = put(&backend, b"named by the first manifest");
    let other = put(&backend, b"named by the second manifest");
    let manifest = root
        .join(session.to_string())
        .join("checkpoints")
        .join("cp.json");
    fs::create_dir_all(manifest.parent().expect("checkpoint dir")).expect("create dir");
    fs::write(&manifest, format!(r#"{{"block_ref":"{}"}}"#, held.0)).expect("write the manifest");
    let sources = backend.reference_sources().expect("list sources");
    assert_eq!(sources.len(), 1);
    write_cursor(&root, sources[0].key(), &sources, json!({}), None);

    fs::write(&manifest, format!(r#"{{"block_ref":"{}"}}"#, other.0))
        .expect("rewrite the manifest");
    age(&manifest, Duration::from_secs(10));
    age(&block_path(&root, &held), Duration::from_secs(7200));
    age(&block_path(&root, &other), Duration::from_secs(7200));

    let report = backend
        .reclaim_orphan_blocks(&immediate())
        .expect("reclaim");
    assert_eq!(report.sources, 1, "the rewritten manifest was read again");
    assert!(
        block_path(&root, &other).exists(),
        "the new address is kept"
    );
    assert_eq!(report.removed, 1);
    assert!(
        !block_path(&root, &held).exists(),
        "the dropped address is freed"
    );
    fs::remove_dir_all(&root).ok();
}

/// A log that grew while the cursor names logs after it sends the mark back to
/// that log alone: the log before it stays filed, so a store with a busy
/// session keeps making progress instead of starting over every pass.
#[test]
fn test_mark_survives_growth() {
    let root = temp_root();
    let backend = LocalFileBackend::new(root.clone());
    let sessions = [SessionId::new(), SessionId::new(), SessionId::new()];
    let first = put(&backend, b"named by the first log");
    let second = put(&backend, b"named by the second log");
    let third = put(&backend, b"named by the third log");
    append_ref(&backend, sessions[0], &first);
    append_ref(&backend, sessions[1], &second);
    append_ref(&backend, sessions[2], &third);

    let sources = backend.reference_sources().expect("list sources");
    assert_eq!(sources.len(), 3, "the fixture orders three logs");
    let done = sources[2].key().to_string();
    let buckets = plant_buckets(&root, &[&first, &second, &third]);
    write_cursor(&root, &done, &sources, buckets, None);

    // The middle log grows, so the rewind has one log before it to keep filed
    // and one after it to read again.
    let middle = sessions
        .into_iter()
        .find(|session| sources[1].key().ends_with(&session.to_string()))
        .expect("the middle log belongs to one of the fixture's sessions");
    let grown = put(&backend, b"named after the mark");
    append_ref(&backend, middle, &grown);
    let orphan = put(&backend, b"freed by the resumed pass");

    let report = backend
        .reclaim_orphan_blocks(&immediate())
        .expect("reclaim");
    assert_eq!(
        report.sources, 2,
        "only the log that grew and the one after it"
    );
    assert_eq!(report.removed, 1);
    for hash in [&first, &second, &third, &grown] {
        assert!(block_path(&root, hash).exists(), "{} stays", &hash.0[..8]);
    }
    assert!(!block_path(&root, &orphan).exists());
    fs::remove_dir_all(&root).ok();
}

/// A sweep resumes after the last shard an earlier pass finished: the shard it
/// already walked is not walked again, and the ones it did not reach are.
#[test]
fn test_reclaim_resumes_sweep() {
    let root = temp_root();
    let backend = LocalFileBackend::new(root.clone());
    let first = put(&backend, b"left behind by the earlier pass");
    let second = (0..64)
        .map(|n| put(&backend, format!("reached by this pass {n}").as_bytes()))
        .find(|hash| hash.0[..2] != first.0[..2])
        .expect("a block in another shard");
    age(&block_path(&root, &first), Duration::from_secs(7200));
    age(&block_path(&root, &second), Duration::from_secs(7200));
    let sources = backend.reference_sources().expect("list sources");
    assert!(sources.is_empty());
    let shard = &first.0[..2];
    assert!(
        shard < &second.0[..2],
        "the fixture needs the order it asserts"
    );
    write_cursor(&root, "", &sources, json!({}), Some(shard));

    let report = backend
        .reclaim_orphan_blocks(&immediate())
        .expect("reclaim");
    assert_eq!(report.removed, 1);
    assert!(
        block_path(&root, &first).exists(),
        "the finished shard is not walked again"
    );
    assert!(!block_path(&root, &second).exists());
    fs::remove_dir_all(&root).ok();
}

/// A block stored and named between two passes survives both: the first pass
/// finds it inside the write lease with no event naming it yet, and the
/// second finds the event the append left behind.
#[test]
fn test_reclaim_keeps_pending_write() {
    let root = temp_root();
    let backend = LocalFileBackend::new(root.clone());
    let session = SessionId::new();
    let pending = put(&backend, b"about to be named");
    let aged = put(&backend, b"fully orphaned");
    age(&block_path(&root, &aged), Duration::from_secs(7200));

    let report = backend
        .reclaim_orphan_blocks(&ReclaimPolicy::default())
        .expect("reclaim");
    assert_eq!(report.removed, 1, "only the aged orphan went");
    assert!(
        block_path(&root, &pending).exists(),
        "the lease held the block a writer had not yet named"
    );

    append_ref(&backend, session, &pending);
    age(&block_path(&root, &pending), Duration::from_secs(7200));
    let report = backend
        .reclaim_orphan_blocks(&ReclaimPolicy::default())
        .expect("reclaim");
    assert_eq!(report.removed, 0);
    assert!(
        block_path(&root, &pending).exists(),
        "the appended event keeps the block"
    );
    fs::remove_dir_all(&root).ok();
}

/// A checkpoint name that is not text cannot key a source: two such manifests
/// would collapse onto one key, and a resumed mark would skip one of them
/// while its references went unfiled.
#[cfg(unix)]
#[test]
fn test_checkpoint_key_needs_text() {
    let named = Path::new("cp.json");
    assert_eq!(
        checkpoint_key("s1", named).expect("a text name keys a source"),
        "cp:s1/cp.json"
    );
    let unnamed = Path::new(OsStr::from_bytes(b"cp\xff.json"));
    assert!(
        checkpoint_key("s1", unnamed).is_err(),
        "a name that is not text has no key"
    );
}

/// A checkpoint manifest that does not parse stops the pass: a manifest it
/// cannot read is one whose addresses it does not hold.
#[test]
fn test_reclaim_rejects_bad_manifest() {
    let root = temp_root();
    let backend = LocalFileBackend::new(root.clone());
    let session = SessionId::new();
    append_ref(&backend, session, &put(&backend, b"named by the log"));
    let orphan = put(&backend, b"survives a bad manifest");
    age(&block_path(&root, &orphan), Duration::from_secs(7200));
    let checkpoints = root.join(session.to_string()).join("checkpoints");
    fs::create_dir_all(&checkpoints).expect("create checkpoint dir");
    fs::write(checkpoints.join("cp.json"), "not a manifest").expect("write the corrupt manifest");

    let error = stopped_with(backend.reclaim_orphan_blocks(&immediate()));
    assert!(
        matches!(error, ReclaimError::Unreadable { .. }),
        "an unparsable manifest stops the pass: {error:?}"
    );
    assert!(block_path(&root, &orphan).exists(), "nothing was deleted");
    fs::remove_dir_all(&root).ok();
}

/// A budget spent during the sweep stops it after the shard it finished and
/// reports the pass incomplete; the next pass finishes the shards it did not
/// reach.
#[test]
fn test_reclaim_stops_sweep() {
    let root = temp_root();
    let backend = LocalFileBackend::new(root.clone());
    let first = put(&backend, b"freed by the first pass");
    let second = (0..64)
        .map(|n| put(&backend, format!("waits for the next pass {n}").as_bytes()))
        .find(|hash| hash.0[..2] != first.0[..2])
        .expect("a block in another shard");
    let (early, late) = if first.0[..2] < second.0[..2] {
        (&first, &second)
    } else {
        (&second, &first)
    };
    age(&block_path(&root, early), Duration::from_secs(7200));
    age(&block_path(&root, late), Duration::from_secs(7200));

    let spent = ReclaimPolicy {
        write_lease: Duration::ZERO,
        budget: Duration::ZERO,
    };
    let report = backend
        .reclaim_orphan_blocks(&spent)
        .expect("an unfinished sweep is not an error");
    assert!(!report.complete, "the budget stopped the sweep");
    assert_eq!(report.removed, 1, "the shard it reached still finished");
    assert!(!block_path(&root, early).exists());
    assert!(block_path(&root, late).exists());

    let report = backend
        .reclaim_orphan_blocks(&immediate())
        .expect("reclaim");
    assert!(report.complete, "the next pass finished the store");
    assert_eq!(report.removed, 1);
    assert!(!block_path(&root, late).exists());
    fs::remove_dir_all(&root).ok();
}

/// A pass stopped by its budget keeps the shard it finished, so the next pass
/// reports the store finished without examining that shard's blocks again.
#[test]
fn test_sweep_keeps_position() {
    let root = temp_root();
    let backend = LocalFileBackend::new(root.clone());
    let young = put(&backend, b"inside the write lease");
    // A second file in the same shard, so a pass that walked the shard again
    // would count it and the one that skipped it would not.
    let aged = BlockHash(format!("{}{}", &young.0[..2], "0".repeat(62)));
    assert_ne!(aged, young, "the fixture needs two distinct blocks");
    fs::write(block_path(&root, &aged), b"collect me").expect("write the aged block");
    age(&block_path(&root, &aged), Duration::from_secs(7200));

    let spent = ReclaimPolicy {
        write_lease: Duration::from_secs(3600),
        budget: Duration::ZERO,
    };
    let first = backend
        .reclaim_orphan_blocks(&spent)
        .expect("an unfinished sweep is not an error");
    assert!(!first.complete, "the budget stopped the sweep");
    assert_eq!(first.blocks, 2, "the shard was walked before it stopped");
    assert_eq!(first.removed, 1);
    assert_eq!(first.leased, 1, "the young block was kept");

    let second = backend
        .reclaim_orphan_blocks(&immediate())
        .expect("reclaim");
    assert!(second.complete);
    assert_eq!(second.blocks, 0, "the finished shard is not walked again");
    assert_eq!(second.removed, 0);
    assert!(block_path(&root, &young).exists());
    fs::remove_dir_all(&root).ok();
}

/// A second pass over a store the first one finished finds the same
/// references and nothing to free, so a maintenance window can repeat
/// without eroding the store.
#[test]
fn test_reclaim_repeats_cleanly() {
    let root = temp_root();
    let backend = LocalFileBackend::new(root.clone());
    let session = SessionId::new();
    let held = put(&backend, b"named across passes");
    append_ref(&backend, session, &held);
    let orphan = put(&backend, b"freed by the first pass");
    age(&block_path(&root, &held), Duration::from_secs(7200));
    age(&block_path(&root, &orphan), Duration::from_secs(7200));

    let first = backend
        .reclaim_orphan_blocks(&immediate())
        .expect("reclaim");
    assert_eq!(first.removed, 1);
    let second = backend
        .reclaim_orphan_blocks(&immediate())
        .expect("reclaim");
    assert_eq!(second.removed, 0);
    assert_eq!(second.blocks, 1, "the named block is examined again");
    assert!(second.complete);
    assert!(block_path(&root, &held).exists());
    fs::remove_dir_all(&root).ok();
}

/// A bucket the mark recorded and the sweep can no longer find stops the
/// sweep: read as naming nothing, it would free every block whose address it
/// held.
#[test]
fn test_sweep_needs_recorded_bucket() {
    let root = temp_root();
    let backend = LocalFileBackend::new(root.clone());
    let named = put(&backend, b"named by the bucket that vanished");
    age(&block_path(&root, &named), Duration::from_secs(7200));
    let workspace = root.join(".cas").join(GC_DIR);
    fs::create_dir_all(workspace.join(REFS_DIR)).expect("create workspace");
    let mut run = MarkRun {
        refs: workspace.join(REFS_DIR),
        cursor: workspace.join(CURSOR_NAME),
        fingerprint: String::new(),
        done: None,
        filed: BTreeMap::new(),
        buckets: BTreeMap::from([(named.0[..2].to_string(), 65)]),
        swept: None,
        open: BTreeMap::new(),
        since_record: 0,
        record_stride: RECORD_BATCH,
        report: ReclaimReport::default(),
    };

    let error = backend
        .sweep(&mut run, &immediate(), Instant::now())
        .expect_err("the missing bucket stops the sweep");
    assert!(matches!(error, ReclaimError::Scratch { .. }));
    assert!(block_path(&root, &named).exists(), "nothing was freed");
    fs::remove_dir_all(&root).ok();
}

/// A sweep that fails after finishing a shard keeps what that shard freed and
/// reports the count with the stop: the store already changed, so a caller
/// told nothing happened would be describing a state it is not in.
#[test]
fn test_reclaim_stopped_after_shard() {
    let root = temp_root();
    let backend = LocalFileBackend::new(root.clone());
    let first = put(&backend, b"freed before the failure");
    let second = (0..64)
        .map(|n| put(&backend, format!("behind the broken bucket {n}").as_bytes()))
        .find(|hash| hash.0[..2] != first.0[..2])
        .expect("a block in another shard");
    let (early, late) = if first.0[..2] < second.0[..2] {
        (&first, &second)
    } else {
        (&second, &first)
    };
    age(&block_path(&root, early), Duration::from_secs(7200));
    age(&block_path(&root, late), Duration::from_secs(7200));
    let sources = backend.reference_sources().expect("list sources");
    write_cursor(&root, "", &sources, json!({}), None);
    let refs = root.join(".cas").join(GC_DIR).join(REFS_DIR);
    fs::create_dir_all(refs.join(&late.0[..2])).expect("a bucket that cannot be read");

    let stop = backend
        .reclaim_orphan_blocks(&immediate())
        .expect_err("the unreadable bucket stops the sweep");
    assert_eq!(stop.report.removed, 1, "the finished shard stays freed");
    assert!(!block_path(&root, early).exists());
    assert!(
        block_path(&root, late).exists(),
        "the shard behind the failure was not swept"
    );
    fs::remove_dir_all(&root).ok();
}

/// A bucket a crash left with a torn line is cut back to the byte count its
/// cursor recorded before the mark appends, so a fresh address is not joined
/// onto the torn bytes and lost.
#[test]
fn test_resume_cuts_torn_bucket() {
    let root = temp_root();
    let backend = LocalFileBackend::new(root.clone());
    let session = SessionId::new();
    let named = put(&backend, b"named by the log");
    append_ref(&backend, session, &named);
    age(&block_path(&root, &named), Duration::from_secs(7200));
    let sources = backend.reference_sources().expect("list sources");
    assert_eq!(sources.len(), 1);
    // The earlier mark recorded a bucket holding no complete line, then died
    // halfway through writing one.
    let refs = root.join(".cas").join(GC_DIR).join(REFS_DIR);
    fs::create_dir_all(&refs).expect("create workspace");
    let shard = named.0[..2].to_string();
    let torn = refs.join(&shard);
    fs::write(&torn, "deadbeef").expect("write the torn line");
    let mut buckets = Map::new();
    buckets.insert(shard, 0.into());
    write_cursor(&root, "", &sources, Value::Object(buckets), None);

    let report = backend
        .reclaim_orphan_blocks(&immediate())
        .expect("reclaim");
    assert_eq!(report.removed, 0, "the block the log names survives");
    assert_eq!(report.blocks, 1, "the sweep reached the block");
    assert!(block_path(&root, &named).exists());
    fs::remove_dir_all(&root).ok();
}

/// A checkpoint manifest is read on every pass even when the cursor filed it,
/// since rewriting one whole can keep its size and its modification time.
#[test]
fn test_manifest_read_every_pass() {
    let root = temp_root();
    let backend = LocalFileBackend::new(root.clone());
    let session = SessionId::new();
    let held = put(&backend, b"named by the manifest");
    let manifest = root
        .join(session.to_string())
        .join("checkpoints")
        .join("cp.json");
    fs::create_dir_all(manifest.parent().expect("checkpoint dir")).expect("create dir");
    fs::write(&manifest, format!(r#"{{"block_ref":"{}"}}"#, held.0)).expect("write the manifest");
    let sources = backend.reference_sources().expect("list sources");
    assert_eq!(sources.len(), 1);
    write_cursor(&root, sources[0].key(), &sources, json!({}), None);

    let report = backend
        .reclaim_orphan_blocks(&immediate())
        .expect("reclaim");
    assert_eq!(report.sources, 1, "the manifest is read again");
    assert_eq!(report.removed, 0, "the address it holds keeps its block");
    assert!(block_path(&root, &held).exists());
    fs::remove_dir_all(&root).ok();
}

/// A source list that moved once is marked again, and one that moves again
/// stops the pass, so a sweep never runs under a mark that missed a source.
#[test]
fn test_relist_allows_one_change() {
    assert!(matches!(relist(false, false), Relist::Settled));
    assert!(matches!(relist(false, true), Relist::Settled));
    assert!(matches!(relist(true, false), Relist::Restart));
    assert!(matches!(relist(true, true), Relist::Moved));
}
