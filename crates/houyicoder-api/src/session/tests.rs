//! Tests for the session log port: a logged turn reads back as the usage the
//! views show, and a paged read of a history ends on the store's terms.

use super::recorded_turn_usage;
use houyicoder_async::PFut;
use houyicoder_context::{EventId, SessionEvent, SessionId, SessionLogEntry};
use houyicoder_protocol::llm::UsageProvenance;

use super::{SessionLog, TrajectoryHead, TrajectoryPage, TrajectoryPages, TrajectoryPosition};
use houyicoder_context::{
    CheckpointId, CheckpointManifest, ContextBackend, ContextError, ContextSnapshot,
};
use std::collections::VecDeque;
use std::sync::Mutex;

/// A usage entry as the runner appends it: the counts, the model that ran
/// the call, and whether the endpoint reported each count.
fn turn_usage(model: &str) -> SessionLogEntry {
    SessionLogEntry {
        id: EventId::new(),
        session: SessionId::new(),
        ts: 0,
        prev_hash: None,
        event: SessionEvent::TurnUsage {
            turn: 1,
            call_in_turn: 1,
            input_tokens: 12_000,
            output_tokens: 500,
            cache_read_input_tokens: 9_600,
            cache_write_input_tokens: 400,
            reasoning_tokens: 120,
            model: model.to_string(),
            recovery: false,
            effort: None,
            input_reported: true,
            cache_read_reported: true,
            cache_write_reported: false,
        },
    }
}

#[test]
fn test_recorded_turn_reads_back() {
    let entry = turn_usage("claude-sonnet-5-5");
    let (model, usage) = recorded_turn_usage(&entry.event).expect("a usage entry reads back");
    assert_eq!(model, "claude-sonnet-5-5");
    assert_eq!(usage.input_tokens, 12_000);
    assert_eq!(usage.total_tokens, 12_500);
    assert_eq!(usage.non_cached_input_tokens, 2_000);
    assert_eq!(usage.cache_hit_rate(), Some(0.8));
    // The flags travel with the counts: a record that did not report the
    // cache-write count reads back short of a full account.
    assert_eq!(
        usage.provenance,
        UsageProvenance {
            input_reported: true,
            cache_read_reported: true,
            cache_write_reported: false,
        }
    );
}

#[test]
fn test_other_events_read_none() {
    let cleared = SessionEvent::ContextCleared { prior_turn: 3 };
    assert!(recorded_turn_usage(&cleared).is_none());
}

/// A log that answers paged reads from a script. The end conditions a walk
/// guards against are the store's to produce, and a store only reaches them
/// through failures and histories its own log cannot serve, so the script
/// states them directly. Only the page surface is answered: a walk reads
/// nothing else.
struct ScriptedLog {
    script: Mutex<VecDeque<Result<TrajectoryPage, ContextError>>>,
    /// A page every read answers with, so a walk cannot advance by asking
    /// again. Set instead of a script, for the log that has nothing past the
    /// position it already reported.
    stuck: Option<TrajectoryPage>,
    asked: Mutex<Vec<TrajectoryPosition>>,
}

impl ScriptedLog {
    fn new(script: Vec<Result<TrajectoryPage, ContextError>>) -> Self {
        Self {
            script: Mutex::new(script.into()),
            stuck: None,
            asked: Mutex::new(Vec::new()),
        }
    }

    /// A log that answers every read with the same page.
    fn stuck(page: TrajectoryPage) -> Self {
        Self {
            script: Mutex::new(VecDeque::new()),
            stuck: Some(page),
            asked: Mutex::new(Vec::new()),
        }
    }

    /// The position each read was asked from, in order.
    fn asked(&self) -> Vec<TrajectoryPosition> {
        self.asked.lock().unwrap().clone()
    }
}

/// One page holding the given records, continuing at an ordinal.
fn page(events: Vec<SessionLogEntry>, next: usize) -> TrajectoryPage {
    TrajectoryPage {
        events,
        next: TrajectoryPosition {
            index: next,
            byte_offset: None,
        },
        skipped_bytes: 0,
    }
}

fn entry(text: &str) -> SessionLogEntry {
    SessionLogEntry {
        id: EventId::new(),
        session: SessionId::new(),
        ts: 0,
        prev_hash: None,
        event: SessionEvent::UserInput {
            text: text.to_string(),
        },
    }
}

impl SessionLog for ScriptedLog {
    fn append(&self, _event: SessionLogEntry) -> PFut<'_, Result<EventId, ContextError>> {
        unreachable!("a paged walk appends nothing")
    }
    fn replay(&self, _session: SessionId) -> PFut<'_, Result<Vec<SessionLogEntry>, ContextError>> {
        unreachable!("a paged walk reads pages, not the whole log")
    }
    fn current_view(&self, _session: SessionId) -> PFut<'_, Result<ContextSnapshot, ContextError>> {
        unreachable!("a paged walk assembles no context")
    }
    fn trajectory_tail(&self, _session: SessionId) -> Vec<SessionLogEntry> {
        unreachable!("a paged walk reads pages, not the tail")
    }
    fn trajectory_page(
        &self,
        _session: SessionId,
        from: TrajectoryPosition,
        _limit: usize,
    ) -> Result<TrajectoryPage, ContextError> {
        self.asked.lock().unwrap().push(from);
        if let Some(page) = &self.stuck {
            return Ok(page.clone());
        }
        // An exhausted script answers with an empty page, which is how a
        // store reports that it has nothing left to serve.
        self.script
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Ok(TrajectoryPage::default()))
    }
    fn trajectory_head(&self, _session: SessionId) -> TrajectoryHead {
        TrajectoryHead::default()
    }
    fn reset_trajectory(&self, _session: SessionId) {}
    fn write_checkpoint(
        &self,
        _manifest: CheckpointManifest,
    ) -> PFut<'_, Result<CheckpointId, ContextError>> {
        unreachable!("a paged walk writes no checkpoint")
    }
    fn read_checkpoint(
        &self,
        _id: CheckpointId,
    ) -> PFut<'_, Result<CheckpointManifest, ContextError>> {
        unreachable!("a paged walk reads no checkpoint")
    }
    fn list_checkpoints(
        &self,
        _session: SessionId,
    ) -> PFut<'_, Result<Vec<CheckpointId>, ContextError>> {
        unreachable!("a paged walk lists no checkpoints")
    }
    fn backend(&self) -> &dyn ContextBackend {
        unreachable!("a paged walk never reaches the backend")
    }
}

/// A walk ends at the first page without records, asking each next page from
/// the position the previous one reported.
#[test]
fn test_pages_end_when_empty() {
    let log = ScriptedLog::new(vec![
        Ok(page(vec![entry("first"), entry("second")], 2)),
        // The empty page reports a position past what it covered: only the
        // page being empty ends the walk here, not the position standing still.
        Ok(page(Vec::new(), 3)),
    ]);
    let session = SessionId::new();
    let pages: Vec<TrajectoryPage> =
        TrajectoryPages::new(&log, session, TrajectoryPosition::start())
            .map(|page| page.expect("the script reports no failure"))
            .collect();
    assert_eq!(pages.len(), 1, "the empty page ends the walk");
    assert_eq!(pages[0].events.len(), 2);
    assert_eq!(
        log.asked(),
        vec![
            TrajectoryPosition::start(),
            TrajectoryPosition {
                index: 2,
                byte_offset: None
            },
        ],
        "each read resumes where the page before it ended",
    );
}

/// A page that reports its own start back would be read forever, so the walk
/// stops after reporting it once.
#[test]
fn test_pages_end_without_moving() {
    let log = ScriptedLog::stuck(page(vec![entry("stuck")], 0));
    let session = SessionId::new();
    let pages: Vec<TrajectoryPage> =
        TrajectoryPages::new(&log, session, TrajectoryPosition::start())
            .map(|page| page.expect("the script reports no failure"))
            .collect();
    assert_eq!(pages.len(), 1, "a page that does not advance ends the walk");
    assert_eq!(pages[0].events.len(), 1, "and the reader still sees it");
}

/// A failed read ends the walk after reporting the failure, because retrying
/// the same position would keep failing.
#[test]
fn test_pages_end_on_failure() {
    let log = ScriptedLog::new(vec![Err(ContextError::Io), Ok(page(vec![entry("x")], 1))]);
    let session = SessionId::new();
    let pages: Vec<Result<TrajectoryPage, ContextError>> =
        TrajectoryPages::new(&log, session, TrajectoryPosition::start()).collect();
    assert_eq!(pages.len(), 1, "the walk reports the failure once");
    assert!(pages[0].is_err(), "and reports it as the failure it is");
}
