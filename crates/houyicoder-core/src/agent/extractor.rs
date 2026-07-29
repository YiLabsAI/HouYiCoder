//! The memory extractor: a cursor and mutual-exclusion gate around the
//! forked extraction run. The cursor marks the last message the background
//! extraction consumed, so the next run counts only new messages; the mutex
//! skips the fork when the main agent already saved a memory via a
//! save_memory tool call in this turn range (no point re-extracting what was
//! just written). Both are recomputed by re-scanning the message log each
//! call — no stored flag. State is process-lifetime only: the cursor is
//! not persisted, so a fresh process counts all messages on the first
//! pass.
//!
//! This first slice is synchronous: the fork runs to completion before
//! returning. Fire-and-forget spawn, coalescing, and shutdown drain land in
//! the next slice. The forked agent always receives the full conversation as
//! its prompt-cache prefix; the cursor only governs the new-message count fed
//! into the extraction prompt, the mutex scan range, and the advance that
//! prevents re-counting.

use std::path::PathBuf;
use std::sync::atomic::AtomicU32;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use houyicoder_api::live::{LiveEvent, LiveSink, MemorySavedKind};
use houyicoder_api::memory::MemoryProvider;
use houyicoder_api::provider::ModelProvider;
use houyicoder_api::session::SessionLog;
use houyicoder_context::{EventId, TurnEvent, TurnEventKind};
use tokio::task::JoinHandle;

use super::extract::run_forked_extract;
use super::{RunError, RunResult, RunnerConfig};

/// The outcome of one extraction pass.
#[derive(Debug)]
pub enum ExtractOutcome {
    /// The forked agent ran to completion.
    Extracted(RunResult),
    /// The fork was skipped because the main agent already saved a memory in
    /// this turn range (mutual exclusion). The cursor still advances past the
    /// range so the next run does not re-scan it.
    Skipped { new_message_count: usize },
}

/// The memory extractor: cursor and mutex gate around the forked run. Holds
/// the shared provider, memory, store, and config so it can drive a forked
/// extraction on demand. The cursor is an in-memory Option of the last
/// consumed message id; it advances on a successful run and on a
/// mutual-exclusion skip, but NOT on error (errored messages are reconsidered
/// next pass).
pub struct MemoryExtractor {
    cursor: Mutex<Option<EventId>>,
    in_progress: Mutex<bool>,
    pending_context: Mutex<Option<Vec<TurnEvent>>>,
    in_flight: Mutex<Vec<JoinHandle<()>>>,
    store: Arc<dyn SessionLog>,
    provider: Arc<dyn ModelProvider>,
    memory: Arc<dyn MemoryProvider>,
    cwd: PathBuf,
    config: RunnerConfig,
    /// Host-installed sink fired once per pass when memories land (extract
    /// fork wrote N, or the main agent saved this turn so the fork was
    /// skipped). None when no host wires one (tests, forked runners), so
    /// notify is a no-op there.
    notify_sink: Mutex<Option<LiveSink>>,
}

impl MemoryExtractor {
    /// Construct with the shared handles. The store, provider, and memory
    /// are shared with the main runner so prompt caching and the in-process
    /// write lock carry over.
    pub fn new(
        store: Arc<dyn SessionLog>,
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
            store,
            provider,
            memory,
            cwd,
            config,
            notify_sink: Mutex::new(None),
        }
    }

    /// Install the host sink fired on each pass that writes memories. The
    /// runner forwards its own live sink here so the extractor (a detached
    /// spawned task) can push a MemorySaved event without holding a wire
    /// handle. The forked runner's live sink stays None so the fork's token
    /// deltas do not fire into the user transcript.
    pub fn set_notify_sink(&self, sink: LiveSink) {
        *self.notify_sink.lock().expect("notify_sink") = Some(sink);
    }

    /// Fire one MemorySaved notice if a sink is wired + count > 0. Best-effort:
    /// a None sink (tests, forked runner) is a no-op; the sink itself is
    /// try_send-droppable on a full channel.
    fn fire_saved(&self, count: u32, kind: MemorySavedKind) {
        if count == 0 {
            return;
        }
        let sink = self.notify_sink.lock().expect("notify_sink").clone();
        if let Some(sink) = sink {
            sink(&LiveEvent::MemorySaved { count, kind });
        }
    }

    /// Drive one extraction pass synchronously: the gating logic + the forked
    /// run + cursor advance. Returns the outcome so the fire-and-forget body
    /// (and tests) can inspect it. This is the per-pass body; the spawn
    /// wrapper, coalescing, and trailing pickup live in run_extraction.
    pub async fn run_extraction_once(
        &self,
        messages: &[TurnEvent],
    ) -> Result<ExtractOutcome, RunError> {
        let cursor = *self.cursor.lock().expect("cursor");
        let new_message_count = count_messages_since(messages, cursor.as_ref());
        if has_memory_writes_since(messages, cursor.as_ref()) {
            // The main agent saved this turn (mutual exclusion: the fork
            // would re-extract). This is the path the user directly
            // triggered, so it deserves a notice — count the saves + fire.
            let saved = count_memory_writes_since(messages, cursor.as_ref()) as u32;
            advance_cursor(&self.cursor, messages);
            self.fire_saved(saved, MemorySavedKind::Extracted);
            return Ok(ExtractOutcome::Skipped { new_message_count });
        }
        // A fresh counter the fork's save_memory tool bumps per successful
        // write. Reset before the run so the load after is this pass's count
        // (not an accumulator across passes).
        let counter = Arc::new(AtomicU32::new(0));
        let result = run_forked_extract(
            Arc::clone(&self.store),
            Arc::clone(&self.provider),
            Arc::clone(&self.memory),
            &self.cwd,
            self.config.clone(),
            messages,
            Arc::clone(&counter),
        )
        .await;
        // Advance the cursor only on success. On error the cursor stays so
        // the errored messages are reconsidered next pass.
        if result.is_ok() {
            advance_cursor(&self.cursor, messages);
            self.fire_saved(
                counter.load(std::sync::atomic::Ordering::SeqCst),
                MemorySavedKind::Extracted,
            );
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
        messages: Vec<TurnEvent>,
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
            let _ = is_trailing; // trailing runs skip the throttle (not yet impl)
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
    pub fn extract_memories(self: &Arc<Self>, messages: Vec<TurnEvent>) {
        // Cheap pre-check: if there are no new messages since the cursor, do
        // nothing — avoids spawning a forked LLM run when the conversation
        // has not advanced (e.g. a re-emitted FinalOutput after a verify
        // retry). The cursor-missing fallback in count_messages_since counts
        // all, so this only short-circuits when the cursor is already at the
        // last message.
        let cursor = *self.cursor.lock().expect("cursor");
        if count_messages_since(&messages, cursor.as_ref()) == 0 {
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
fn advance_cursor(cursor: &Mutex<Option<EventId>>, messages: &[TurnEvent]) {
    if let Some(last) = messages.last() {
        *cursor.lock().expect("cursor") = Some(last.id);
    }
}

/// Count model-visible messages (user + assistant) after the cursor. If the
/// cursor is None (fresh process) or its id is not found in the messages
/// (compaction removed it), count all — never return 0, which would
/// permanently disable extraction for the rest of the session.
pub(crate) fn count_messages_since(messages: &[TurnEvent], cursor: Option<&EventId>) -> usize {
    let start = match cursor {
        None => 0,
        Some(id) => match messages.iter().position(|m| &m.id == id) {
            Some(i) => i + 1,
            None => 0,
        },
    };
    messages
        .iter()
        .skip(start)
        .filter(|m| is_model_visible(&m.kind))
        .count()
}

/// True if the main agent emitted a save_memory tool call after the cursor
/// (mutual exclusion: the fork would just re-extract what was already
/// saved). Same fallback as count_messages_since when the cursor id is not
/// found.
pub(crate) fn has_memory_writes_since(messages: &[TurnEvent], cursor: Option<&EventId>) -> bool {
    let start = match cursor {
        None => 0,
        Some(id) => match messages.iter().position(|m| &m.id == id) {
            Some(i) => i + 1,
            None => 0,
        },
    };
    messages
        .iter()
        .skip(start)
        .any(|m| is_save_memory_call(&m.kind))
}

/// Count of save_memory tool calls the main agent emitted after the cursor.
/// The Skipped path (mutual exclusion: the fork would re-extract what the
/// main agent already saved) still owes the user a memory-saved notice — it
/// is the path the user directly triggered by telling the agent to save.
/// Same fallback as has_memory_writes_since when the cursor id is not found.
pub(crate) fn count_memory_writes_since(messages: &[TurnEvent], cursor: Option<&EventId>) -> usize {
    let start = match cursor {
        None => 0,
        Some(id) => match messages.iter().position(|m| &m.id == id) {
            Some(i) => i + 1,
            None => 0,
        },
    };
    messages
        .iter()
        .skip(start)
        .filter(|m| is_save_memory_call(&m.kind))
        .count()
}

fn is_model_visible(kind: &TurnEventKind) -> bool {
    matches!(
        kind,
        TurnEventKind::UserInput { .. }
            | TurnEventKind::MidTurnInput { .. }
            | TurnEventKind::AssistantMessage { .. }
    )
}

fn is_save_memory_call(kind: &TurnEventKind) -> bool {
    matches!(kind, TurnEventKind::ToolCall { tool, .. } if tool == "save_memory")
}

#[cfg(test)]
#[path = "extractor_tests.rs"]
mod extractor_tests;
