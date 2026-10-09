//! Session subsystem root. SessionConnection is the live engine
//! connection: it owns the command channel, message receiver, request-id
//! counter, and driver task. Session is the presented session, pairing the
//! stable identity with that optional live connection.

use crate::session::identity::SessionIdentity;

pub(crate) mod capabilities;
mod connection;
pub(crate) mod identity;
pub(crate) mod persistence;

pub use connection::{
    ConnectionStatus, EnqueueError, PollOutcome, RequestIdExhausted, SessionConnection,
};

/// The presented session: its stable identity and the optional live engine
/// connection. The identity is always present. The connection is a
/// capability the session holds while connected and may lose without
/// losing the identity or the already-observed state; the loss is carried
/// inside SessionConnection as ConnectionStatus::Lost rather than by
/// dropping the session.
pub(crate) struct Session {
    /// The session's routing id and the starting working directory, stable
    /// for the session's lifetime.
    pub(crate) identity: SessionIdentity,
    /// The live connection, present while connected and None in the bare
    /// or not-yet-connected path.
    pub(crate) connection: Option<SessionConnection>,
}

impl Session {
    /// A session with an identity and no connection, the state the default
    /// app lands in before the composition root attaches one.
    pub(crate) fn bare(identity: SessionIdentity) -> Self {
        Self {
            identity,
            connection: None,
        }
    }
}
