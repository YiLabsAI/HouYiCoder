//! The direct shell command a session is running, if any.

use std::mem::replace;

use houyicoder_protocol::envelope::RequestId;

/// The one direct shell command a session may have in flight.
///
/// A command does not join the model run lifecycle: the host runs it with no
/// model request, so no frame streams and no turn is assembled. It still needs
/// its own state, because the request id settles only its own reply, the input
/// box keeps taking text while it runs, and the Esc key has to know which work
/// it stops.
#[derive(Debug, Default)]
pub(crate) enum ShellRunState {
    /// No command is running.
    #[default]
    Idle,
    /// A command is running. The reply carrying this request id settles it.
    Running { request: RequestId },
    /// The user asked to stop the command and its reply is still on its way.
    Cancelling { request: RequestId },
}

impl ShellRunState {
    /// True while a command is running or waiting to be stopped.
    pub(crate) fn is_active(&self) -> bool {
        !matches!(self, Self::Idle)
    }

    /// True once the user asked to stop the running command.
    pub(crate) fn is_cancelling(&self) -> bool {
        matches!(self, Self::Cancelling { .. })
    }

    /// The request id the host will answer, or None when no command runs.
    pub(crate) fn request_id(&self) -> Option<RequestId> {
        match self {
            Self::Idle => None,
            Self::Running { request } | Self::Cancelling { request } => Some(*request),
        }
    }

    /// Record a command the frontend just submitted.
    pub(crate) fn begin(&mut self, request: RequestId) {
        *self = Self::Running { request };
    }

    /// Running → Cancelling. A no-op when idle, so a late Esc cannot revive a
    /// command that already answered.
    pub(crate) fn begin_cancel(&mut self) {
        let prev = replace(self, Self::Idle);
        *self = match prev {
            Self::Running { request } => Self::Cancelling { request },
            other => other,
        };
    }

    /// Clear the state when the reply answers this request. Returns whether it
    /// did, so a reply for an older command cannot clear a newer one.
    pub(crate) fn settle(&mut self, request: RequestId) -> bool {
        if self.request_id() != Some(request) {
            return false;
        }
        self.finish();
        true
    }

    /// Any state → Idle. The reply that would have settled the command can no
    /// longer arrive, so the caller reports the loss itself.
    pub(crate) fn finish(&mut self) {
        *self = Self::Idle;
    }
}

#[cfg(test)]
mod tests;
