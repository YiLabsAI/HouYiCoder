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
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep, sleep_until};

use crate::agent::extract::run_forked_extract;
use crate::agent::memory::MutationLog;
use crate::agent::{RunError, RunResult, RunnerConfig};

/// The evidence-boundary type the extractor cuts and the forked run reads.
use super::extraction_window;
use crate::agent::auto_dream::InProgressGuard;
use extraction_window::ExactExtractionWindow;
use std::mem::take;
use std::pin::Pin;

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
    idle_notify: Notify,
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
            idle_notify: Notify::new(),
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
        let (outcome, causality, changes) = self.collect_extraction_once(messages).await;
        if let Some(causality) = causality {
            self.emit_changes(MemoryChangeOrigin::AutoMemory, causality, changes);
        }
        outcome
    }

    async fn collect_extraction_once(
        &self,
        messages: &[SessionLogEntry],
    ) -> (
        Result<ExtractOutcome, RunError>,
        Option<MemoryChangeCausality>,
        Vec<MemoryChange>,
    ) {
        let cursor = *self.cursor.lock().expect("cursor");
        let Some(tail) = ExactExtractionWindow::unconsumed(messages, cursor.as_ref()) else {
            tracing::warn!("extraction cursor does not resolve; skipping the pass");
            advance_cursor(&self.cursor, messages);
            return (
                Ok(ExtractOutcome::Skipped(ExtractSkip::CursorLost)),
                None,
                Vec::new(),
            );
        };
        if primary_saved_in(tail) {
            advance_cursor(&self.cursor, messages);
            return (
                Ok(ExtractOutcome::Skipped(ExtractSkip::PrimaryWrote)),
                None,
                Vec::new(),
            );
        }
        let Some(window) = ExactExtractionWindow::from_unconsumed(tail) else {
            return (
                Ok(ExtractOutcome::Skipped(ExtractSkip::NoQueryTurn)),
                None,
                Vec::new(),
            );
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
        if result.is_ok() {
            advance_cursor(&self.cursor, messages);
            let causality = self.causality_for(window.session(), window.trigger_user_event());
            (
                result.map(ExtractOutcome::Extracted),
                Some(causality),
                recorder.take(),
            )
        } else {
            (result.map(ExtractOutcome::Extracted), None, Vec::new())
        }
    }

    /// Run extraction inline inside the turn's serial pipeline, draining any
    /// stashed trailing context into shared_log without emitting a separate
    /// notice. Boxed to break the compile-time Send cycle through the forked
    /// runner's lifecycle.
    pub(crate) fn run_pipeline<'a>(
        &'a self,
        messages: Vec<SessionLogEntry>,
        shared_log: &'a MutationLog,
    ) -> Pin<Box<dyn std::future::Future<Output = Option<MemoryChangeCausality>> + Send + 'a>> {
        Box::pin(async move {
            {
                let mut ip = self.in_progress.lock().expect("in_progress");
                if *ip {
                    *self.pending_context.lock().expect("pending") = Some(messages);
                    return None;
                }
                *ip = true;
            }
            let _guard = InProgressGuard::with_notify(&self.in_progress, &self.idle_notify);
            let mut current = messages;
            let mut last_causality = None;
            loop {
                let (_outcome, causality, changes) = self.collect_extraction_once(&current).await;
                if let Some(c) = causality {
                    last_causality = Some(c);
                }
                shared_log.extend(changes);
                let pending = self.pending_context.lock().expect("pending").take();
                if let Some(next) = pending {
                    current = next;
                } else {
                    break;
                }
            }
            last_causality
        })
    }

    /// Track a background task handle so shutdown drain awaits it.
    pub(crate) fn track_task(&self, handle: JoinHandle<()>) {
        let mut in_flight = self.in_flight.lock().expect("in_flight");
        in_flight.retain(|h| !h.is_finished());
        in_flight.push(handle);
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
    ) -> Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        Box::pin(async move {
            let _guard = if !is_trailing {
                Some(InProgressGuard::with_notify(
                    &self.in_progress,
                    &self.idle_notify,
                ))
            } else {
                None
            };
            let _outcome = self.run_extraction_once(&messages).await;
            let pending = self.pending_context.lock().expect("pending").take();
            if let Some(trailing) = pending {
                Arc::clone(&self).run_extraction(trailing, true).await;
            } else {
                *self.in_progress.lock().expect("in_progress") = false;
                self.idle_notify.notify_waiters();
            }
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

    /// Wait until no extraction pass is running, up to the given budget. The
    /// turn's background pipeline uses it so the dream reads what the pass
    /// wrote. Unlike the shutdown drain it never takes the in-flight handles,
    /// which belong to shutdown alone: taking them here would let a later turn
    /// find an empty list and start the dream beside a running pass.
    pub async fn wait_idle(&self, budget: Duration) {
        let deadline = Instant::now() + budget;
        loop {
            let notified = self.idle_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if !*self.in_progress.lock().expect("in_progress") {
                return;
            }
            if Instant::now() >= deadline {
                return;
            }
            tokio::select! {
                () = &mut notified => {}
                () = sleep_until(deadline) => return,
            }
        }
    }

    /// Drain in-flight extraction tasks before shutdown. Awaits every spawned
    /// handle up to the timeout; on timeout the remaining handles are dropped
    /// (detached) so the caller can proceed — the runtime's abort handles the
    /// stragglers. No-op when nothing is in flight. Soft-timeout drain
    /// shape.
    pub async fn drain_pending(&self, timeout: Duration) {
        let handles: Vec<JoinHandle<()>> = take(&mut *self.in_flight.lock().expect("in_flight"));
        if handles.is_empty() {
            return;
        }
        let mut deadline = Box::pin(sleep(timeout));
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

/// Whether the tail holds a memory_write call that succeeded.
fn primary_saved_in(tail: &[SessionLogEntry]) -> bool {
    let save_call_ids: Vec<&str> = tail
        .iter()
        .filter_map(|m| match &m.event {
            SessionEvent::ToolCall { tool, call_id, .. }
                if matches!(tool.as_str(), "memory_write" | "save_memory") =>
            {
                Some(call_id.as_str())
            }
            _ => None,
        })
        .collect();
    if save_call_ids.is_empty() {
        return false;
    }
    save_call_ids.iter().any(|cid| {
        tail.iter().any(|m| {
            matches!(
                &m.event,
                SessionEvent::ToolResult { call_id, output, .. }
                    if call_id == cid && output.get("saved").is_some()
            )
        })
    })
}

#[cfg(test)]
mod tests;
