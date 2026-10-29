//! A log of the user notices one server emitted, so tests can assert what the
//! user was told rather than only what the durable records say.

use houyicoder_api::agent_event::{AgentEventHandlers, EventHandler, UserNoticeEvent};
use houyicoder_core::agent::Runner;
use std::sync::{Arc, Mutex};

/// Notices collected in emission order, plus the handler that collects them.
pub(in crate::server) struct NoticeLog {
    seen: Arc<Mutex<Vec<String>>>,
    handler: Arc<dyn EventHandler<UserNoticeEvent>>,
}

impl NoticeLog {
    /// An empty log with the handler that fills it.
    pub(in crate::server) fn new() -> Self {
        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        let handler: Arc<dyn EventHandler<UserNoticeEvent>> =
            Arc::new(move |event: UserNoticeEvent| {
                sink.lock().expect("notice lock").push(event.message);
            });
        Self { seen, handler }
    }

    /// Install the collector on the runner a test server is built over, so
    /// the server's notices land here. Without this the runner has no notice
    /// handler and every notice is dropped.
    pub(in crate::server) fn attach(&self, runner: &mut Runner) {
        let mut handlers = AgentEventHandlers::default();
        handlers.set_user_notice(self.handler.clone());
        runner.set_event_handlers(handlers);
    }

    /// The messages collected so far, in the order they were emitted.
    pub(in crate::server) fn messages(&self) -> Vec<String> {
        self.seen.lock().expect("notice lock").clone()
    }
}
