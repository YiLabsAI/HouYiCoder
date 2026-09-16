//! Session subsystem root. SessionConnection is the live engine
//! connection: it owns the command channel, message receiver, request-id
//! counter, and driver task.

mod connection;

pub use connection::{RequestIdExhausted, SessionConnection};
