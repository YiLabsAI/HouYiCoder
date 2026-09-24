//! The memory extractor: a cursor and mutual-exclusion gate around the
//! forked extraction run.
//!
//! The cursor marks the last event an earlier pass consumed; the next pass
//! cuts an exact window at query boundaries, so the fork reads only whole
//! turns it has not yet seen. A cursor that does not resolve in the snapshot
//! skips the pass and re-seeds to the snapshot tail, never widening to the
//! full history.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use houyicoder_api::agent_event::{
    EventHandler, MemoryChange, MemoryChangeCausality, MemoryChangeOrigin, MemoryChangedEvent,
};
use houyicoder_api::memory::MemoryProvider;
use houyicoder_api::provider::ModelProvider;
use houyicoder_api::session::SessionLog;
use houyicoder_context::{EventId, MemoryChangeId, SessionEvent, SessionId, SessionLogEntry};
use tokio::task::JoinHandle;

use super::extract::run_forked_extract;
use super::memory::MutationLog;
use super::{RunError, RunResult, RunnerConfig};

/// The evidence-boundary type the extractor cuts and the forked run reads.
#[path = "extraction_window.rs"]
pub(crate) mod extraction_window;
use extraction_window::ExactExtractionWindow;

#[derive(Debug)]
pub enum ExtractOutcome {
    /// The forked agent ran to completion.
    Extracted(RunResult),
    /// The fork was skipped: the main agent already saved a memory in this
    /// range, the unconsumed tail held no complete query turn, or the cursor
    /// could not be located in the snapshot.
    Skipped(ExtractSkip),
}

/// Why a pass wrote nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtractSkip {
    /// The main agent already saved a memory in this range.
    PrimaryWrote,
    /// The unconsumed tail holds no complete query turn.
    NoQueryTurn,
    /// The cursor names an event this snapshot does not hold.
    CursorLost,
}

/// The two logs an extraction pass reads and writes: the durable session
/// whose turn frontier labels the change, and the forked run's own log.
/// Named fields keep a construction site stating which handle is which
/// instead of relying on argument order; transposing them leaves the frontier
/// read unanswerable and every notice reading as an earlier turn.
pub struct ExtractionLogs {
    /// The durable session log; the frontier read runs against it.
    pub session: Arc<dyn SessionLog>,
    /// The forked run's log. Isolated from the durable log so a pass never
    /// appends to the user's history.
    pub fork: Arc<dyn SessionLog>,
}

/// The memory extractor: cursor and mutex gate around the forked run. Holds
/// the shared provider, memory, logs, and config so it can drive a forked
/// extraction on demand. The cursor is an in-memory Option of the last
/// consumed message id; it advances on a successful run and on a
/// mutual-exclusion skip, but NOT on error (errored messages are reconsidered
/// next pass) and not on a zero-window skip (the cursor already covers the
/// range).
pub struct MemoryExtractor {
    cursor: Mutex<Option<EventId>>,
    in_progress: Mutex<bool>,
    pending_context: Mutex<Option<Vec<SessionLogEntry>>>,
    in_flight: Mutex<Vec<JoinHandle<()>>>,
    logs: ExtractionLogs,
    provider: Arc<dyn ModelProvider>,
    memory: Arc<dyn MemoryProvider>,
    cwd: PathBuf,
    config: RunnerConfig,
    memory_changed: Mutex<Option<Arc<dyn EventHandler<MemoryChangedEvent>>>>,
}

impl MemoryExtractor {
    /// Construct with the shared handles. The provider and memory are shared
    /// with the main runner so prompt caching and the in-process write lock
    /// carry over.
    pub fn new(
        logs: ExtractionLogs,
        provider: Arc<dyn ModelProvider>,
        memory: Arc<dyn MemoryProvider>,
        cwd: PathBuf,
        config: RunnerConfig,
    ) -> Self {
        Self {
            cursor: Mutex::new(None),
            in_progress: Mutex::new(false),
            pending_context: Mutex::new(None),
            in_flight: Mutex::new(Vec::new()),
            logs,
            provider,
            memory,
            cwd,
            config,
            memory_changed: Mutex::new(None),
        }
    }

    /// Seed the cursor to a restored history's last event, but only while it
    /// is still None. A live extractor may already have consumed past the
    /// seed point; rewinding it would double-count that range.
    pub fn seed_cursor(&self, id: EventId) {
        let mut cursor = self.cursor.lock().expect("cursor");
        if cursor.is_none() {
            *cursor = Some(id);
        }
    }

    /// The last message id the extraction consumed or was seeded to, or
    /// None when neither has happened.
    pub fn cursor(&self) -> Option<EventId> {
        *self.cursor.lock().expect("cursor")
    }

    /// Install the handler for successful memory changes.
    pub(crate) fn set_memory_changed_handler(
        &self,
        handler: Option<Arc<dyn EventHandler<MemoryChangedEvent>>>,
    ) {
        *self.memory_changed.lock().expect("memory handler lock") = handler;
    }

    fn emit_changes(
        &self,
        origin: MemoryChangeOrigin,
        causality: MemoryChangeCausality,
        changes: Vec<MemoryChange>,
    ) {
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
                origin,
                causality,
                changes,
            });
        }
    }

    /// The latest durable user input for a session, or None when the session
    /// holds none. A pass classifies its trigger against this, so the read
    /// must reach the durable log rather than the forked run's own log.
    pub fn turn_frontier(&self, session: SessionId) -> Option<EventId> {
        self.logs.session.last_user_input_id(session)
    }

    /// Classify a pass against the durable user-input frontier: while the
    /// pass's trigger is still the latest prompt, its changes belong to the
    /// turn that just completed. A newer prompt means the user moved on, and
    /// the notice must not read as the turn now on screen. An unreadable
    /// frontier is reported as an earlier turn rather than claiming the
    /// current one.
    fn causality_for(&self, session: SessionId, trigger: EventId) -> MemoryChangeCausality {
        if self.turn_frontier(session) == Some(trigger) {
            MemoryChangeCausality::ThisTurn
        } else {
            MemoryChangeCausality::PreviousTurn
        }
    }

    /// Drive one extraction pass synchronously: the gating logic + the forked
    /// run + cursor advance. Returns the outcome so the fire-and-forget body
    /// (and tests) can inspect it. This is the per-pass body; the spawn
    /// wrapper, coalescing, and trailing pickup live in run_extraction.
    pub async fn run_extraction_once(
        &self,
        messages: &[SessionLogEntry],
    ) -> Result<ExtractOutcome, RunError> {
        let cursor = *self.cursor.lock().expect("cursor");
        let Some(tail) = ExactExtractionWindow::unconsumed(messages, cursor.as_ref()) else {
            // The cursor names an event this snapshot does not hold, so the
            // consumed prefix cannot be located. Widening to the full history
            // would hand the fork evidence an earlier pass already consumed;
            // leaving the cursor lost would end extraction for the session.
            // Re-seed to this snapshot's tail so the next pass has a located
            // range.
            tracing::warn!("extraction cursor does not resolve; skipping the pass");
            advance_cursor(&self.cursor, messages);
            return Ok(ExtractOutcome::Skipped(ExtractSkip::CursorLost));
        };
        // Skip the fork when the main agent already saved in this range:
        // re-extracting would duplicate work the user just triggered. The
        // dedup at write time still catches a duplicate, but the fork is
        // wasted, so a lightweight scan (does the tail hold any
        // save_memory call?) gates it. Primary-save notification no longer
        // lives here: the main runner records at call time and drains at the
        // run boundary.
        if primary_saved_in(tail) {
            advance_cursor(&self.cursor, messages);
            return Ok(ExtractOutcome::Skipped(ExtractSkip::PrimaryWrote));
        }
        let Some(window) = ExactExtractionWindow::from_unconsumed(tail) else {
            return Ok(ExtractOutcome::Skipped(ExtractSkip::NoQueryTurn));
        };
        tracing::debug!(
            session = %window.session(),
            trigger = %window.trigger_user_event(),
            from = %window.start_event(),
            to = %window.end_event(),
            eligible = window.model_visible_count(),
            "cut the extraction window"
        );
        let recorder = Arc::new(MutationLog::new());
        let result = run_forked_extract(
            Arc::clone(&self.logs.fork),
            Arc::clone(&self.provider),
            Arc::clone(&self.memory),
            &self.cwd,
            self.config.clone(),
            &window,
            Arc::clone(&recorder),
        )
        .await;
        // Advance the cursor only on success. On error the cursor stays so
        // the errored messages are reconsidered next pass.
        if result.is_ok() {
            advance_cursor(&self.cursor, messages);
            let causality = self.causality_for(window.session(), window.trigger_user_event());
            self.emit_changes(MemoryChangeOrigin::AutoMemory, causality, recorder.take());
        }
        result.map(ExtractOutcome::Extracted)
    }

    /// The fire-and-forget body: run one pass, then in the finally drain the
    /// stashed pending context (if a second trigger fired while this pass was
    /// in-flight) as a trailing run. in_progress stays true across the
    /// trailing chain — it is set false only when no more pending context
    /// remains, so a concurrent trigger during the chain coalesces (stashes)
    /// rather than spawning a concurrent fork. Boxed so the trailing
    /// recursion does not blow the future's size (Rust async recursion
    /// requires boxing).
    pub fn run_extraction(
        self: Arc<Self>,
        messages: Vec<SessionLogEntry>,
        is_trailing: bool,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        Box::pin(async move {
            // Guard the initial run only: in_progress stays true across the
            // trailing chain so concurrent triggers coalesce; the guard
            // resets it at the chain end OR if a panic unwinds this task
            // (run_extraction_once or the trailing recursion), which would
            // otherwise wedge the flag true forever. Trailing runs skip the
            // guard — they are inner to the initial's scope.
            let _guard = if !is_trailing {
                Some(super::auto_dream::InProgressGuard::new(&self.in_progress))
            } else {
                None
            };
            let _outcome = self.run_extraction_once(&messages).await;
            // finally: drain the stashed context. in_progress stays true across
            // the trailing chain so concurrent triggers coalesce; only set
            // false when nothing is left to run (the guard also resets on
            // scope exit, so this explicit set is the normal-path fast path).
            let pending = self.pending_context.lock().expect("pending").take();
            if let Some(trailing) = pending {
                Arc::clone(&self).run_extraction(trailing, true).await;
            } else {
                *self.in_progress.lock().expect("in_progress") = false;
            }
            // _guard drops here (initial only), resetting in_progress on panic.
        })
    }

    /// Fire-and-forget: coalesce if a fork is in-flight (stash the latest
    /// context, overwriting any older stash since only the latest matters),
    /// otherwise set in_progress and spawn the body. Setting in_progress
    /// synchronously here (not in the spawned body) closes the race where a
    /// concurrent trigger would see in_progress=false between the check and
    /// the spawned task arming it.
    pub fn extract_memories(self: &Arc<Self>, messages: Vec<SessionLogEntry>) {
        // Cheap pre-check: when the unconsumed tail holds no whole query
        // turn, skip the spawn and the in-progress churn — a re-emitted
        // FinalOutput after a verify retry, for example, has nothing new.
        // A cursor that does not resolve is the pass body's to recover, so
        // it never short-circuits here. The pass body re-checks, which is
        // what covers the trailing drain.
        let cursor = *self.cursor.lock().expect("cursor");
        if let Some(tail) = ExactExtractionWindow::unconsumed(&messages, cursor.as_ref())
            && ExactExtractionWindow::from_unconsumed(tail).is_none()
        {
            return;
        }
        let mut ip = self.in_progress.lock().expect("in_progress");
        if *ip {
            *self.pending_context.lock().expect("pending") = Some(messages);
            return;
        }
        *ip = true;
        drop(ip);
        let me = Arc::clone(self);
        let handle = tokio::spawn(async move {
            let _ = me.run_extraction(messages, false).await;
        });
        let mut in_flight = self.in_flight.lock().expect("in_flight");
        // Prune completed handles so the Vec does not grow unboundedly
        // (drain_pending is shutdown-only). is_finished is a non-blocking
        // poll; detached-but-complete tasks are reaped here.
        in_flight.retain(|h| !h.is_finished());
        in_flight.push(handle);
    }

    /// Drain in-flight extraction tasks before shutdown. Awaits every spawned
    /// handle up to the timeout; on timeout the remaining handles are dropped
    /// (detached) so the caller can proceed — the runtime's abort handles the
    /// stragglers. No-op when nothing is in flight. Soft-timeout drain
    /// shape.
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
                _ = &mut deadline => {
                    // Timed out; the rest detach. The caller proceeds (the
                    // runtime aborts the stragglers on shutdown).
                    return;
                }
            }
        }
    }
}

/// Advance the cursor to the last message id. No-op if messages is empty.
fn advance_cursor(cursor: &Mutex<Option<EventId>>, messages: &[SessionLogEntry]) {
    if let Some(last) = messages.last() {
        *cursor.lock().expect("cursor") = Some(last.id);
    }
}

/// Whether the tail holds any save_memory tool call from the main agent.
/// A lightweight gate for the mutual-exclusion skip; it does not reconstruct
/// the change (the main runner records primary saves at call time).
fn primary_saved_in(tail: &[SessionLogEntry]) -> bool {
    tail.iter().any(|m| {
        matches!(
            &m.event,
            SessionEvent::ToolCall { tool, .. } if tool == "save_memory"
        )
    })
}

#[cfg(test)]
#[path = "extractor_tests.rs"]
mod extractor_tests;
