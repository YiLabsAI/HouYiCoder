//! Tests for the session log port: a logged turn reads back as the usage the
//! views show, and a paged read of a history ends on the store's terms.

use super::aggregate_recap_spend;
use super::recorded_turn_usage;
use houyicoder_async::PFut;
use houyicoder_context::{
    EventId, RecapSource, RecapUsage, SessionEvent, SessionId, SessionLogEntry,
};
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
            cas_recorded: false,
        },
    }
}

/// A recap as the host appends it, with whatever account the endpoint gave.
fn recap_entry(usage: Option<RecapUsage>) -> SessionLogEntry {
    SessionLogEntry {
        id: EventId::new(),
        session: SessionId::new(),
        ts: 0,
        prev_hash: None,
        event: SessionEvent::AwayRecap {
            text: "the parser is half converted".into(),
            last_user_event: EventId::new(),
            as_of_event: EventId::new(),
            source: RecapSource::Away,
            model: "small-1".into(),
            usage,
        },
    }
}

fn reported(input: u64, output: u64, cache_read: u64) -> RecapUsage {
    RecapUsage {
        input_tokens: Some(input),
        output_tokens: Some(output),
        cache_read_input_tokens: Some(cache_read),
        cache_write_input_tokens: None,
        duration_ms: Some(1_200),
    }
}

/// A recap's spend joins the session account without moving a turn counter,
/// and every figure it names is summed. A call the endpoint reported nothing
/// for is counted as unmeasured rather than as a free one, so the total
/// reads as a floor.
#[test]
fn test_recap_spend_sums_reported() {
    let events = vec![
        turn_usage("claude-sonnet-5-5"),
        recap_entry(Some(reported(2_000, 160, 1_500))),
        recap_entry(Some(reported(2_100, 140, 1_600))),
        recap_entry(None),
    ];
    let spend = aggregate_recap_spend(&events);
    assert_eq!(spend.calls, 3, "every recap event is a call");
    assert_eq!(
        spend.unmeasured_calls, 1,
        "the silent one is not a free one"
    );
    assert_eq!(spend.input_tokens, 4_100);
    assert_eq!(spend.output_tokens, 300);
    assert_eq!(spend.cache_read_input_tokens, 3_100);
    assert_eq!(spend.duration_ms, Some(2_400));
    assert_eq!(
        spend.provenance,
        UsageProvenance::default(),
        "the silent recap leaves the whole account unknown, not merely short"
    );
}

/// A usage object carrying a duration but no token figure is still an
/// unmeasured call: the time says the request ran, not what it cost. It
/// contributes nothing to the bucket, so no row can print a time beside an
/// account nobody reported.
#[test]
fn test_recap_spend_counts_silent() {
    let silent = RecapUsage {
        input_tokens: None,
        output_tokens: None,
        cache_read_input_tokens: None,
        cache_write_input_tokens: None,
        duration_ms: Some(900),
    };
    let spend = aggregate_recap_spend(&[recap_entry(Some(silent))]);
    assert_eq!(spend.calls, 1);
    assert_eq!(spend.unmeasured_calls, 1);
    assert_eq!(spend.duration_ms, None);
    assert_eq!(spend.input_tokens, 0);
    assert_eq!(
        spend.provenance,
        UsageProvenance::default(),
        "a recap with no figures leaves the account unknown, not zero"
    );
}

/// The bucket converts to the status shape with its provenance intact, so a
/// recap nobody measured lowers the combined account's completeness instead of
/// adding zeroes that read as measured.
#[test]
fn test_recap_spend_converts() {
    let events = [
        recap_entry(Some(reported(2_000, 160, 1_500))),
        recap_entry(None),
    ];
    let usage = aggregate_recap_spend(&events).to_usage();
    assert_eq!(usage.input_tokens, 2_000);
    assert!(
        !usage.provenance.input_reported,
        "the unmeasured recap leaves the converted account a floor"
    );
}

#[test]
fn test_recap_spend_skips_turns() {
    let spend = aggregate_recap_spend(&[turn_usage("claude-sonnet-5-5")]);
    assert_eq!(spend.calls, 0, "a turn is not a recap");
    assert_eq!(spend.input_tokens, 0);
    assert_eq!(spend.unmeasured_calls, 0);
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
    fn append_if<'a>(
        &'a self,
        _event: SessionLogEntry,
        _allow: &'a (dyn Fn(&[SessionLogEntry]) -> bool + Send + Sync),
    ) -> PFut<'a, Result<Option<EventId>, ContextError>> {
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
    /// This double stands for a session whose log is read page by page and
    /// whose window no reader can be placed at, so a reader beginning here
    /// starts at the head.
    fn replay_starts_at(&self, _session: SessionId) -> Result<Option<usize>, ContextError> {
        Ok(None)
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
