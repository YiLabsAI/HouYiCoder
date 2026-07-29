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
//! prior dream failed: the lock mtime rewinds on failure so the gate keeps
//! passing, and the throttle prevents a retry storm.
//!
//! The index is rebuilt in code before and after the LLM run, so the agent
//! sees a clean store on entry and the derived index regenerates from the
//! final topic set on exit — no hand-maintained index that can drift.
//!
//! Scope promote/demote: the forked agent calls promote_memory / demote_memory
//! to move a topic between the auto and project scopes; save_memory accepts a
//! scope parameter so a refresh of a project-scope entry lands in the project
//! root rather than shadowing it with a competing auto copy.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicU32;
use std::time::Duration;

use houyicoder_api::live::{LiveEvent, LiveSink, MemorySavedKind};
use houyicoder_api::memory::MemoryProvider;
use houyicoder_api::provider::ModelProvider;
use houyicoder_api::session::SessionLog;
use houyicoder_context::{MemoryRecallStats, MemorySummary, SessionId};
use tokio::task::JoinHandle;

use super::runner_config::RunnerConfig;
use super::tools::memory_add::MemoryAddTool;
use super::tools::memory_delete::DeleteMemoryTool;
use super::tools::memory_promote_demote::{DemoteMemoryTool, PromoteMemoryTool};
use super::tools::memory_show::ShowMemoryTool;
use super::{RunError, RunResult, Runner, ToolRegistry};

const LOCK_FILE: &str = ".consolidate-lock";
pub(crate) const INDEX_FILE: &str = "MEMORY.md";
/// A lock holder is considered stale past this even if its PID is live.
/// The forked agent has a maxTurns backstop, so no dream runs longer than
/// this window. Uses a time-based staleness check instead of a libc
/// PID-liveness probe (no libc dependency).
const HOLDER_STALE_SECS: u64 = 3600;
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
/// (the lock mtime rewinds on failure, so the time-gate keeps passing every
/// turn — the throttle prevents a retry storm).
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

/// The consolidation lock. mtime = lastConsolidatedAt; body = holder PID.
/// Relies on a stale guard (HOLDER_STALE_SECS) rather than a PID liveness
/// check — the forked agent has maxTurns as the backstop, so no dream runs
/// longer than the stale window. This avoids a libc dependency.
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
        std::fs::metadata(&self.path)
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    /// Acquire: atomically create the lock file only if it does not exist.
    /// Returns the prior mtime (for rollback), or None if the lock is recent
    /// (held by another dream) or another process won the create race. A lock
    /// older than HOLDER_STALE_SECS is reclaimed (unlinked then create_new).
    ///
    /// The create_new closes the cross-process TOCTOU race that a plain
    /// stat-then-overwrite had (two processes both stat, both overwrite, both
    /// think they hold the lock). create_new is atomic for the absent-lock
    /// case (the common path). The stale-reclaim path (unlink then create_new)
    /// has a narrow race between the unlink and the create_new — two
    /// reclaimers both unlink, one wins create_new, the other bails on
    /// AlreadyExists. The consequences of both winning are self-healing
    /// (index drift healed by rebuild_if_stale; topic writes are atomic
    /// temp+rename), so the narrow window is acceptable.
    pub(crate) fn try_acquire(&self) -> Option<u64> {
        let prior = self.last_consolidated_at();
        let now = now_secs();
        if now.saturating_sub(prior) < HOLDER_STALE_SECS {
            return None;
        }
        if let Some(parent) = self.path.parent() {
            drop(std::fs::create_dir_all(parent));
        }
        // Stale reclaim: a prior lock past the stale window means the holder
        // died (maxTurns backstop is well under the stale window). Unlink it
        // so create_new can land. Absent lock (prior == 0) skips the unlink.
        if prior > 0 {
            drop(std::fs::remove_file(&self.path));
        }
        let pid = std::process::id();
        match std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&self.path)
        {
            Ok(mut f) => {
                use std::io::Write;
                drop(f.write_all(pid.to_string().as_bytes()));
                drop(f.flush());
                Some(prior)
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => None,
            Err(_) => None,
        }
    }

    /// Rewind after a failed dream. prior 0 (no prior lock) → unlink so the
    /// time-gate passes from a clean state. prior > 0 → restore the lock
    /// file with its mtime rewound to the pre-acquire value, so the
    /// time-gate passes again but the scan throttle gates the retry. Uses
    /// std FileTimes, no libc.
    pub(crate) fn rollback(&self, prior: u64) {
        if prior == 0 {
            drop(std::fs::remove_file(&self.path));
            return;
        }
        // Recreate the file and rewind its mtime so last_consolidated_at
        // returns the pre-acquire value.
        drop(std::fs::write(&self.path, String::new()));
        let times = std::fs::FileTimes::new()
            .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(prior));
        if let Ok(file) = std::fs::OpenOptions::new().write(true).open(&self.path) {
            drop(file.set_times(times));
        }
    }

    /// Stamp after a successful dream (write PID, mtime = now).
    pub(crate) fn record_consolidation(&self) {
        let pid = std::process::id();
        drop(std::fs::write(&self.path, pid.to_string()));
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
}

impl<'a> InProgressGuard<'a> {
    pub(crate) fn new(flag: &'a Mutex<bool>) -> Self {
        Self { flag }
    }
}

impl Drop for InProgressGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut ip) = self.flag.lock() {
            *ip = false;
        }
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Format the memory listing as a bulleted block for the prompt. Each entry
/// is one line (key, source, description, mtime) so the agent can scan the
/// whole store in one read and decide what to inspect further.
pub(crate) fn format_listing(listing: &[MemorySummary]) -> String {
    if listing.is_empty() {
        return "(no memories yet — orient from the index below if present)".to_string();
    }
    let mut out = String::new();
    for m in listing.iter().take(200) {
        out.push_str(&format!(
            "- {} [{}] mtime={}: {}\n",
            m.key,
            m.source.as_label(),
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
/// candidates — the rule should move into the always-on carrier so it stops
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

/// Build the consolidation prompt. The current listing plus the MEMORY.md
/// index are injected so the Orient phase reads from the prompt (no tool
/// round-trip just to see what exists). Four phases: Orient, Consolidate,
/// Merge cross-session retry_after_error from the durable trajectory into
/// the reward snapshot. When the session log root is set, scans previous
/// sessions' RewardObservation events (excluding the current session, whose
/// count already lives in the in-memory snapshot) and ADDS the cumulative
/// cross-session count to the current snapshot. Adding — not replacing —
/// keeps the current session's in-memory count (which may be ahead of the
/// flushed log) and layers the cross-session trend on top, without
/// double-counting the current session. Best-effort: a missing root or
/// unreadable logs add nothing (falls back to in-memory only).
pub(crate) fn merge_cross_session_reward(
    session_log_root: &Option<PathBuf>,
    current_session: Option<&str>,
    reward: &mut Option<crate::agent::reward_snapshot::RewardSnapshot>,
) {
    if let Some(log_root) = session_log_root {
        let cross = crate::agent::durable_scan::scan_cross_session_retry(log_root, current_session);
        if cross > 0
            && let Some(snap) = reward.as_mut()
        {
            snap.retry_after_error = snap.retry_after_error.saturating_add(cross);
        }
    }
}

/// Build the consolidation prompt for the forked dream agent. Four phases:
/// Orient (scan the memory store + recall-frequency stats), Consolidate
/// (merge near-duplicates, resolve contradictions), Prune (drop dead weight
/// surfaced by the recall-frequency stats via delete_memory), and Scope-flow
/// (promote high-recall entries into the always-on carrier so the rule no
/// longer relies on recall luck).
pub(crate) fn build_consolidation_prompt(
    memory_root: &str,
    listing: &[MemorySummary],
    index_text: &str,
    stats: &[MemoryRecallStats],
    now_secs: u64,
    reward: Option<&crate::agent::reward_snapshot::RewardSnapshot>,
) -> String {
    let reward_str = reward
        .map(crate::agent::reward_snapshot::format_reward)
        .unwrap_or_default();
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
        - For any candidate you might merge, update, prune, or promote, call \
        show_memory with its key to read the full body before acting.\n\
        - Improve existing topic files rather than creating near-duplicates.\n\n\
        ## Phase 2 — Consolidate\n\n\
        For each thing worth remembering, call save_memory to write or update \
        a topic file. Reusing a key refreshes that entry. Focus on:\n\
        - Merging new signal into existing topic files rather than creating \
        near-duplicates.\n\
        - Converting relative dates (yesterday, last week) to absolute dates \
        so they remain interpretable after time passes.\n\
        - Fixing contradicted facts — if today's investigation disproves an \
        old memory, fix it at the source (update the entry, or delete it in \
        Phase 3).\n\n\
        ## Phase 3 — Prune\n\n\
        - Call delete_memory for entries that are stale, contradicted by the \
        current code or project state, superseded by a merged successor, or \
        dead weight (zero recall hits + never recalled, or not recalled for \
        a long time — see the recall frequency block). Deletion is permanent \
        but reversible by saving the same key again.\n\
        - Keep the index tight: each entry is one line, with detail in the \
        topic file. The index regenerates from the topic files after the \
        dream, so do not edit {INDEX_FILE} directly — just delete or save \
        topics.\n\n\
        ## Phase 4 — Scope flow\n\n\
        Rules that have crossed the promotion threshold move from the \
        recall-on-demand auto scope into the always-on project carrier; rules \
        that have decayed move back. Use the recall-frequency block to decide:\n\
        - Call promote_memory for rules with high recall frequency (the rule \
        is referenced often, so it should be always-on rather than rely on \
        recall luck) or for rules you see cited repeatedly across the listing. \
        Promotion merges the rule's first line into the project memory file \
        (agent.md) so the rule is always-on, and moves the topic into the \
        project memory dir so it is still recallable.\n\
        - Call promote_memory for rules with a high gate-violations count \
        (the agent keeps violating a PreToolUse gate on that rule — the rule \
        is missing from the always-on carrier or recall is missing it, so it \
        should be promoted so the agent sees it before acting). This closes \
        the loop where a rule the agent keeps violating finally stops relying \
        on recall luck.\n\
        - Call demote_memory for rules that have decayed: long unstirred in \
        recall and no recent gate violations. Demotion removes the rule line \
        from the always-on carrier (freeing prefix budget) and moves the \
        topic back into the auto scope so it is recall-on-demand only.\n\
        - When refreshing a topic that is already in the project scope (one \
        you promoted earlier), pass scope=project to save_memory so the \
        refresh lands in the project dir rather than shadowing the explicit \
        entry with a competing auto copy.\n\n\
        ---\n\n\
        Return a brief summary of what you consolidated, updated, pruned, \
        promoted, or demoted. If nothing changed (memories are already tight), \
        say so.",
        count = listing.len(),
        listing = format_listing(listing),
        stats = format_recall_stats(stats, now_secs),
        reward_str = reward_str,
    )
}

/// Build a forked dream runner over a caller-provided ephemeral store. The
/// store is ephemeral (an in-memory backend the caller constructs) so the
/// forked dream transcript stays out of the durable main log. The provider
/// and memory are shared (Arc clone) so prompt caching and the in-process
/// write lock carry over. The tool set is the five structured memory tools:
/// save_memory (write/update, scope-aware), show_memory (read one body),
/// delete_memory (prune), promote_memory (auto -> project, always-on), and
/// demote_memory (project -> auto, recall-on-demand). The runner is NOT
/// wired with with_memory — the dream prompt is self-contained (listing
/// plus index injected) and recall injection is a main-loop feature that
/// would add noise here; run_forked does not inject recall regardless.
pub fn build_forked_dream_runner(
    store: Arc<dyn SessionLog>,
    provider: Arc<dyn ModelProvider>,
    memory: Arc<dyn MemoryProvider>,
    cwd: &Path,
    config: RunnerConfig,
    counter: Arc<AtomicU32>,
) -> Runner {
    // The add + delete + promote + demote tools share one counter so a
    // touch (add, delete, promote, or demote) counts toward the notice
    // (the consolidation dream both writes new entries, prunes stale ones,
    // and flows rules between scopes).
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(
        MemoryAddTool::new(memory.clone())
            .with_counter(counter.clone())
            .with_origin(houyicoder_context::MemoryOrigin::Dream),
    ));
    tools.register(Arc::new(ShowMemoryTool::new(memory.clone())));
    tools.register(Arc::new(
        DeleteMemoryTool::new(memory.clone()).with_counter(counter.clone()),
    ));
    tools.register(Arc::new(
        PromoteMemoryTool::new(memory.clone()).with_counter(counter.clone()),
    ));
    tools.register(Arc::new(
        DemoteMemoryTool::new(memory.clone()).with_counter(counter),
    ));
    Runner::new(store, provider, tools, config).with_cwd(cwd.to_path_buf())
}

/// Drive a forked consolidation run on a fresh ephemeral session. No prefix
/// — the dream is pure consolidation (the Stop-hook extractor plus
/// before-compact markers already gather new signal). Returns the run
/// result. Bounded by the config max_turns. The counter is reset before the
/// run + bumped per touch so the caller reads it after.
pub async fn run_forked_dream(
    store: Arc<dyn SessionLog>,
    provider: Arc<dyn ModelProvider>,
    memory: Arc<dyn MemoryProvider>,
    cwd: &Path,
    config: RunnerConfig,
    prompt: &str,
    counter: Arc<AtomicU32>,
) -> Result<RunResult, RunError> {
    counter.store(0, std::sync::atomic::Ordering::SeqCst);
    let runner = build_forked_dream_runner(store, provider, memory, cwd, config, counter);
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
    /// Host-installed sink fired once per consolidation pass that touched
    /// memories. None when no host wires one (tests, forked runners).
    notify_sink: Mutex<Option<LiveSink>>,
    /// The durable session log root, so the dream can scan previous
    /// sessions' RewardObservation events for cross-session retry
    /// patterns. None in tests or when the session log root is not
    /// known (the dream falls back to the in-memory snapshot only).
    session_log_root: Option<PathBuf>,
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
            notify_sink: Mutex::new(None),
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

    /// Install the host sink fired when a consolidation pass touches
    /// memories. The runner forwards its own live sink here so the dream (a
    /// detached spawned task) can push a MemorySaved event without holding a
    /// wire handle. The forked runner's live sink stays None so the fork's
    /// token deltas do not fire into the user transcript.
    pub fn set_notify_sink(&self, sink: LiveSink) {
        *self.notify_sink.lock().expect("notify_sink") = Some(sink);
    }

    /// Fire one MemorySaved notice if a sink is wired + count > 0. Best-effort:
    /// a None sink (tests, forked runner) is a no-op.
    fn fire_saved(&self, count: u32) {
        if count == 0 {
            return;
        }
        let sink = self.notify_sink.lock().expect("notify_sink").clone();
        if let Some(sink) = sink {
            sink(&LiveEvent::MemorySaved {
                count,
                kind: MemorySavedKind::Consolidated,
            });
        }
    }

    /// The fire-and-forget entry point. Called from the main runner at
    /// FinalOutput (alongside the Stop-hook extractor). Per-turn cost when
    /// the gate is closed: one stat (read the lock mtime). When the gate
    /// opens: spawn a forked agent (async, non-blocking) to run the
    /// consolidation. Coalesces if a dream is already in flight.
    pub fn execute_dream(
        self: &Arc<Self>,
        mut reward: Option<crate::agent::reward_snapshot::RewardSnapshot>,
        current_session: Option<&str>,
    ) {
        let root = self.memory.memory_root();
        if root.is_empty() {
            return;
        }
        // Cross-session scan: merge previous sessions' retry_after_error
        // from the durable trajectory so the dream sees patterns that span
        // sessions, not just the current one. The current session is
        // skipped (its count is already in the in-memory snapshot).
        // Best-effort: a missing root or unreadable logs return 0 (falls
        // back to in-memory only).
        merge_cross_session_reward(&self.session_log_root, current_session, &mut reward);
        let now = now_secs();
        let lock = ConsolidationLock::new(Path::new(&root));
        let last_at = lock.last_consolidated_at();
        let elapsed = now.saturating_sub(last_at);
        // Memory-delta gate: fire only when new memories landed since the last
        // dream. Two paths — the base (full interval plus a few new memories)
        // or an early fire (a burst of new memories past a shorter floor).
        // last_at is the lock mtime, which record_consolidation stamps at the
        // END of a successful dream, so the dream's own promote/demote writes
        // (mtime <= last_at) do not inflate the count. A quiet store never
        // fires, so a forked LLM run is not spent re-organizing stable content.
        let new_count = self.memory.count_new_since(last_at);
        let base = elapsed >= DEFAULT_MIN_HOURS * 3600 && new_count >= MIN_NEW_MEMORIES;
        let early = elapsed >= EARLY_FIRE_SECS && new_count >= EARLY_FIRE_NEW;
        let reward_ready = reward
            .as_ref()
            .is_some_and(|r| r.retry_after_error >= REWARD_DREAM_RETRY_THRESHOLD);
        let gate_msg = format!(
            "retry_after_error={} fails={} redundant={} new={} elapsed={} base={} early={} reward_ready={}",
            reward.as_ref().map(|r| r.retry_after_error).unwrap_or(0),
            reward
                .as_ref()
                .map(|r| r.failures.iter().map(|f| f.fail_count).sum::<u32>())
                .unwrap_or(0),
            reward.as_ref().map(|r| r.redundant.len()).unwrap_or(0),
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
        if !base && !early && !reward_path {
            return;
        }
        // Scan throttle: the time-gate has passed, but do not attempt the
        // lock more often than the throttle window. This is the retry
        // backoff when a prior dream failed (the lock mtime rewinds, so the
        // time-gate keeps passing every turn).
        if now.saturating_sub(last_scan) < SCAN_INTERVAL_SECS {
            return;
        }
        // Coalesce: if a dream is in flight, drop this trigger (the
        // in-flight dream will see whatever it sees; a second concurrent
        // dream would double-stamp the lock and double-write the store).
        let mut ip = self.in_progress.lock().expect("in_progress");
        if *ip {
            return;
        }
        // Setting in_progress synchronously here (not in the spawned body)
        // closes the race where a concurrent trigger would see
        // in_progress=false between the check and the spawned task arming it.
        *ip = true;
        drop(ip);
        *self.last_scan_at.lock().expect("last_scan_at") = now;
        let is_reward_dream = reward_path && !base && !early;
        let me = Arc::clone(self);
        let handle = tokio::spawn(async move {
            me.run_dream_body(root, reward, is_reward_dream).await;
        });
        let mut in_flight = self.in_flight.lock().expect("in_flight");
        // Prune completed handles so the Vec does not grow unboundedly across
        // a long-running process (drain_pending is shutdown-only). is_finished
        // is a non-blocking poll; detached-but-complete tasks are reaped here.
        in_flight.retain(|h| !h.is_finished());
        in_flight.push(handle);
    }

    /// The spawned body: deterministic pre-pass, build the prompt, run the
    /// forked agent, then stamp the lock on success or rewind on failure.
    /// in_progress clears in the finally (no trailing pickup: the dream is a
    /// single shot, unlike extraction which coalesces a trailing context).
    pub(crate) async fn run_dream_body(
        self: Arc<Self>,
        root: String,
        reward: Option<crate::agent::reward_snapshot::RewardSnapshot>,
        is_reward_dream: bool,
    ) {
        // Reset in_progress on scope exit, including a panic unwinding this
        // task. Without the guard, a panic below would wedge the flag true.
        let _guard = InProgressGuard::new(&self.in_progress);
        let lock = ConsolidationLock::new(Path::new(&root));
        // Reward-dream skips the dream-lock so the 24h full dream's mtime
        // gate is unaffected; in_progress + last_scan throttle bound it
        // within one process. The full dream acquires the cross-process lock.
        let prior = if is_reward_dream {
            0
        } else {
            match lock.try_acquire() {
                Some(p) => p,
                None => return,
            }
        };
        // Deterministic pre-pass: heal any drifted index so the LLM sees a
        // clean store (a deterministic heal saves the LLM turns it would
        // otherwise spend fixing the index).
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
        let index_text =
            std::fs::read_to_string(Path::new(&root).join(INDEX_FILE)).unwrap_or_default();
        let prompt = if is_reward_dream {
            // Reward dream: focused lesson extraction from blind retries,
            // not a full consolidation pass. Uses a dedicated prompt that
            // removes the "if nothing changed, say so" blanket exit and
            // scopes out consolidation/prune/scope-flow phases.
            crate::agent::prompt::reward_lesson::build_reward_lesson_prompt(
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
        // A fresh counter the forked add/delete tools bump per touch. Reset
        // before the run so the load after is this pass's count.
        let counter = Arc::new(AtomicU32::new(0));
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
            Arc::clone(&counter),
        )
        .await;
        match result {
            Ok(r) => {
                let dream_msg = format!("dream outcome: {:?}", r.outcome);
                tracing::debug!("{dream_msg}");
                if !is_reward_dream {
                    lock.record_consolidation();
                }
                // Deterministic post-pass: regenerate the index from the
                // final topic set (a deterministic regenerate saves the LLM
                // maintaining it by hand).
                drop(self.memory.rebuild_index());
                // Fire one notice if the dream touched any memories this
                // pass (an Ok run that wrote nothing is not worth a notice).
                self.fire_saved(counter.load(std::sync::atomic::Ordering::SeqCst));
            }
            Err(e) => {
                let fork_err = format!("auto-dream fork failed: {e}");
                tracing::debug!("{fork_err}");
                if !is_reward_dream {
                    lock.rollback(prior);
                }
            }
        }
        // _guard drops here, resetting in_progress (also on panic).
    }

    /// Drain in-flight dream tasks before shutdown. Awaits every spawned
    /// handle up to the timeout; on timeout the remaining handles are
    /// dropped (detached) so the caller can proceed. No-op when nothing is
    /// in flight.
    pub async fn drain_pending(&self, timeout: Duration) {
        let handles: Vec<JoinHandle<()>> =
            std::mem::take(&mut *self.in_flight.lock().expect("in_flight"));
        if handles.is_empty() {
            return;
        }
        let mut deadline = Box::pin(tokio::time::sleep(timeout));
        for handle in handles {
            tokio::select! {
                _ = handle => {}
                _ = &mut deadline => return,
            }
        }
    }
}

#[cfg(test)]
#[path = "auto_dream_tests.rs"]
mod auto_dream_tests;
