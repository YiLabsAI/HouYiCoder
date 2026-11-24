//! Background memory consolidation.
//!
//! Fires a forked agent when the time-gate (default 24h) passes and the
//! lock is acquirable. The agent merges near-duplicate topics, resolves
//! contradictions, converts relative dates to absolute, prunes stale
//! entries — all through the structured memory tools (show / save / delete
//! by key), never raw file access. Runs off the hot path as a
//! fire-and-forget task with a turn budget as backstop.
//!
//! A scan throttle (ten minutes) backs off when the gate has passed but a
//! prior dream failed: a failed dream never advances the mtime of the last
//! success, so the gate keeps passing and the throttle prevents a retry
//! storm.
//!
//! The index is rebuilt in code before and after the LLM run, so the agent
//! sees a clean store on entry and the derived index regenerates from the
//! final topic set on exit — no hand-maintained index that can drift.
//!
//! Scope promote/demote: the forked agent calls promote_memory / demote_memory
//! to move a topic between the auto and project scopes. save_memory is pinned
//! to the auto root for the dream, with one host-side exception: refreshing a
//! key that already lives in the project root writes that root in place, so
//! the explicit entry is updated rather than shadowed.

use std::fs::File;
use std::fs::FileTimes;
use std::fs::OpenOptions;
use std::fs::create_dir_all;
use std::fs::metadata;
use std::fs::read_to_string;
use std::io;
use std::io::Write;
use std::mem::take;
use std::path::{Path, PathBuf};
use std::process::id;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use crate::agent::memory::MutationLog;
use houyicoder_api::agent_event::{
    EventHandler, MemoryChange, MemoryChangeCausality, MemoryChangeOrigin, MemoryChangedEvent,
};
use houyicoder_api::memory::MemoryProvider;
use houyicoder_api::provider::ModelProvider;
use houyicoder_api::session::SessionLog;
use houyicoder_async::PFut;
use houyicoder_context::{
    MemoryChangeId, MemoryOrigin, MemoryRecallStats, MemoryScope, MemorySummary, SessionId,
};
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::agent::prompt::reward_lesson::build_reward_lesson_prompt;
use crate::agent::reward_snapshot::RewardSnapshot;
use crate::agent::reward_snapshot::format_reward;
use crate::agent::runner_config::RunnerConfig;
use crate::agent::tools::{MemoryArchiveTool, MemoryDeleteTool, MemoryReadTool, MemoryWriteTool};
use crate::agent::{RunError, RunResult, Runner, ToolRegistry};
use tokio::time::sleep;

const LOCK_FILE: &str = ".consolidate-lock";
pub(crate) const INDEX_FILE: &str = "MEMORY.md";
/// Minimum hours between dreams. The lock mtime is lastConsolidatedAt; the
/// time-gate closes until this many hours have elapsed.
const DEFAULT_MIN_HOURS: u64 = 24;
/// Minimum new memories since the last dream for the base fire path. The
/// dream consolidates NEW material, so a quiet store (fewer than this many
/// new topic files since the last dream) does not pay a forked LLM run. This
/// replaces a session-count proxy with a direct "new material" metric.
const MIN_NEW_MEMORIES: usize = 5;
/// Early-fire path: when a burst of new memories lands, consolidate sooner
/// rather than waiting for the full interval. The floor prevents a churn
/// spike from firing back-to-back dreams.
const EARLY_FIRE_SECS: u64 = 4 * 3600;
const EARLY_FIRE_NEW: usize = 10;
/// Scan throttle: once the time-gate has passed, do not re-attempt the lock
/// more often than this. This is the retry backoff when a prior dream failed
/// (a failure leaves the last-success mtime untouched, so the time-gate
/// keeps passing every turn — the throttle prevents a retry storm).
const SCAN_INTERVAL_SECS: u64 = 600;
/// Reward-dream: a lighter, sooner pass fired by reward signal (failure
/// clusters / redundant calls) rather than the 24h memory-delta gate. Stays
/// under a small max_turns so it is a quick lesson-extraction, not a full
/// consolidation. The 15min floor + the scan throttle prevent a reward
/// storm from firing back-to-back reward-dreams.
const REWARD_DREAM_MIN_INTERVAL_SECS: u64 = 15 * 60;
/// Blind retries (same-input call re-issued after a prior failure, no
/// intervening write) needed to wake the reward-dream. This is an
/// agent-decision signal, not a world-state failure count — fail_count
/// measures a command failed, retry_after_error measures the agent chose
/// to retry it without changing anything. 2 = a pattern (one retry is
/// noise, two is a signal the agent is stuck); 1 would fire on every
/// transient blip; 3 would waste 3 retries before the dream wakes.
const REWARD_DREAM_RETRY_THRESHOLD: u32 = 2;
const REWARD_DREAM_MAX_TURNS: u32 = 8;
/// Max turns for the forked consolidation agent. Generous: the agent lists,
/// inspects, merges, and prunes, which takes several tool round-trips.
pub const DEFAULT_DREAM_MAX_TURNS: u32 = 25;

/// The consolidation lock: an exclusive claim on one memory root's dream.
/// Mutual exclusion is a kernel file lock, which the kernel releases when
/// the holder exits, so a dead dream never wedges the store and no stale
/// reclaim or timeout is needed. The lock file's mtime is lastConsolidatedAt;
/// only a completed full dream stamps it, so a reward dream can hold the
/// claim without moving the interval gate.
pub(crate) struct ConsolidationLock {
    path: PathBuf,
}

impl ConsolidationLock {
    pub(crate) fn new(memory_root: &Path) -> Self {
        Self {
            path: memory_root.join(LOCK_FILE),
        }
    }

    /// mtime of the lock file (= lastConsolidatedAt), or 0 if absent.
    /// Per-turn cost: one stat.
    pub(crate) fn last_consolidated_at(&self) -> u64 {
        metadata(&self.path)
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    /// Claim the dream slot, or None when another live process holds it.
    /// The claim does not wait: a held lock means that dream is working, and
    /// a second dream over one store would rewrite topics from a stale
    /// listing. The pre-claim stamp is remembered so a claim that ends
    /// without a completed full dream restores it.
    pub(crate) fn try_acquire(&self) -> Option<ConsolidationGuard> {
        if let Some(parent) = self.path.parent() {
            drop(create_dir_all(parent));
        }
        let prior_stamp = self.last_consolidated_at();
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&self.path)
            .ok()?;
        file.try_lock().ok()?;
        Some(ConsolidationGuard {
            file,
            prior_stamp,
            stamped: false,
        })
    }
}

/// A live dream's claim on the consolidation lock, released on drop.
///
/// The stamp is owned here rather than by the lock: creating the lock file
/// stamps its mtime, so a claim that did not complete a full dream must put
/// the last-success stamp back. Otherwise a reward dream on a fresh store,
/// or a failed dream, would close the interval gate as if it had succeeded.
pub(crate) struct ConsolidationGuard {
    /// Held open for as long as the kernel lock is held. The stamp is
    /// written through this handle: a second handle to a locked file is
    /// refused on some platforms.
    file: File,
    /// The last-success stamp before the claim.
    prior_stamp: u64,
    /// Whether this claim completed a full dream and stamped its success.
    stamped: bool,
}

impl ConsolidationGuard {
    /// Stamp a completed full consolidation so the interval gate measures
    /// from the last success.
    pub(crate) fn record_consolidation(&mut self) {
        match self.stamp() {
            Ok(()) => self.stamped = true,
            Err(e) => {
                let msg = format!("consolidation stamp failed: {e}");
                tracing::warn!("{msg}");
            }
        }
    }

    fn stamp(&self) -> io::Result<()> {
        self.file.set_len(0)?;
        let mut file = &self.file;
        file.write_all(id().to_string().as_bytes())?;
        self.file.set_modified(SystemTime::now())
    }
}

impl Drop for ConsolidationGuard {
    fn drop(&mut self) {
        if !self.stamped {
            let times =
                FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(self.prior_stamp));
            if let Err(e) = self.file.set_times(times) {
                let msg = format!("consolidation stamp restore failed: {e}");
                tracing::warn!("{msg}");
            }
        }
        drop(self.file.unlock());
    }
}

/// Drop guard that resets the in-progress flag on scope exit, including a
/// panic unwinding the spawned task. Without this, a panic anywhere in the
/// dream body between the spawn and the trailing reset would leave the flag
/// true forever, permanently wedging future dreams (the coalesce check would
/// bail every subsequent trigger). On drop it clears the flag without
/// panicking — a poisoned mutex is left as-is rather than double-panicking
/// inside Drop. Shared with the extractor (same in-progress pattern).
pub(crate) struct InProgressGuard<'a> {
    flag: &'a Mutex<bool>,
    notify: Option<&'a Notify>,
}

impl<'a> InProgressGuard<'a> {
    pub(crate) fn new(flag: &'a Mutex<bool>) -> Self {
        Self { flag, notify: None }
    }

    pub(crate) fn with_notify(flag: &'a Mutex<bool>, notify: &'a Notify) -> Self {
        Self {
            flag,
            notify: Some(notify),
        }
    }
}

impl Drop for InProgressGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut ip) = self.flag.lock() {
            *ip = false;
        }
        if let Some(notify) = self.notify {
            notify.notify_waiters();
        }
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Format the memory listing as a bulleted block for the prompt, one line per
/// entry (key, source, scope, mtime, description). The scope is named because
/// a read without one is refused for a key stored in two scopes.
pub(crate) fn format_listing(listing: &[MemorySummary]) -> String {
    if listing.is_empty() {
        return "(no memories yet — orient from the index below if present)".to_string();
    }
    let mut out = String::new();
    for m in listing.iter().take(200) {
        out.push_str(&format!(
            "- {} [{}] scope={} mtime={}: {}\n",
            m.key,
            m.memory_type.as_label(),
            m.scope.as_label(),
            m.mtime_secs,
            m.description,
        ));
    }
    out
}

/// Format the recall-frequency stats as a compact block so the dream can
/// spot dead weight: entries with zero recall hits or a stale last-access
/// are prune candidates. age is days since last recall (never = never
/// recalled). Capped at 200 rows so a large store does not bloat the prompt.
/// The gate_violations column surfaces signal B: rules the agent keeps
/// violating (a PreToolUse gate denied a call on that rule) are promotion
/// candidates — the rule should move into the project scope so it stops
/// relying on recall luck.
fn format_recall_stats(stats: &[MemoryRecallStats], now: u64) -> String {
    if stats.is_empty() {
        return "(no recall stats yet — a fresh store or a cold restart)".to_string();
    }
    let mut out = String::new();
    for s in stats.iter().take(200) {
        let age = if s.last_access_ts == 0 {
            "never recalled".to_string()
        } else {
            let days = now.saturating_sub(s.last_access_ts) / 86_400;
            format!("{days} days since last recall")
        };
        let violations = if s.gate_violations == 0 {
            String::new()
        } else {
            format!(", {} gate violations", s.gate_violations)
        };
        out.push_str(&format!(
            "- {}: {} hits ({}){}\n",
            s.key, s.recall_hits, age, violations
        ));
    }
    out
}

/// Build the consolidation prompt for the forked dream agent. Four phases:
/// Orient (scan the memory store + recall-frequency stats), Consolidate
/// (merge near-duplicates, resolve contradictions), Prune (drop dead weight
/// surfaced by the recall-frequency stats via delete_memory), and Scope-flow
/// (promote high-recall entries into the project scope so the rule no
/// longer relies on recall luck).
pub(crate) fn build_consolidation_prompt(
    memory_root: &str,
    listing: &[MemorySummary],
    index_text: &str,
    stats: &[MemoryRecallStats],
    now_secs: u64,
    reward: Option<&RewardSnapshot>,
) -> String {
    let reward_str = reward.map(format_reward).unwrap_or_default();
    format!(
        "# Dream: Memory Consolidation\n\n\
        You are performing a dream — a reflective pass over the memory files. \
        Synthesize what you have learned into durable, well-organized memories \
        so that future sessions can orient quickly.\n\n\
        Memory directory: {memory_root}\n\n\
        ## Current index ({INDEX_FILE})\n\n{index_text}\n\n\
        ## Current memories ({count})\n\n{listing}\n\
        ## Recall frequency (advisory)\n\n{stats}\n\
        {reward_str}\
        ---\n\n\
        ## Phase 1 — Orient\n\n\
        - Read the index and the listing above to see what already exists.\n\
        - For any candidate you might merge, update, or prune, call \
        memory_read with its key to read the full body before acting.\n\
        - Improve existing topic files rather than creating near-duplicates.\n\n\
        ## Phase 2 — Consolidate\n\n\
        For each thing worth remembering, call memory_write to write or update \
        a topic file. Reusing a key refreshes that entry. Focus on:\n\
        - Merging near-duplicate entries into one canonical topic and passing \
        the superseded keys in the replaces field so their counters merge and \
        the old files move to the archive directory.\n\
        - Converting relative dates (yesterday, last week) to absolute dates \
        so they remain interpretable after time passes.\n\
        - Fixing contradicted facts — if today's investigation disproves an \
        old memory, fix it at the source (update the entry, or delete it in \
        Phase 3).\n\n\
        ## Phase 3 — Prune\n\n\
        - Call memory_delete for entries that are contradicted by the \
        current code or project state, or dead weight (zero recall hits + \
        never recalled, or not recalled for a long time — see the recall \
        frequency block). Call memory_archive for completed project notes \
        that should leave the active index while remaining restorable.\n\
        - Keep the index tight: each entry is one line, with detail in the \
        topic file. The index regenerates from the topic files after the \
        dream, so do not edit {INDEX_FILE} directly — just delete, archive, \
        or write topics.\n\n\
        ## Phase 4 — Scope flow\n\n\
        Promotion, demotion, and decay archival run deterministically in the \
        host based on recall hits, occurrences, and gate violations.\n\
        - When refreshing a topic that already lives in the project scope, \
        call memory_write with the updated content: the host updates that \
        project entry in place rather than writing a duplicate in auto.\n\
        - When consolidating several granular feedback rules into one rule, \
        pass the superseded keys in replaces so the merged rule inherits \
        their accumulated counts for host promotion.\n\n\
        ---\n\n\
        Return a brief summary of what you consolidated, updated, or pruned. \
        If nothing changed (memories are already tight), say so.",
        count = listing.len(),
        listing = format_listing(listing),
        stats = format_recall_stats(stats, now_secs),
        reward_str = reward_str,
    )
}

/// Build an isolated consolidation runner with memory management tools.
/// Recall stays disabled because the consolidation prompt contains the index.
pub(crate) fn build_forked_dream_runner(
    store: Arc<dyn SessionLog>,
    provider: Arc<dyn ModelProvider>,
    memory: Arc<dyn MemoryProvider>,
    cwd: &Path,
    config: RunnerConfig,
    recorder: Arc<MutationLog>,
) -> Runner {
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(
        MemoryWriteTool::new(memory.clone())
            .with_recorder(recorder.clone())
            .with_origin(MemoryOrigin::Dream)
            .with_pinned_scope(MemoryScope::Auto),
    ));
    tools.register(Arc::new(MemoryReadTool::new(memory.clone())));
    tools.register(Arc::new(
        MemoryDeleteTool::new_pinned(memory.clone(), MemoryScope::Auto)
            .with_recorder(recorder.clone()),
    ));
    tools.register(Arc::new(
        MemoryArchiveTool::new_pinned(memory.clone(), MemoryScope::Auto).with_recorder(recorder),
    ));
    Runner::new(store, provider, tools, config).with_cwd(cwd.to_path_buf())
}

/// Drive a forked consolidation run on a fresh ephemeral session. No prefix
/// — the dream is pure consolidation (the Stop-hook extractor plus
/// before-compact markers already gather new signal). Returns the run
/// result. Bounded by the config max_turns. The recorder is drained after
/// the run so the caller reads the exact successful operations.
pub(crate) async fn run_forked_dream(
    store: Arc<dyn SessionLog>,
    provider: Arc<dyn ModelProvider>,
    memory: Arc<dyn MemoryProvider>,
    cwd: &Path,
    config: RunnerConfig,
    prompt: &str,
    recorder: Arc<MutationLog>,
) -> Result<RunResult, RunError> {
    let runner = build_forked_dream_runner(store, provider, memory, cwd, config, recorder);
    let session = SessionId::new();
    runner.run_forked(session, &[], prompt.to_string()).await
}

/// The consolidation dream: a gate around the forked run. Holds the shared
/// provider, memory, ephemeral store, and config so it can drive a forked
/// dream on demand. The scan throttle + in-progress flag are in-memory
/// (per-process); the lock file carries across processes.
pub struct DreamRunner {
    store: Arc<dyn SessionLog>,
    provider: Arc<dyn ModelProvider>,
    memory: Arc<dyn MemoryProvider>,
    cwd: PathBuf,
    config: RunnerConfig,
    in_progress: Mutex<bool>,
    in_flight: Mutex<Vec<JoinHandle<()>>>,
    last_scan_at: Mutex<u64>,
    /// Blind-retry count already handed to a reward dream. The in-memory
    /// snapshot counts retries cumulatively for the session, so without this
    /// watermark every later turn re-arms the reward gate on the same retries
    /// and fires an empty consolidation pass.
    consumed_retries: Mutex<u32>,
    memory_changed: Mutex<Option<Arc<dyn EventHandler<MemoryChangedEvent>>>>,
    /// The durable session log root the session runs against. Reported for
    /// diagnostics and assembly checks; consolidation no longer reads other
    /// sessions' logs, because a retry total without the failures behind it
    /// cannot be turned into a lesson.
    pub(crate) session_log_root: Option<PathBuf>,
}

impl DreamRunner {
    /// Construct with the shared handles. The store is an ephemeral
    /// in-memory backend so the dream transcript never lands in the durable
    /// main log. The provider and memory are shared with the main runner so
    /// prompt caching and the in-process write lock carry over.
    pub fn new(
        store: Arc<dyn SessionLog>,
        provider: Arc<dyn ModelProvider>,
        memory: Arc<dyn MemoryProvider>,
        cwd: PathBuf,
        config: RunnerConfig,
    ) -> Self {
        Self {
            store,
            provider,
            memory,
            cwd,
            config,
            in_progress: Mutex::new(false),
            in_flight: Mutex::new(Vec::new()),
            last_scan_at: Mutex::new(0),
            consumed_retries: Mutex::new(0),
            memory_changed: Mutex::new(None),
            session_log_root: None,
        }
    }

    /// Install the durable session log root for cross-session reward
    /// scanning. When set, execute_dream scans previous sessions'
    /// RewardObservation events and merges the cross-session
    /// retry_after_error into the current snapshot. None in tests.
    pub fn with_session_log_root(mut self, root: PathBuf) -> Self {
        self.session_log_root = Some(root);
        self
    }

    /// Install the handler for successful memory changes.
    pub(crate) fn set_memory_changed_handler(
        &self,
        handler: Option<Arc<dyn EventHandler<MemoryChangedEvent>>>,
    ) {
        *self.memory_changed.lock().expect("memory handler lock") = handler;
    }

    fn emit_changes(&self, changes: Vec<MemoryChange>) {
        if changes.is_empty() {
            return;
        }
        let handler = self
            .memory_changed
            .lock()
            .expect("memory handler lock")
            .clone();
        if let Some(handler) = handler {
            handler.handle(MemoryChangedEvent {
                id: MemoryChangeId::new(),
                origin: MemoryChangeOrigin::AutoDream,
                // A dream runs on a session of its own, so it never belongs
                // to the turn on screen.
                causality: MemoryChangeCausality::PreviousTurn,
                changes,
            });
        }
    }

    /// Evaluate the consolidation gates and claim the running flag when open.
    fn open_dream_gate(&self, reward: Option<&RewardSnapshot>) -> Option<(String, bool)> {
        let root = self.memory.memory_root();
        if root.is_empty() {
            return None;
        }
        let now = now_secs();
        let lock = ConsolidationLock::new(Path::new(&root));
        let last_at = lock.last_consolidated_at();
        let elapsed = now.saturating_sub(last_at);
        let new_count = self.memory.count_new_since(last_at);
        let base = elapsed >= DEFAULT_MIN_HOURS * 3600 && new_count >= MIN_NEW_MEMORIES;
        let early = elapsed >= EARLY_FIRE_SECS && new_count >= EARLY_FIRE_NEW;
        let consumed = *self.consumed_retries.lock().expect("consumed retries");
        let reward_ready = reward.is_some_and(|r| {
            !r.failures.is_empty()
                && r.retry_after_error.saturating_sub(consumed) >= REWARD_DREAM_RETRY_THRESHOLD
        });
        let gate_msg = format!(
            "retry_after_error={} fails={} redundant={} new={} elapsed={} base={} early={} reward_ready={}",
            reward.map(|r| r.retry_after_error).unwrap_or(0),
            reward
                .map(|r| r.failures.iter().map(|f| f.fail_count).sum::<u32>())
                .unwrap_or(0),
            reward.map(|r| r.redundant.len()).unwrap_or(0),
            new_count,
            elapsed,
            base,
            early,
            reward_ready,
        );
        tracing::debug!("{gate_msg}");
        let last_scan = *self.last_scan_at.lock().expect("last_scan_at");
        let reward_path =
            reward_ready && now.saturating_sub(last_scan) >= REWARD_DREAM_MIN_INTERVAL_SECS;
        let watermark = !base
            && !early
            && !reward_path
            && now.saturating_sub(last_scan) >= 1_800
            && self.memory.count_new_since(0) > 120;
        if !base && !early && !reward_path && !watermark {
            return None;
        }
        if now.saturating_sub(last_scan) < SCAN_INTERVAL_SECS {
            return None;
        }
        let mut ip = self.in_progress.lock().expect("in_progress");
        if *ip {
            return None;
        }
        *ip = true;
        drop(ip);
        *self.last_scan_at.lock().expect("last_scan_at") = now;
        let is_reward_dream = reward_path && !base && !early && !watermark;
        Some((root, is_reward_dream))
    }

    /// Run consolidation inline inside the caller's serial pipeline and append
    /// any mutations to shared_log instead of emitting a separate notice.
    /// Returns true when the dream made at least one mutation.
    pub(crate) async fn run_consolidation(
        self: &Arc<Self>,
        reward: Option<RewardSnapshot>,
        shared_log: &MutationLog,
    ) -> bool {
        let Some((root, is_reward_dream)) = self.open_dream_gate(reward.as_ref()) else {
            return false;
        };
        let changes = Arc::clone(self)
            .collect_dream_changes(root, reward, is_reward_dream)
            .await;
        let changed = !changes.is_empty();
        shared_log.extend(changes);
        changed
    }

    /// Start a standalone consolidation task when the gates open. Coalesces
    /// when a consolidation pass is already running.
    pub fn execute_dream(self: &Arc<Self>, reward: Option<RewardSnapshot>) {
        let Some((root, is_reward_dream)) = self.open_dream_gate(reward.as_ref()) else {
            return;
        };
        let me = Arc::clone(self);
        let handle = tokio::spawn(async move {
            me.run_dream_body(root, reward, is_reward_dream).await;
        });
        let mut in_flight = self.in_flight.lock().expect("in_flight");
        in_flight.retain(|h| !h.is_finished());
        in_flight.push(handle);
    }

    /// Run the consolidation body and emit a standalone notice when changes
    /// landed.
    pub(crate) async fn run_dream_body(
        self: Arc<Self>,
        root: String,
        reward: Option<RewardSnapshot>,
        is_reward_dream: bool,
    ) {
        let me = Arc::clone(&self);
        let changes = self
            .collect_dream_changes(root, reward, is_reward_dream)
            .await;
        me.emit_changes(changes);
    }

    /// Execute one consolidation pass under the lock and return the recorded
    /// mutations without emitting a notice. Boxed to break the compile-time
    /// Send cycle through the forked runner's lifecycle.
    fn collect_dream_changes(
        self: Arc<Self>,
        root: String,
        reward: Option<RewardSnapshot>,
        is_reward_dream: bool,
    ) -> PFut<'static, Vec<MemoryChange>> {
        Box::pin(async move {
            let _guard = InProgressGuard::new(&self.in_progress);
            let lock = ConsolidationLock::new(Path::new(&root));
            let Some(mut claim) = lock.try_acquire() else {
                return Vec::new();
            };
            drop(self.memory.rebuild_if_stale());
            let listing = self.memory.list_memories();
            let fork_msg = format!(
                "listing={} is_reward={} reward_fails={} retry_after_error={}",
                listing.len(),
                is_reward_dream,
                reward
                    .as_ref()
                    .map(|r| r.failures.iter().map(|f| f.fail_count).sum::<u32>())
                    .unwrap_or(0),
                reward.as_ref().map(|r| r.retry_after_error).unwrap_or(0),
            );
            tracing::debug!("{fork_msg}");
            let stats = self.memory.read_recall_stats();
            let index_text = read_to_string(Path::new(&root).join(INDEX_FILE)).unwrap_or_default();
            let prompt = if is_reward_dream {
                build_reward_lesson_prompt(
                    &root,
                    &listing,
                    &index_text,
                    reward.as_ref().expect("reward dream has a snapshot"),
                )
            } else {
                build_consolidation_prompt(
                    &root,
                    &listing,
                    &index_text,
                    &stats,
                    now_secs(),
                    reward.as_ref(),
                )
            };
            let recorder = Arc::new(MutationLog::new());
            let config = if is_reward_dream {
                let mut c = self.config.clone();
                c.max_turns = REWARD_DREAM_MAX_TURNS;
                c
            } else {
                self.config.clone()
            };
            let result = run_forked_dream(
                Arc::clone(&self.store),
                Arc::clone(&self.provider),
                Arc::clone(&self.memory),
                &self.cwd,
                config,
                &prompt,
                Arc::clone(&recorder),
            )
            .await;
            match result {
                Ok(r) => {
                    let dream_msg = format!("dream outcome: {:?}", r.outcome);
                    tracing::debug!("{dream_msg}");
                    if is_reward_dream {
                        if let Some(snapshot) = reward.as_ref() {
                            *self.consumed_retries.lock().expect("consumed retries") =
                                snapshot.retry_after_error;
                        }
                    } else {
                        claim.record_consolidation();
                    }
                    drop(self.memory.rebuild_index());
                    recorder.take()
                }
                Err(e) => {
                    let fork_err = format!("auto-dream fork failed: {e}");
                    tracing::debug!("{fork_err}");
                    Vec::new()
                }
            }
        })
    }

    /// Drain in-flight dream tasks before shutdown. Awaits every spawned
    /// handle up to the timeout; on timeout the remaining handles are
    /// dropped (detached) so the caller can proceed. No-op when nothing is
    /// in flight.
    pub async fn drain_pending(&self, timeout: Duration) {
        let handles: Vec<JoinHandle<()>> = take(&mut *self.in_flight.lock().expect("in_flight"));
        if handles.is_empty() {
            return;
        }
        let mut deadline = Box::pin(sleep(timeout));
        for handle in handles {
            tokio::select! {
                _ = handle => {}
                _ = &mut deadline => return,
            }
        }
    }
}

#[cfg(test)]
mod tests;
