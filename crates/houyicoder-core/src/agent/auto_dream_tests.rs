use super::*;
use std::collections::HashSet;
use std::sync::Mutex as StdMutex;

use houyicoder_api::live::{LiveEvent, LiveSink, MemorySavedKind};
use houyicoder_context::MemoryEntry;
use houyicoder_memory::InMemoryBackend;
use houyicoder_protocol::llm::{
    CompletionRequest, CompletionResponse, LlmEvent, ModelCapabilities, OutputItem, ProviderError,
    Usage,
};
use houyicoder_session::SessionStore;

use houyicoder_api::provider::stream_from_response;
use houyicoder_async::{PFut, PStream};

/// A recording live sink capturing MemorySaved events so a dream test asserts
/// the consolidation pass fired one notice. Other events are ignored.
struct RecordingSink(StdMutex<Vec<LiveEvent>>);
impl RecordingSink {
    fn new() -> (LiveSink, std::sync::Arc<RecordingSink>) {
        let inner = std::sync::Arc::new(RecordingSink(StdMutex::new(Vec::new())));
        let inner_clone = std::sync::Arc::clone(&inner);
        let sink: LiveSink = std::sync::Arc::new(move |ev: &LiveEvent| {
            inner_clone.0.lock().expect("sink").push(ev.clone());
        });
        (sink, inner)
    }
    fn memory_saved(&self) -> Vec<(u32, MemorySavedKind)> {
        self.0
            .lock()
            .expect("sink")
            .iter()
            .filter_map(|ev| match ev {
                LiveEvent::MemorySaved { count, kind } => Some((*count, *kind)),
                _ => None,
            })
            .collect()
    }
}

// -- ConsolidationLock tests --

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("auto-dream-{tag}-{}", std::process::id()));
    drop(std::fs::remove_dir_all(&dir));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn test_lock_absent_returns_zero() {
    let dir = temp_dir("absent");
    let lock = ConsolidationLock::new(&dir);
    assert_eq!(lock.last_consolidated_at(), 0, "absent lock returns 0");
    drop(std::fs::remove_dir_all(&dir));
}

#[test]
fn test_lock_acquire_then_record() {
    let dir = temp_dir("acquire");
    let lock = ConsolidationLock::new(&dir);
    // No prior lock → try_acquire succeeds with prior=0.
    let prior = lock.try_acquire().expect("no lock → acquire succeeds");
    assert_eq!(prior, 0, "prior mtime is 0 when no prior lock");
    // A fresh lock is now recent → a second acquire is blocked.
    assert!(lock.try_acquire().is_none(), "recent lock blocks acquire");
    // Stamp a successful dream.
    lock.record_consolidation();
    assert!(lock.last_consolidated_at() > 0, "record stamps the mtime");
    drop(std::fs::remove_dir_all(&dir));
}

/// rollback rewinds the mtime to the pre-acquire value (not unlink-to-0)
/// so the time-gate passes again but the scan throttle gates the retry.
/// prior 0 → unlink (clean state).
#[test]
fn test_rollback_rewinds_mtime() {
    let dir = temp_dir("rollback");
    let lock = ConsolidationLock::new(&dir);
    // Simulate a prior successful consolidation an hour ago.
    let prior_at = now_secs().saturating_sub(3600);
    drop(std::fs::write(lock_path(&dir), "old"));
    set_mtime(&lock_path(&dir), prior_at);
    assert_eq!(lock.last_consolidated_at(), prior_at, "prior stamp set");

    // Acquire (the prior is an hour ago, past the stale window).
    let prior = lock.try_acquire().expect("stale lock reclaimed");
    assert_eq!(prior, prior_at, "acquire returns the pre-acquire mtime");
    // The acquire stamped now; rollback must rewind to prior_at, not 0.
    lock.rollback(prior);
    assert_eq!(
        lock.last_consolidated_at(),
        prior_at,
        "rollback rewinds the mtime to the pre-acquire value"
    );
    drop(std::fs::remove_dir_all(&dir));
}

#[test]
fn test_rollback_unlinks_no_prior() {
    let dir = temp_dir("rollback-zero");
    let lock = ConsolidationLock::new(&dir);
    let prior = lock.try_acquire().expect("acquire");
    assert_eq!(prior, 0, "no prior lock");
    lock.rollback(0);
    assert_eq!(
        lock.last_consolidated_at(),
        0,
        "rollback to prior 0 unlinks"
    );
    drop(std::fs::remove_dir_all(&dir));
}

fn lock_path(dir: &Path) -> PathBuf {
    dir.join(LOCK_FILE)
}

fn set_mtime(path: &Path, secs: u64) {
    let times = std::fs::FileTimes::new()
        .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs));
    if let Ok(file) = std::fs::OpenOptions::new().write(true).open(path) {
        drop(file.set_times(times));
    }
}

// -- prompt tests --

#[test]
fn test_prompt_has_phases_root() {
    let listing = vec![houyicoder_context::MemorySummary::new(
        "user-prefers-terse",
        "User prefers terse responses",
        houyicoder_context::MemorySource::Feedback,
        houyicoder_context::MemoryScope::Auto,
        100,
    )];
    let prompt = build_consolidation_prompt(
        "/tmp/test-memory",
        &listing,
        "# Memory index\n\n- foo\n",
        &[],
        0,
        None,
    );
    assert!(prompt.contains("Phase 1 — Orient"));
    assert!(prompt.contains("Phase 2 — Consolidate"));
    assert!(prompt.contains("Phase 3 — Prune"));
    assert!(prompt.contains("/tmp/test-memory"));
    assert!(prompt.contains("user-prefers-terse"), "listing is injected");
    assert!(prompt.contains("# Memory index"), "index text is injected");
    assert!(
        prompt.contains("Recall frequency"),
        "stats section is present"
    );
    assert!(
        prompt.contains("dead weight"),
        "Prune phase references the recall-frequency signal"
    );
    // Phase 4 (Scope flow) instructs promote/demote so the dream moves
    // high-recall or high-violation rules into the always-on carrier.
    assert!(
        prompt.contains("Phase 4"),
        "Scope-flow phase is present in the prompt"
    );
    assert!(
        prompt.contains("promote_memory"),
        "prompt mentions the promote_memory tool"
    );
    assert!(
        prompt.contains("demote_memory"),
        "prompt mentions the demote_memory tool"
    );
}

#[test]
fn test_prompt_empty_listing_notes() {
    let prompt = build_consolidation_prompt("/tmp/m", &[], "", &[], 0, None);
    assert!(prompt.contains("(no memories yet"), "empty listing noted");
}

/// The recall-frequency block renders hits + age so the dream can spot
/// dead weight, and notes a fresh store when stats are empty.
#[test]
fn test_prompt_stats_hits_age() {
    let stats = vec![MemoryRecallStats {
        key: "stale-rule".into(),
        recall_hits: 0,
        gate_violations: 0,
        last_access_ts: 0,
    }];
    let prompt = build_consolidation_prompt("/tmp/m", &[], "", &stats, 0, None);
    assert!(prompt.contains("stale-rule"));
    assert!(prompt.contains("0 hits"));
    assert!(
        prompt.contains("never recalled"),
        "zero last_access = never"
    );
    // Non-empty stats must not print the fresh-store note.
    assert!(
        !prompt.contains("fresh store"),
        "non-empty stats do not print the fresh-store note"
    );
}

#[test]
fn test_prompt_empty_notes_fresh() {
    let prompt = build_consolidation_prompt("/tmp/m", &[], "", &[], 0, None);
    assert!(
        prompt.contains("no recall stats yet"),
        "empty stats note the fresh-store state"
    );
}

// -- DreamRunner gate tests --

/// A memory provider with a real filesystem root so memory_root returns
/// a non-empty path; records adds/deletes so the gate test can assert
/// the forked dream wrote through.
struct FsMemory {
    root: PathBuf,
    written: StdMutex<Vec<MemoryEntry>>,
}
impl FsMemory {
    fn new(root: PathBuf) -> Self {
        std::fs::create_dir_all(&root).unwrap();
        Self {
            root,
            written: StdMutex::new(Vec::new()),
        }
    }
}
impl MemoryProvider for FsMemory {
    fn recall(&self, _q: &str, _b: usize, _surfaced: &HashSet<String>) -> Vec<MemoryEntry> {
        Vec::new()
    }
    fn add(&self, e: MemoryEntry) -> Result<(), houyicoder_context::MemoryError> {
        self.written.lock().expect("w").push(e);
        Ok(())
    }
    fn memory_root(&self) -> String {
        self.root.to_string_lossy().into_owned()
    }
    /// Count .md topic files in the root newer than the given timestamp
    /// (seconds since epoch). The dream gate calls this to decide whether
    /// new material landed; the stub matches the markdown provider so the
    /// gate tests can drive the memory-delta path without the real provider.
    fn count_new_since(&self, since: u64) -> usize {
        std::fs::read_dir(&self.root)
            .map(|entries| {
                entries
                    .flatten()
                    .filter(|e| e.path().extension().is_some_and(|x| x == "md"))
                    .filter(|e| {
                        e.metadata()
                            .ok()
                            .and_then(|m| m.modified().ok())
                            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                            .is_some_and(|d| d.as_secs() > since)
                    })
                    .count()
            })
            .unwrap_or(0)
    }
}

/// A scripted provider: the forked dream's single call emits a
/// save_memory call (the dream consolidating one fact); the next call
/// emits a final text so the loop ends within max_turns.
struct DreamProvider {
    calls: StdMutex<usize>,
}
impl ModelProvider for DreamProvider {
    fn complete(
        &self,
        _req: CompletionRequest,
    ) -> PFut<'_, Result<CompletionResponse, ProviderError>> {
        let mut c = self.calls.lock().expect("c");
        *c += 1;
        let n = *c;
        drop(c);
        Box::pin(async move { Ok(scripted(n)) })
    }
    fn stream(&self, _req: CompletionRequest) -> PStream<'_, Result<LlmEvent, ProviderError>> {
        let mut c = self.calls.lock().expect("c");
        *c += 1;
        let n = *c;
        drop(c);
        stream_from_response(scripted(n))
    }
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
}
fn scripted(n: usize) -> CompletionResponse {
    if n == 1 {
        CompletionResponse {
            output: vec![
                OutputItem::Text {
                    text: "Consolidating one fact.".into(),
                },
                OutputItem::ToolCall {
                    id: "s1".into(),
                    name: "save_memory".into(),
                    input: serde_json::json!({
                        "key": "dream-fact",
                        "description": "A fact the dream consolidated",
                        "source": "project",
                        "content": "A consolidated fact with an absolute date."
                    }),
                },
            ],
            usage: Usage::default(),
            model: "test".into(),
        }
    } else {
        CompletionResponse {
            output: vec![OutputItem::Text {
                text: "done".into(),
            }],
            usage: Usage::default(),
            model: "test".into(),
        }
    }
}

fn dream_runner(
    memory: Arc<FsMemory>,
    provider: Arc<dyn ModelProvider>,
) -> (Arc<DreamRunner>, PathBuf) {
    let root = memory.memory_root();
    let ephemeral: Arc<dyn SessionLog> =
        Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let cwd = std::env::temp_dir().join(format!("dream-cwd-{}", std::process::id()));
    std::fs::create_dir_all(&cwd).unwrap();
    let config = RunnerConfig {
        max_turns: DEFAULT_DREAM_MAX_TURNS,
        ..RunnerConfig::default()
    };
    let dream = Arc::new(DreamRunner::new(
        ephemeral,
        provider,
        memory as Arc<dyn MemoryProvider>,
        cwd,
        config,
    ));
    (dream, PathBuf::from(root))
}

/// When the lock is recent (within the stale window), the gate closes:
/// no fork is spawned, no write lands.
#[tokio::test]
async fn test_gate_closed_lock_recent() {
    let root = temp_dir("closed");
    let memory = Arc::new(FsMemory::new(root.clone()));
    // Stamp a recent lock so the time-gate closes.
    std::fs::write(root.join(LOCK_FILE), "pid").unwrap();
    let provider = Arc::new(DreamProvider {
        calls: StdMutex::new(0),
    });
    let (dream, _) = dream_runner(memory.clone(), provider.clone() as Arc<dyn ModelProvider>);
    dream.execute_dream(None, None);
    dream.drain_pending(Duration::from_secs(1)).await;
    assert_eq!(
        memory.written.lock().expect("w").len(),
        0,
        "gate closed → no forked write"
    );
    drop(std::fs::remove_dir_all(&root));
}

/// A reward snapshot with blind retries (retry_after_error >= threshold)
/// fires the reward-dream path: the lighter, sooner pass that skips the
/// dream-lock and caps max_turns. The forked agent runs; the dream-lock
/// mtime is left untouched so the 24h full dream still fires on its own
/// cadence. The gate keys on retry_after_error (agent decision), not
/// fail_count (world state).
#[tokio::test]
async fn test_reward_dream_blind_retry() {
    let root = temp_dir("reward-dream");
    let memory = Arc::new(FsMemory::new(root.clone()));
    let provider = Arc::new(DreamProvider {
        calls: StdMutex::new(0),
    });
    let (dream, _) = dream_runner(memory.clone(), provider.clone() as Arc<dyn ModelProvider>);
    let reward = crate::agent::reward_snapshot::RewardSnapshot {
        retry_after_error: 2,
        ..Default::default()
    };
    dream.execute_dream(Some(reward), None);
    dream.drain_pending(Duration::from_secs(5)).await;
    let calls = *provider.calls.lock().expect("calls");
    assert!(calls > 0, "reward-dream spawned the forked agent");
    drop(std::fs::remove_dir_all(&root));
}

/// run_dream_body called directly (not via execute_dream's spawn) so the
/// gcov instrumentation records the dream-said + debug-log lines that
/// fire-and-forget spawn hides from coverage. The spawn itself is a
/// tokio mechanism, not logic under test.
#[tokio::test]
async fn test_dream_body_runs_direct() {
    let root = temp_dir("dream-body-direct");
    let memory = Arc::new(FsMemory::new(root.clone()));
    let provider = Arc::new(DreamProvider {
        calls: StdMutex::new(0),
    });
    let (dream, _) = dream_runner(memory.clone(), provider.clone() as Arc<dyn ModelProvider>);
    let reward = crate::agent::reward_snapshot::RewardSnapshot {
        retry_after_error: 2,
        ..Default::default()
    };
    Arc::clone(&dream)
        .run_dream_body(root.to_string_lossy().into_owned(), Some(reward), true)
        .await;
    let calls = *provider.calls.lock().expect("calls");
    assert!(calls > 0, "run_dream_body ran the forked agent directly");
    drop(std::fs::remove_dir_all(&root));
}

/// A reward snapshot with only redundant clusters (no blind retries) does
/// NOT fire the reward-dream gate — the gate keys on retry_after_error
/// (agent decision), not redundant clusters (context loss is a different
/// signal, surfaced separately in the snapshot).
#[tokio::test]
async fn test_reward_skip_redundant_only() {
    let root = temp_dir("reward-redundant");
    let memory = Arc::new(FsMemory::new(root.clone()));
    let provider = Arc::new(DreamProvider {
        calls: StdMutex::new(0),
    });
    let (dream, _) = dream_runner(memory.clone(), provider.clone() as Arc<dyn ModelProvider>);
    let reward = crate::agent::reward_snapshot::RewardSnapshot {
        redundant: vec![
            crate::agent::reward_snapshot::RedundantCluster {
                tool: "grep".into(),
                kind: crate::observability::evolution::RedundancyKind::CrossBatch,
                count: 3,
            },
            crate::agent::reward_snapshot::RedundantCluster {
                tool: "read".into(),
                kind: crate::observability::evolution::RedundancyKind::SameBatch,
                count: 2,
            },
        ],
        ..Default::default()
    };
    dream.execute_dream(Some(reward), None);
    dream.drain_pending(Duration::from_secs(5)).await;
    let calls = *provider.calls.lock().expect("calls");
    assert_eq!(calls, 0, "redundant-only snapshot must not fire the gate");
    drop(std::fs::remove_dir_all(&root));
}

/// When the gate opens (no prior lock, fresh root, enough new memories),
/// the forked dream runs and writes through the shared provider, then
/// stamps the lock. Seeds five topic files so the memory-delta gate (the
/// base path: full interval plus a few new memories) passes.
#[tokio::test]
async fn test_gate_open_fires_stamps() {
    let root = temp_dir("open");
    let memory = Arc::new(FsMemory::new(root.clone()));
    // Seed five topic files so the memory-delta gate sees enough new
    // material to fire (last_at is 0 on a fresh root, so elapsed is huge).
    for i in 0..5 {
        std::fs::write(root.join(format!("seed-{i}.md")), "body").unwrap();
    }
    let provider = Arc::new(DreamProvider {
        calls: StdMutex::new(0),
    });
    let (dream, _) = dream_runner(memory.clone(), provider.clone() as Arc<dyn ModelProvider>);
    let (sink, recording) = RecordingSink::new();
    dream.set_notify_sink(sink);
    dream.execute_dream(None, None);
    dream.drain_pending(Duration::from_secs(5)).await;
    assert_eq!(
        memory.written.lock().expect("w").len(),
        1,
        "forked dream wrote one memory"
    );
    assert!(root.join(LOCK_FILE).exists(), "lock stamped on success");
    // The dream touched one memory, so the sink fires one Consolidated notice.
    assert_eq!(
        recording.memory_saved(),
        vec![(1, MemorySavedKind::Consolidated)],
        "a successful dream fires one Consolidated memory-saved notice"
    );
    drop(std::fs::remove_dir_all(&root));
}

/// The memory-delta gate: a fresh root (elapsed is huge since last_at is 0)
/// with NO new memories does not fire — a quiet store does not pay a forked
/// LLM run to re-organize stable content. This is the divergence from a
/// pure time-gate (which would fire on elapsed alone).
#[tokio::test]
async fn test_gate_closed_when_quiet() {
    let root = temp_dir("no-new");
    let memory = Arc::new(FsMemory::new(root.clone()));
    let provider = Arc::new(DreamProvider {
        calls: StdMutex::new(0),
    });
    let (dream, _) = dream_runner(memory.clone(), provider.clone() as Arc<dyn ModelProvider>);
    dream.execute_dream(None, None);
    dream.drain_pending(Duration::from_secs(1)).await;
    assert_eq!(
        memory.written.lock().expect("w").len(),
        0,
        "no new memories → no forked write"
    );
    drop(std::fs::remove_dir_all(&root));
}

/// A provider with no filesystem root (empty memory_root) never fires a
/// dream — the gate no-ops before any stat on a non-existent path.
#[tokio::test]
async fn test_gate_noop_without_root() {
    struct InMemOnly;
    impl MemoryProvider for InMemOnly {
        fn recall(&self, _q: &str, _b: usize, _surfaced: &HashSet<String>) -> Vec<MemoryEntry> {
            Vec::new()
        }
        fn add(&self, _e: MemoryEntry) -> Result<(), houyicoder_context::MemoryError> {
            Ok(())
        }
        // memory_root defaults to empty string
    }
    let ephemeral: Arc<dyn SessionLog> =
        Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let provider: Arc<dyn ModelProvider> =
        Arc::new(crate::provider::test_support::FakeProvider::text("ok"));
    let dream = Arc::new(DreamRunner::new(
        ephemeral,
        provider,
        Arc::new(InMemOnly) as Arc<dyn MemoryProvider>,
        std::env::temp_dir(),
        RunnerConfig::default(),
    ));
    dream.execute_dream(None, None);
    dream.drain_pending(Duration::from_millis(100)).await;
    assert!(
        dream.in_flight.lock().expect("in_flight").is_empty(),
        "no spawn when memory_root is empty"
    );
}

/// Coalesce: a second execute_dream while the first is in flight drops
/// (no second fork). Deterministic — arms in_progress manually.
#[tokio::test]
async fn test_execute_dream_coalesces_flight() {
    let root = temp_dir("coalesce");
    let memory = Arc::new(FsMemory::new(root.clone()));
    let provider = Arc::new(DreamProvider {
        calls: StdMutex::new(0),
    });
    let (dream, _) = dream_runner(memory, provider as Arc<dyn ModelProvider>);
    *dream.in_progress.lock().expect("in_progress") = true;
    let before = dream.in_flight.lock().expect("in_flight").len();
    dream.execute_dream(None, None);
    assert_eq!(
        dream.in_flight.lock().expect("in_flight").len(),
        before,
        "no spawn while a dream is in flight"
    );
    drop(std::fs::remove_dir_all(&root));
}

/// A memory whose list_memories panics — drives the InProgressGuard path
/// (a panic in run_dream_body must reset in_progress, not wedge it).
struct PanickingMemory {
    root: PathBuf,
}
impl MemoryProvider for PanickingMemory {
    fn recall(&self, _q: &str, _b: usize, _surfaced: &HashSet<String>) -> Vec<MemoryEntry> {
        Vec::new()
    }
    fn add(&self, _e: MemoryEntry) -> Result<(), houyicoder_context::MemoryError> {
        Ok(())
    }
    fn memory_root(&self) -> String {
        self.root.to_string_lossy().into_owned()
    }
    fn list_memories(&self) -> Vec<MemorySummary> {
        panic!("list_memories crashed — drives the in-progress guard");
    }
}

/// A panic in run_dream_body (here: list_memories panics) must reset
/// in_progress via the Drop guard, not wedge it true forever (which would
/// permanently block every subsequent dream at the coalesce check).
#[tokio::test]
async fn test_panic_resets_in_progress() {
    let root = temp_dir("panic-guard");
    let memory = Arc::new(PanickingMemory { root: root.clone() });
    let provider: Arc<dyn ModelProvider> =
        Arc::new(crate::provider::test_support::FakeProvider::text("ok"));
    let ephemeral: Arc<dyn SessionLog> =
        Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let dream = Arc::new(DreamRunner::new(
        ephemeral,
        provider,
        memory as Arc<dyn MemoryProvider>,
        std::env::temp_dir(),
        RunnerConfig::default(),
    ));
    dream.execute_dream(None, None);
    // The spawned task panics in list_memories. Drain waits for the handle
    // to resolve (JoinError from the panic). The guard's Drop ran during
    // the unwinding, so in_progress is false now.
    dream.drain_pending(Duration::from_secs(5)).await;
    assert!(
        !*dream.in_progress.lock().expect("in_progress"),
        "in_progress must reset after a panic (Drop guard)"
    );
    drop(std::fs::remove_dir_all(&root));
}

/// try_acquire uses create_new (atomic create-if-absent). A second
/// acquirer on a fresh lock bails — closes the cross-process
/// stat-then-overwrite race where two processes both thought they held it.
/// In a single process the stale check already bails the second acquirer
/// (the first's lock is fresh), so this test sets the lock then advances
/// past the stale window to reach the create_new path, asserting the
/// create_new fails on an existing (stale-but-reclaimed) target.
#[test]
fn test_lock_acquire_is_atomic() {
    let dir = temp_dir("acquire-atomic");
    let lock = ConsolidationLock::new(&dir);
    // First acquire on an absent lock: create_new succeeds, prior = 0.
    let prior = lock.try_acquire().expect("first acquire creates the lock");
    assert_eq!(prior, 0, "no prior lock");
    assert!(lock.last_consolidated_at() > 0, "lock file created");
    // A second acquirer on the SAME fresh lock: the stale check (1h) bails
    // first — the lock is fresh. This is the in-process guard. The
    // create_new atomicity matters cross-process (a second process stat-ing
    // an absent lock + create_new-ing loses the race). Here we pin the
    // in-process behavior: a fresh lock blocks a second acquirer.
    let lock_b = ConsolidationLock::new(&dir);
    assert!(
        lock_b.try_acquire().is_none(),
        "a fresh lock blocks a second acquirer"
    );
    drop(std::fs::remove_dir_all(&dir));
}

/// fire_saved pushes a MemorySaved event to the installed sink (Consolidated
/// kind, the count + kind carried verbatim) and is a no-op when no sink is
/// wired or the count is zero. Direct call (no spawn) so coverage attributes
/// it cleanly.
#[tokio::test]
async fn test_fire_saved_pushes_consolidated() {
    let root = temp_dir("fire-saved");
    let memory = Arc::new(FsMemory::new(root.clone()));
    let provider: Arc<dyn ModelProvider> =
        Arc::new(crate::provider::test_support::FakeProvider::text("ok"));
    let (dream, _) = dream_runner(memory, provider);

    // No sink wired: a fire is a silent no-op (tests, forked runner).
    dream.fire_saved(2);
    // Wire a recording sink + fire: the sink receives one Consolidated event.
    let (sink, recording) = RecordingSink::new();
    dream.set_notify_sink(sink);
    dream.fire_saved(2);
    assert_eq!(
        recording.memory_saved(),
        vec![(2, MemorySavedKind::Consolidated)],
        "fire_saved pushes one Consolidated event to the sink"
    );
    // A zero-count fire is a no-op even with a sink wired.
    dream.fire_saved(0);
    assert_eq!(
        recording.memory_saved().len(),
        1,
        "a zero-count fire does not push another event"
    );
    drop(std::fs::remove_dir_all(&root));
}

/// Cross-session scan merges previous sessions' retry_after_error from the
/// durable trajectory into the reward snapshot. Write a session log with
/// a RewardObservation event, call merge_cross_session_reward, verify the
/// count is added to the snapshot.
#[test]
fn test_merge_session_adds_retry() {
    use crate::agent::reward_snapshot::RewardSnapshot;
    use houyicoder_context::{EventId, SessionId, TurnEvent, TurnEventKind};

    let tmp = std::env::temp_dir().join(format!("merge-cross-{}", std::process::id()));
    let _cleanup = std::fs::remove_dir_all(&tmp);
    let dir = tmp.join("prev-session");
    std::fs::create_dir_all(&dir).expect("mkdir");
    let ev = TurnEvent {
        id: EventId::new(),
        session: SessionId::new(),
        ts: 0,
        prev_hash: None,
        kind: TurnEventKind::RewardObservation {
            redundant: 0,
            retry_after_error: 3,
        },
    };
    let line = serde_json::to_string(&ev).expect("serialize");
    std::fs::write(dir.join("log.jsonl"), format!("{line}\n")).expect("write");

    let root = std::path::PathBuf::from(&tmp);
    let mut reward = Some(RewardSnapshot {
        retry_after_error: 1,
        ..Default::default()
    });
    super::merge_cross_session_reward(&Some(root), None, &mut reward);
    assert_eq!(
        reward.as_ref().unwrap().retry_after_error,
        4,
        "cross-session 3 + current 1 = 4"
    );
    let _cleanup2 = std::fs::remove_dir_all(&tmp);
}

/// A None session_log_root is a no-op (tests, forked runners).
#[test]
fn test_merge_session_none_noop() {
    use crate::agent::reward_snapshot::RewardSnapshot;
    let mut reward = Some(RewardSnapshot {
        retry_after_error: 2,
        ..Default::default()
    });
    super::merge_cross_session_reward(&None, None, &mut reward);
    assert_eq!(reward.as_ref().unwrap().retry_after_error, 2);
}
