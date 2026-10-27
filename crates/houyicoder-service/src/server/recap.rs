//! Recaps: when an idle session may be summarized, and the request that says it.
//!
//! A recap spends a model request while the user is not looking, so the host
//! decides every part of it: whether the feature is on, which clients are
//! attached and focused, whether a turn is running, and whether this point in
//! the session was already summarized. A terminal reports only its own focus.
//! Losing the last focused client cancels an away recap; opening an idle
//! session through the resume path does not, since gaining focus is that open.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::PoisonError;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use houyicoder_api::provider::ModelProvider;
use houyicoder_api::session::SessionLog;
use houyicoder_async::CancellationToken;
use houyicoder_context::RecapSource;
use houyicoder_context::SessionId;
use tokio::time::Instant;

/// How long every attached client must have been unfocused before an away
/// recap is generated.
pub const AWAY_AFTER: Duration = Duration::from_secs(5 * 60);

/// The wall-clock ceiling of one recap request.
pub const REQUEST_DEADLINE: Duration = Duration::from_secs(10);

/// The most output tokens one recap request may ask for.
pub const OUTPUT_TOKENS: u32 = 160;

/// The most bytes one recap text may take in the transcript. A response over
/// this is refused rather than cut: a sentence shortened to fit would report
/// work that never happened.
pub const OUTPUT_BYTES: usize = 2 * 1024;

/// What the recap path needs from settings. A disabled feature or a session
/// with no resolved model spends nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecapSettings {
    pub enabled: bool,
    /// The catalog entry the request names. None skips the recap: the main
    /// model is never a fallback, because that is the expensive path this
    /// feature exists to avoid.
    pub model: Option<String>,
}

impl RecapSettings {
    /// Whether a recap may run at all for this session.
    pub fn usable(&self) -> bool {
        self.enabled && self.model.is_some()
    }
}

/// The two ways a recap is entered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Entry {
    /// The user left the session open and walked away.
    Away,
    /// The session was opened through the resume path.
    Resume,
}

impl Entry {
    /// The durable source this entry records.
    pub fn durable_source(self) -> RecapSource {
        match self {
            Entry::Away => RecapSource::Away,
            Entry::Resume => RecapSource::Resume,
        }
    }
}

/// What one client last reported about its focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    /// Attached but has never reported: an unknown client keeps the away
    /// timer off, because a client that is looking must not be summarized.
    Unknown,
    Focused,
    Unfocused,
}

/// A recap the state machine authorized, with the token its cancellation
/// acts on.
pub struct RecapJob {
    pub entry: Entry,
    pub cancel: CancellationToken,
}

struct Running {
    entry: Entry,
    cancel: CancellationToken,
}

struct State {
    clients: HashMap<u64, Focus>,
    away_deadline: Option<Instant>,
    /// Turns in progress. A count rather than a flag: two connections can run
    /// a turn each, and the first to finish must not restart the away clock
    /// while the other still works.
    running_turns: usize,
    resume_pending: bool,
    running: Option<Running>,
}

impl State {
    fn new() -> Self {
        Self {
            clients: HashMap::new(),
            away_deadline: None,
            running_turns: 0,
            resume_pending: false,
            running: None,
        }
    }

    /// Whether the away entry may fire: at least one client is attached and
    /// every attached client reported itself unfocused.
    fn all_unfocused(&self) -> bool {
        !self.clients.is_empty()
            && self
                .clients
                .values()
                .all(|focus| *focus == Focus::Unfocused)
    }

    fn cancel(&mut self, entry: Entry) {
        if let Some(running) = &self.running
            && running.entry == entry
        {
            running.cancel.cancel();
            self.running = None;
        }
    }

    /// Re-derive the away deadline from the current focus and turn state.
    fn settle(&mut self, now: Instant) {
        if !self.all_unfocused() {
            self.away_deadline = None;
            self.cancel(Entry::Away);
            return;
        }
        if self.running_turns > 0 {
            self.away_deadline = None;
            return;
        }
        self.away_deadline.get_or_insert(now + AWAY_AFTER);
    }
}

/// The host-side state of the recap feature for one session.
///
/// One instance belongs to a session and outlives every connection, so the
/// feature keeps one timer and one request per session rather than one per
/// client. A client that reconnects attaches as a new client and reports its
/// focus again; until it does, its unknown focus keeps the away timer off.
/// The state is behind a lock held only for bookkeeping, never across a model
/// call: the request runs in its own task and is stopped through its cancel
/// token. Its clock is the runtime's, so a deadline can be observed without
/// waiting the window out.
pub struct RecapSession {
    session: SessionId,
    settings: RecapSettings,
    provider: Arc<dyn ModelProvider>,
    log: Arc<dyn SessionLog>,
    state: Mutex<State>,
}

impl RecapSession {
    /// Build the state for one session. The provider is the session's own, so
    /// a recap never reaches an endpoint the session's traffic does not
    /// already use.
    pub fn new(
        session: SessionId,
        settings: RecapSettings,
        provider: Arc<dyn ModelProvider>,
        log: Arc<dyn SessionLog>,
    ) -> Self {
        Self {
            session,
            settings,
            provider,
            log,
            state: Mutex::new(State::new()),
        }
    }

    /// The session this state belongs to.
    pub fn session(&self) -> SessionId {
        self.session
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Register an attached client. It starts out with unknown focus, so it
    /// keeps the away timer off until it reports. False when the feature is
    /// off or unconfigured, in which case nothing was registered.
    pub fn attach(&self, client: u64) -> bool {
        if !self.settings.usable() {
            return false;
        }
        let mut state = self.lock();
        state.clients.insert(client, Focus::Unknown);
        state.away_deadline = None;
        true
    }

    /// Drop an attached client. The last one leaving cancels whatever is in
    /// flight: with nobody attached the session is closing, not away.
    pub fn detach(&self, client: u64, now: Instant) {
        let mut state = self.lock();
        state.clients.remove(&client);
        if state.clients.is_empty() {
            state.away_deadline = None;
            state.resume_pending = false;
            if let Some(running) = state.running.take() {
                running.cancel.cancel();
            }
            return;
        }
        state.settle(now);
    }

    /// Record one client's focus report.
    pub fn report_focus(&self, client: u64, focused: bool, now: Instant) {
        if !self.settings.usable() {
            return;
        }
        let mut state = self.lock();
        if !state.clients.contains_key(&client) {
            return;
        }
        let focus = if focused {
            Focus::Focused
        } else {
            Focus::Unfocused
        };
        state.clients.insert(client, focus);
        state.settle(now);
    }

    /// A turn started: nothing may be generated across it, and a resume recap
    /// waiting to run is no longer about an idle session.
    pub fn turn_started(&self) {
        let mut state = self.lock();
        state.running_turns = state.running_turns.saturating_add(1);
        state.away_deadline = None;
        state.resume_pending = false;
        state.cancel(Entry::Away);
        state.cancel(Entry::Resume);
    }

    /// A turn finished: if every client is still unfocused and no other turn
    /// runs, the away clock starts now rather than during the turn.
    pub fn turn_finished(&self, now: Instant) {
        let mut state = self.lock();
        state.running_turns = state.running_turns.saturating_sub(1);
        state.settle(now);
    }

    /// The session was opened through the resume path and nothing summarized
    /// it yet. It runs even while a client is focused.
    pub fn resume_entry(&self) {
        if !self.settings.usable() {
            return;
        }
        let mut state = self.lock();
        if state.running_turns > 0 {
            return;
        }
        state.resume_pending = true;
    }

    /// When the caller must call take_due again, or None when no entry is
    /// armed.
    pub fn next_due(&self, now: Instant) -> Option<Instant> {
        let state = self.lock();
        if state.running.is_some() {
            return None;
        }
        if state.resume_pending {
            return Some(now);
        }
        state.away_deadline
    }

    /// Take the entry that is due, if any, and mark it running.
    pub fn take_due(&self, now: Instant) -> Option<RecapJob> {
        let mut state = self.lock();
        if state.running.is_some() || !self.settings.usable() {
            return None;
        }
        if state.resume_pending {
            state.resume_pending = false;
            // The resume entry summarizes the current point, so an away
            // deadline armed for that same point would only queue a duplicate
            // behind it. New work re-arms the clock when its turn ends.
            state.away_deadline = None;
            return Some(Self::begin(&mut state, Entry::Resume));
        }
        match state.away_deadline {
            Some(deadline) if now >= deadline => {
                state.away_deadline = None;
                Some(Self::begin(&mut state, Entry::Away))
            }
            _ => None,
        }
    }

    fn begin(state: &mut State, entry: Entry) -> RecapJob {
        let cancel = CancellationToken::new();
        state.running = Some(Running {
            entry,
            cancel: cancel.clone(),
        });
        RecapJob { entry, cancel }
    }

    /// The job for this entry finished, whatever its outcome. A job cancelled
    /// during its run was already cleared when the cancellation fired.
    pub fn finish(&self, entry: Entry) {
        let mut state = self.lock();
        if let Some(running) = &state.running
            && running.entry == entry
        {
            state.running = None;
        }
    }

    /// Mark a turn in progress until the guard drops. Every exit path,
    /// including one that unwinds, ends the turn, so a connection that fails
    /// mid-run does not leave the session looking busy forever.
    pub fn turn_scope(self: &Arc<Self>) -> TurnScope {
        self.turn_started();
        TurnScope {
            recap: Arc::clone(self),
        }
    }

    /// Mark one entry's job running until the guard drops. A panic inside the
    /// job still releases the entry, so one failed request cannot stop every
    /// later one.
    pub fn job_scope(self: &Arc<Self>, entry: Entry) -> JobScope {
        JobScope {
            recap: Arc::clone(self),
            entry,
        }
    }

    /// Whether a job for this entry is running.
    pub fn is_running(&self, entry: Entry) -> bool {
        self.lock()
            .running
            .as_ref()
            .is_some_and(|running| running.entry == entry)
    }

    /// The provider and log one request uses.
    pub(crate) fn request_handles(&self) -> (Arc<dyn ModelProvider>, Arc<dyn SessionLog>) {
        (Arc::clone(&self.provider), Arc::clone(&self.log))
    }

    /// The model id a request may name, present only when the feature is
    /// usable.
    pub(crate) fn model(&self) -> Option<String> {
        if !self.settings.usable() {
            return None;
        }
        self.settings.model.clone()
    }
}

#[cfg(test)]
use houyicoder_memory::InMemoryBackend;
#[cfg(test)]
use houyicoder_provider::FakeProvider;
#[cfg(test)]
use houyicoder_session::SessionStore;

#[cfg(test)]
impl RecapSession {
    /// State for a test that carries the session handle without exercising the
    /// recap. The feature is off, so no request is authorized and neither
    /// collaborator is reached.
    pub(crate) fn inert(session: SessionId) -> Self {
        let log: Arc<dyn SessionLog> =
            Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
        Self::new(
            session,
            RecapSettings {
                enabled: false,
                model: None,
            },
            Arc::new(FakeProvider::text("unused")),
            log,
        )
    }
}

/// One turn's hold on a session's recap state. Dropping it ends the turn and
/// restarts the away clock if every client is unfocused.
pub struct TurnScope {
    recap: Arc<RecapSession>,
}

impl Drop for TurnScope {
    fn drop(&mut self) {
        self.recap.turn_finished(Instant::now());
    }
}

/// One entry's hold on a session's recap state. Dropping it releases the
/// entry whether the job returned or unwound.
pub struct JobScope {
    recap: Arc<RecapSession>,
    entry: Entry,
}

impl Drop for JobScope {
    fn drop(&mut self) {
        self.recap.finish(self.entry);
    }
}

/// One client's attachment to a session's recap state. Dropping it detaches
/// the client, and the last one leaving cancels what the session was
/// preparing: nobody is left to read a summary.
pub struct RecapAttachment {
    recap: Arc<RecapSession>,
    client: u64,
}

impl RecapAttachment {
    /// Attach a client to the session's recap state. None when the feature is
    /// off for this session, so no client is registered and nothing is spent.
    pub fn attach(recap: Arc<RecapSession>) -> Option<Self> {
        static NEXT_CLIENT: AtomicU64 = AtomicU64::new(1);
        let client = NEXT_CLIENT.fetch_add(1, Ordering::Relaxed);
        recap.attach(client).then_some(Self { recap, client })
    }

    /// Record this client's focus. The host owns the timer; a terminal only
    /// reports whether it is looking.
    pub fn report_focus(&self, focused: bool, now: Instant) {
        self.recap.report_focus(self.client, focused, now);
    }
}

impl Drop for RecapAttachment {
    fn drop(&mut self) {
        self.recap.detach(self.client, Instant::now());
    }
}

pub(crate) mod job;

#[cfg(test)]
mod tests;
