//! The process-wide registry of live sessions: which runners a reattaching
//! connection can re-hydrate from, plus the two process-scoped values that
//! outlive any one connection. Kept apart from the assembly steps that build a
//! runner, because holding a session and constructing one change for different
//! reasons.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;

use tokio::sync::Notify;

use houyicoder_context::SessionId;
use houyicoder_permission::ModeGate;
use houyicoder_provider::ProviderFacts;

use crate::lifecycle::SessionLeaseStore;
use crate::server::EventSequencer;
use crate::server::recap::RecapSession;

use houyicoder_core::agent::multi_agent::bus_types::AgentBus;
use houyicoder_core::snapshot::file_transaction::FileTransaction;

use super::Runner;
use super::background_tasks::BackgroundTasks;

/// Runner and event-stream state retained across client connections. The
/// sequencer preserves reliable history, sequence allocation, and the durable
/// log cursor while each attachment supplies a new carrier.
struct LiveRunnerHandle {
    runner: Arc<Runner>,
    event_sequencer: EventSequencer,
    gate: Arc<dyn ModeGate>,
    append_notify: Arc<Notify>,
    recap: Arc<RecapSession>,
}

/// The session-indexed host a reattaching connection re-hydrates from. Holds
/// the live runners + cursors (so the Arc<Runner> survives a disconnect) and
/// the SessionLeaseStore (the single source of truth for the parked
/// PendingTurn + the lease holder + the lifecycle state). serve_session
/// reads the handle for this session, rebuilds a Server, and drives serve;
/// when the serve ends on disconnect the host retains everything.
///
/// The gate is per-runner (composition), so it lives in the handle, not on
/// the host — a multi-session host holds one gate per live runner.
pub struct SessionHost {
    runners: Mutex<HashMap<SessionId, LiveRunnerHandle>>,
    store: SessionLeaseStore,
    /// The facts the process's provider was built from. Process-scoped
    /// rather than per-session because the provider is resolved once for the
    /// life of a process; serve_session reads it so a reattached connection
    /// reports the same facts the live one does.
    provider_facts: Option<ProviderFacts>,
    /// The commands the foreground gave up. The host retains the one registry
    /// the process was built with, so a reattached connection for that session
    /// lists and kills the tasks the detached run is still running. The host
    /// does not yet key a registry per session: today one process assembles
    /// one session, so one registry serves the reattach path.
    background_tasks: Option<Arc<BackgroundTasks>>,
    /// The process's multi-agent bus. A reattached connection needs it for the
    /// same reason the live one does: a child's permission ask arrives on this
    /// bus, and a serve loop built without it never raises the card.
    bus: Option<Arc<AgentBus>>,
    /// The write journal the assembled runner records through. A reattached
    /// connection needs it to list and restore the writes the detached run
    /// made; without it the pane has no record to show.
    file_transaction: Option<Arc<FileTransaction>>,
}

impl SessionHost {
    /// In-memory host with no persisted backing. The single-process
    /// fast path: a new process cannot reload sessions (cross-process
    /// reconnect belongs to the local endpoint layer).
    pub fn new(store: SessionLeaseStore) -> Self {
        Self {
            runners: Mutex::new(HashMap::new()),
            store,
            provider_facts: None,
            background_tasks: None,
            bus: None,
            file_transaction: None,
        }
    }

    /// Attach what the process's provider was built from, so a reattaching
    /// connection's status reports the real destination.
    pub fn with_provider_facts(mut self, facts: ProviderFacts) -> Self {
        self.provider_facts = Some(facts);
        self
    }

    /// Attach the registry holding the process's background commands, so a
    /// reattached connection lists and kills the tasks the detached session
    /// is still running instead of starting from an empty pane.
    pub fn with_background_tasks(mut self, tasks: Arc<BackgroundTasks>) -> Self {
        self.background_tasks = Some(tasks);
        self
    }

    /// Attach the process's multi-agent bus, so a reattached connection routes
    /// a child's permission ask instead of dropping it.
    pub fn with_bus(mut self, bus: Option<Arc<AgentBus>>) -> Self {
        self.bus = bus;
        self
    }

    /// The bus a reattaching connection routes child asks on.
    pub(crate) fn bus(&self) -> Option<&Arc<AgentBus>> {
        self.bus.as_ref()
    }

    /// Attach the write journal, so a reattached connection lists and restores
    /// the writes the detached run made.
    pub fn with_file_transaction(mut self, journal: Option<Arc<FileTransaction>>) -> Self {
        self.file_transaction = journal;
        self
    }

    /// The write journal a reattaching connection reads.
    pub(crate) fn file_transaction(&self) -> Option<&Arc<FileTransaction>> {
        self.file_transaction.as_ref()
    }

    /// The facts a reattaching connection should report, or None when the
    /// host was built without them.
    pub(crate) fn provider_facts(&self) -> Option<&ProviderFacts> {
        self.provider_facts.as_ref()
    }

    /// The background command registry a reattaching connection answers from,
    /// or None when the host was built without one.
    pub(crate) fn background_tasks(&self) -> Option<&Arc<BackgroundTasks>> {
        self.background_tasks.as_ref()
    }

    /// Register a runner, its session-scoped event sequencer, and its recap
    /// state.
    pub fn insert(
        &self,
        session: SessionId,
        runner: Arc<Runner>,
        event_sequencer: EventSequencer,
        gate: Arc<dyn ModeGate>,
        append_notify: Arc<Notify>,
        recap: Arc<RecapSession>,
    ) {
        self.runners.lock().expect("host lock").insert(
            session,
            LiveRunnerHandle {
                runner,
                event_sequencer,
                gate,
                append_notify,
                recap,
            },
        );
    }

    /// Clone the runner, sequencer, gate, append notification, and recap state
    /// needed by a reattaching connection. None when the session is not
    /// registered.
    pub(crate) fn clone_handle(&self, session: SessionId) -> Option<RunnerHandleClone> {
        self.runners
            .lock()
            .expect("host lock")
            .get(&session)
            .map(|h| RunnerHandleClone {
                runner: h.runner.clone(),
                event_sequencer: h.event_sequencer.clone(),
                gate: h.gate.clone(),
                append_notify: h.append_notify.clone(),
                recap: h.recap.clone(),
            })
    }

    /// Single source of truth for the parked PendingTurn, lease holder, and
    /// state. pub(crate) so serve_session runs the lease guard.
    pub(crate) fn store(&self) -> &SessionLeaseStore {
        &self.store
    }
}

/// A copied snapshot of a session's live handle. pub(crate) so the
/// serve_session entry point re-hydrates a Server from it without the
/// host exposing its internal LiveRunnerHandle.
pub(crate) struct RunnerHandleClone {
    pub(crate) runner: Arc<Runner>,
    pub(crate) event_sequencer: EventSequencer,
    pub(crate) gate: Arc<dyn ModeGate>,
    pub(crate) append_notify: Arc<Notify>,
    pub(crate) recap: Arc<RecapSession>,
}

#[cfg(test)]
mod tests;
