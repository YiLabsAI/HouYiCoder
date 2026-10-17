//! The session's in-process trajectory mirror.
//!
//! The raw append-only log is the source of truth; this is the sync view of it
//! a reader can hold without touching the disk. One session's mirror keeps its
//! newest events, the durable revision they add up to, the epoch the current
//! history began in, and the whole-session summary folded from them — all
//! under one lock, so a reader cannot see figures from a different revision
//! than the events they came from.
//!
//! The events are a window, not the history. A session read back from a log
//! holds as many of its newest events as a byte budget allows; the rest stay
//! on disk, where the store's paged read reaches them. Everything a reader can
//! ask for by number is exempt from that bound: the row count, the durable
//! count, the id of the latest event, and the id of the latest user input are
//! absolute, so trimming events out of the window never changes what the
//! session is said to hold. A backend with no byte log has nowhere else to
//! read, so its mirror holds everything and nothing is trimmed.
//!
//! A reader that pages takes positions in a durable ordinal per session: the
//! record's place in the log, counting from the record the newest clear
//! boundary begins at. The window answers the ordinals it still holds; the log
//! answers the ones below it, which is why the mirror also carries the byte
//! position the current epoch starts at.
//!
//! Appending writes here; reading never does. The store calls the note methods
//! on its append path and the read methods on its read path, so the ordering
//! that keeps the two consistent lives in one place instead of being spread
//! through the facade. What each event contributes to the session's figures is
//! the summary module's business; how a log becomes this window is the log
//! windows module's.

mod summary;

use std::collections::HashMap;
use std::collections::VecDeque;
use std::mem::size_of;
use std::sync::Mutex;
use std::sync::MutexGuard;

use houyicoder_api::session::{TrajectoryHead, TrajectoryPosition, TrajectoryRevision};
use houyicoder_context::{EventId, SessionEvent, SessionId, SessionLogEntry};
use serde_json::Value;

use summary::TrajectorySummaryState;

/// Bytes a window may hold before the mirror trims its oldest events. Sized for
/// the window's job -- answering a reader that asks for the recent past without
/// a disk read -- rather than for the whole history, which is the log's.
pub(super) const DEFAULT_RESIDENT_BYTES: usize = 16 * 1024 * 1024;

/// The variable a test sets to drive the resident window to a size it can
/// observe. Unset in production, where the default applies. The frontend's
/// transcript window reads the same variable, so one setting bounds both; the
/// layering does not let this crate reach the config crate that names every
/// variable, so each reader spells the name.
pub(super) const RESIDENT_BUDGET_ENV: &str = "HOUYICODER_TEST_RESIDENT_BYTE_BUDGET";

/// What the window holds for a read at an absolute ordinal.
pub(super) enum WindowRead {
    /// The window covered the ordinal. The events are the durable records from
    /// it onward, and the second field is the ordinal a read continues at.
    Held {
        events: Vec<SessionLogEntry>,
        next: usize,
    },
    /// The ordinal is older than the window. The log holds it, and these are
    /// what a read from the log needs to reach the window's own first record.
    Below {
        base_index: usize,
        epoch_byte_offset: Option<u64>,
    },
    /// No mirror for this session in this process.
    Absent,
}

/// One session's durable facts, mirrored in memory.
#[derive(Default)]
struct SessionMirror {
    /// The newest rows, at most the resident budget allows.
    window: VecDeque<SessionLogEntry>,
    /// Rows the window has dropped off its front, of any kind. The session's
    /// row count is this plus the window.
    dropped_rows: usize,
    /// Durable records the window has dropped off its front: the ordinal of
    /// the window's first durable record, and the position a read below the
    /// window continues from in the log.
    base_index: usize,
    /// The byte position the current epoch starts at in the log, which is what
    /// resolves a position that carries no byte offset of its own.
    epoch_byte_offset: Option<u64>,
    /// Durable records this session holds, in the window or not.
    durable_events: usize,
    last_durable_id: Option<EventId>,
    /// The latest row of any kind: a reader that follows the log's progress
    /// wants the newest thing that happened, streamed text included.
    last_event_id: Option<EventId>,
    /// The latest durable user input, taken as it arrives so an input the
    /// window has trimmed still answers.
    last_user_input_id: Option<EventId>,
    /// The first durable event of the window, which is what makes the epoch
    /// identity: a clear restarts the mirror, so its next event begins a new
    /// one.
    epoch_event_id: Option<EventId>,
    /// Bytes the window's rows hold, charged against the resident budget.
    resident_bytes: usize,
    summary: TrajectorySummaryState,
}

impl SessionMirror {
    fn head(&self) -> TrajectoryHead {
        TrajectoryHead {
            revision: TrajectoryRevision {
                event_count: self.dropped_rows + self.window.len(),
                last_event_id: self.last_event_id,
                durable_event_count: self.durable_events,
                last_durable_event_id: self.last_durable_id,
                epoch_event_id: self.epoch_event_id,
            },
            summary: self.summary.snapshot(),
        }
    }

    /// Take one row into the window, counting the durable figures it moves.
    fn push(&mut self, event: SessionLogEntry, budget_bytes: usize, bounded: bool) {
        if !is_display_delta(&event.event) {
            self.durable_events += 1;
            self.last_durable_id = Some(event.id);
            if self.epoch_event_id.is_none() {
                self.epoch_event_id = Some(event.id);
            }
        }
        if matches!(event.event, SessionEvent::UserInput { .. }) {
            self.last_user_input_id = Some(event.id);
        }
        self.last_event_id = Some(event.id);
        self.summary.record(&event);
        self.resident_bytes += resident_bytes(&event);
        self.window.push_back(event);
        if bounded {
            self.trim(budget_bytes);
        }
    }

    /// Drop the oldest rows until the window fits its budget. One row is kept
    /// whatever it measures: a single event larger than the whole budget is
    /// still the newest thing the session did, and dropping it would leave the
    /// window empty while the log holds more.
    fn trim(&mut self, budget_bytes: usize) {
        while self.resident_bytes > budget_bytes && self.window.len() > 1 {
            let Some(dropped) = self.window.pop_front() else {
                return;
            };
            self.resident_bytes = self.resident_bytes.saturating_sub(resident_bytes(&dropped));
            self.dropped_rows += 1;
            if !is_display_delta(&dropped.event) {
                self.base_index += 1;
            }
        }
    }

    /// The durable records the window holds from an ordinal onward, up to a
    /// count.
    fn page(&self, from: usize, limit: usize) -> Vec<SessionLogEntry> {
        if from < self.base_index || limit == 0 {
            return Vec::new();
        }
        self.window
            .iter()
            .filter(|entry| !is_display_delta(&entry.event))
            .skip(from - self.base_index)
            .take(limit)
            .cloned()
            .collect()
    }
}

/// Every session's mirror, under the one lock that keeps events and figures
/// moving together.
pub(super) struct TrajectoryMirrors {
    inner: Mutex<HashMap<SessionId, SessionMirror>>,
    /// Whether a mirror trims its window. False for a backend that keeps no
    /// byte-addressable log, whose mirror is the only place its history
    /// exists.
    bounded: bool,
    budget_bytes: usize,
}

impl TrajectoryMirrors {
    /// The mirrors over a backend that can (or cannot) answer a read below the
    /// window from its own log, and the bytes a window may hold.
    pub(super) fn new(bounded: bool, budget_bytes: usize) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            bounded,
            budget_bytes,
        }
    }

    /// The bytes a history folded from the log may keep in the window. A
    /// backend with no log keeps everything, because there is nowhere else to
    /// read it.
    pub(super) fn restore_budget(&self) -> usize {
        if self.bounded {
            self.budget_bytes
        } else {
            usize::MAX
        }
    }

    /// Note a streaming delta: it advances the session's span and joins the
    /// window, and moves no durable counter because it is not a durable fact.
    pub(super) fn note_delta(&self, session: SessionId, event: SessionLogEntry) {
        let mut mirrors = self.lock();
        let mirror = mirrors.entry(session).or_default();
        mirror.push(event, self.budget_bytes, self.bounded);
    }

    /// Note a durable event: the summary folds it, the revision moves, and the
    /// first durable event of a restarted mirror begins a new epoch.
    pub(super) fn note_durable(&self, session: SessionId, event: SessionLogEntry) {
        let mut mirrors = self.lock();
        let mirror = mirrors.entry(session).or_default();
        mirror.push(event, self.budget_bytes, self.bounded);
    }

    /// Whether this session's mirror still needs the byte position its epoch
    /// starts at. Asked before an append is written, when the log's size is
    /// that position.
    pub(super) fn needs_epoch_offset(&self, session: SessionId) -> bool {
        if !self.bounded {
            return false;
        }
        self.lock()
            .get(&session)
            .is_none_or(|mirror| mirror.epoch_byte_offset.is_none())
    }

    /// Record the byte position the session's epoch starts at, discarding any
    /// window held for the session. Called before an append whose record opens
    /// an epoch: the position is where the log stood, and the rows the mirror
    /// holds describe a history that no longer counts.
    pub(super) fn start_epoch(&self, session: SessionId, byte_offset: u64) {
        let mut mirrors = self.lock();
        mirrors.insert(
            session,
            SessionMirror {
                epoch_byte_offset: Some(byte_offset),
                ..SessionMirror::default()
            },
        );
    }

    /// Install a history folded from a session's log. An empty history leaves
    /// the session with no mirror.
    pub(super) fn commit_restore(&self, session: SessionId, restored: RestoredHistory) {
        if restored.rows == 0 {
            self.reset(session);
            return;
        }
        let mirror = SessionMirror {
            window: restored.window,
            dropped_rows: restored.dropped_rows,
            base_index: restored.base_index,
            epoch_byte_offset: restored.epoch_byte_offset,
            durable_events: restored.rows,
            last_durable_id: restored.last_durable_id,
            last_event_id: restored.last_event_id,
            last_user_input_id: restored.last_user_input_id,
            epoch_event_id: restored.epoch_event_id,
            resident_bytes: restored.resident_bytes,
            summary: restored.summary,
        };
        self.lock().insert(session, mirror);
    }

    /// The newest rows in append order, streamed text included.
    pub(super) fn tail(&self, session: SessionId) -> Vec<SessionLogEntry> {
        self.lock()
            .get(&session)
            .map(|mirror| mirror.window.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// The durable records at or after an absolute ordinal, up to a count.
    pub(super) fn page(
        &self,
        session: SessionId,
        from: TrajectoryPosition,
        limit: usize,
    ) -> WindowRead {
        let mirrors = self.lock();
        let Some(mirror) = mirrors.get(&session) else {
            return WindowRead::Absent;
        };
        if from.index < mirror.base_index {
            return WindowRead::Below {
                base_index: mirror.base_index,
                epoch_byte_offset: mirror.epoch_byte_offset,
            };
        }
        let events = mirror.page(from.index, limit);
        let next = from.index + events.len();
        WindowRead::Held { events, next }
    }

    /// The id of the latest row, or None when the session has no mirror.
    /// Reads a counter, not the window.
    pub(super) fn last_id(&self, session: SessionId) -> Option<EventId> {
        self.lock()
            .get(&session)
            .and_then(|mirror| mirror.last_event_id)
    }

    /// The id of the latest durable user input, or None when the session holds
    /// none yet. Reads a counter, so an input the window has trimmed still
    /// answers.
    pub(super) fn last_user_input_id(&self, session: SessionId) -> Option<EventId> {
        self.lock()
            .get(&session)
            .and_then(|mirror| mirror.last_user_input_id)
    }

    /// The session's trajectory revision and whole-session summary under one
    /// lock, so the two describe the same revision.
    pub(super) fn head(&self, session: SessionId) -> TrajectoryHead {
        self.lock()
            .get(&session)
            .map(SessionMirror::head)
            .unwrap_or_default()
    }

    /// Drop a session's mirror, leaving the append-only log untouched.
    pub(super) fn reset(&self, session: SessionId) {
        self.lock().remove(&session);
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<SessionId, SessionMirror>> {
        self.inner.lock().expect("session mirrors mutex poisoned")
    }
}

/// A history folded from a session's log: the rows the window keeps, the
/// counts the whole epoch adds up to, and where in the log it begins.
pub(super) struct RestoredHistory {
    window: VecDeque<SessionLogEntry>,
    /// Durable ordinals on the session's current epoch, records the reader
    /// could not parse included.
    rows: usize,
    /// Rows the window no longer holds below it: the rows trimming evicted, and
    /// the rows a window that started over left behind.
    dropped_rows: usize,
    /// The ordinal the window's oldest row holds. Trimming raises this by one
    /// per evicted row, and a window that starts over takes the ordinal of the
    /// row that opened its run. A reader maps its ordinals into the window from
    /// here, so the run has to be the window's own.
    base_index: usize,
    /// True when a record the reader could not read stands after the window's
    /// run of ordinals, so a row folded in later begins a run of its own rather
    /// than extending this one.
    gap_after_run: bool,
    epoch_byte_offset: Option<u64>,
    last_durable_id: Option<EventId>,
    last_event_id: Option<EventId>,
    last_user_input_id: Option<EventId>,
    epoch_event_id: Option<EventId>,
    resident_bytes: usize,
    /// Bytes the fold passed over because no complete record fit the log's
    /// read window. Reported so a history that lost records is not read as a
    /// history that had none.
    skipped_bytes: u64,
    summary: TrajectorySummaryState,
    budget_bytes: usize,
}

impl RestoredHistory {
    /// Fold a replayed history, for a backend that keeps no byte log. The rows
    /// are all there is, so the window keeps them all.
    pub(super) fn from_events(events: Vec<SessionLogEntry>) -> Self {
        let mut history = Self::new(usize::MAX, None);
        for event in events {
            history.push(event);
        }
        history
    }

    /// A history holding nothing yet, over the record budget given. The byte
    /// position the epoch starts at is the reading caller's to name: a fold of
    /// the log names the head it began at, and a fold of a replayed list has
    /// no byte position to name.
    pub(super) fn new(budget_bytes: usize, epoch_byte_offset: Option<u64>) -> Self {
        Self {
            window: VecDeque::new(),
            rows: 0,
            dropped_rows: 0,
            base_index: 0,
            gap_after_run: false,
            epoch_byte_offset,
            last_durable_id: None,
            last_event_id: None,
            last_user_input_id: None,
            epoch_event_id: None,
            resident_bytes: 0,
            skipped_bytes: 0,
            summary: TrajectorySummaryState::default(),
            budget_bytes,
        }
    }

    /// Fold one record in, trimming the window as it grows. The counts are the
    /// epoch's, so they survive the trim.
    pub(super) fn push(&mut self, event: SessionLogEntry) {
        if self.gap_after_run {
            // The ordinals between the window's rows and this record hold no
            // row at all, so this record begins a run of its own: the rows
            // below the gap cannot join it, and each one is a row the window
            // no longer holds.
            self.dropped_rows += self.window.len();
            self.window.clear();
            self.resident_bytes = 0;
            self.base_index = self.rows;
            self.gap_after_run = false;
        }
        self.rows += 1;
        if self.last_durable_id.is_none() {
            self.epoch_event_id = Some(event.id);
        }
        self.last_durable_id = Some(event.id);
        self.last_event_id = Some(event.id);
        if matches!(event.event, SessionEvent::UserInput { .. }) {
            self.last_user_input_id = Some(event.id);
        }
        self.summary.record(&event);
        self.resident_bytes += resident_bytes(&event);
        self.window.push_back(event);
        while self.resident_bytes > self.budget_bytes && self.window.len() > 1 {
            let dropped = self.window.pop_front().expect("a row is held");
            self.resident_bytes = self.resident_bytes.saturating_sub(resident_bytes(&dropped));
            self.dropped_rows += 1;
            self.base_index += 1;
        }
    }

    /// Count a record the reader could not read: it holds no row, so the
    /// ordinals it took stay in the count while the window's rows cannot reach
    /// past it. The rows folded so far keep their own ordinals and are kept;
    /// the row that comes after the record is where the window starts over.
    pub(super) fn count_unread(&mut self) {
        self.rows += 1;
        self.gap_after_run = true;
    }

    /// Note bytes the fold passed over inside a record it did read, which is
    /// the tail of a record wider than one read.
    pub(super) fn note_skipped(&mut self, bytes: u64) {
        self.skipped_bytes = self.skipped_bytes.saturating_add(bytes);
    }

    /// Start a new epoch at a clear boundary: the rows folded so far belong to
    /// the history the clear stopped counting. The boundary record itself
    /// opens the epoch, the way a drained replay does.
    pub(super) fn restart_epoch(&mut self, byte_offset: u64) {
        self.window.clear();
        self.rows = 0;
        self.dropped_rows = 0;
        self.base_index = 0;
        self.gap_after_run = false;
        self.epoch_byte_offset = Some(byte_offset);
        self.last_durable_id = None;
        self.last_event_id = None;
        self.last_user_input_id = None;
        self.epoch_event_id = None;
        self.resident_bytes = 0;
        self.summary = TrajectorySummaryState::default();
    }

    /// Durable ordinals the folded epoch holds.
    pub(super) fn rows(&self) -> usize {
        self.rows
    }

    /// Bytes the fold passed over as unreadable.
    pub(super) fn skipped_bytes(&self) -> u64 {
        self.skipped_bytes
    }
}

/// The bytes an event is charged against the resident budget: the record plus
/// the text it carries, which is what its size follows.
///
/// The exact figure is the backend's and is not reachable from here (the
/// backend serializes the record as it writes it). This estimate is what the
/// budget needs: a window that bounds a session of large tool results the same
/// way it bounds one of small ones.
fn resident_bytes(entry: &SessionLogEntry) -> usize {
    size_of::<SessionLogEntry>() + event_bytes(&entry.event)
}

/// The bytes an event's own payload holds.
fn event_bytes(event: &SessionEvent) -> usize {
    match event {
        SessionEvent::UserInput { text }
        | SessionEvent::MetaUser { text }
        | SessionEvent::MidTurnInput { text, .. }
        | SessionEvent::MemoryRecall { text, .. }
        | SessionEvent::SkillListing { text, .. }
        | SessionEvent::AssistantTextDelta { text }
        | SessionEvent::Reasoning { text }
        | SessionEvent::Summary { text } => text.len(),
        SessionEvent::SkillBody { content, .. } => content.len(),
        SessionEvent::AssistantMessage { text, thinking } => {
            text.len() + thinking.as_deref().map_or(0, str::len)
        }
        SessionEvent::ToolCall {
            call_id,
            tool,
            input,
        } => call_id.len() + tool.len() + value_bytes(input),
        SessionEvent::ToolResult {
            call_id, output, ..
        } => call_id.len() + value_bytes(output),
        // Every other event carries counters and identities, whose size the
        // record itself already covers.
        _ => 0,
    }
}

/// The bytes a JSON payload holds, walked rather than serialized: a
/// serialization per append would cost more than the bound it informs.
fn value_bytes(value: &Value) -> usize {
    match value {
        Value::String(text) => text.len(),
        Value::Array(items) => items.iter().map(value_bytes).sum(),
        Value::Object(fields) => fields
            .iter()
            .map(|(name, field)| name.len() + value_bytes(field))
            .sum(),
        _ => 0,
    }
}

/// Whether an event is a streamed display row rather than a durable fact.
fn is_display_delta(event: &SessionEvent) -> bool {
    matches!(event, SessionEvent::AssistantTextDelta { .. })
}
