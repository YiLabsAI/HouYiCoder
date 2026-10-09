//! Session subsystem root. SessionConnection is the live engine
//! connection: it owns the command channel, message receiver, request-id
//! counter, and driver task.

pub(crate) mod capabilities;
mod connection;
pub(crate) mod persistence;

pub use connection::{
    ConnectionStatus, EnqueueError, PollOutcome, RequestIdExhausted, SessionConnection,
};
