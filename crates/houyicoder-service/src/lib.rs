//! The composition root and protocol server. L3 in the layering: the only
//! place that constructs concrete engine implementations, and the protocol
//! server a frontend client connects to. Three modules with distinct
//! concerns, kept apart so the crate does not become a new god-module:
//!
//! - composition: dependency-injection assembly — the single site that
//!   constructs the Runner, providers, tools, sandbox, memory, session, and
//!   gate. Only the entry calls it.
//! - server: the protocol server — handshake, request/event routing, the
//!   resume cursor.
//! - lifecycle: session-record and runner-host management, the lifecycle
//!   state machine, registry reads and writes.
//!
//! The crate may depend on core and every L1 and L0 (it is the composition
//! root); it must not depend on the client or the TUI, which are above it.

#![allow(dead_code)] // crate root re-exports service modules consumed by other crates; locally unused

pub mod acp_adapter;
pub mod acp_serve;
pub mod acp_server;
pub mod composition;
pub mod diagnostics;
pub mod lifecycle;
pub mod projection;
pub mod sandbox_policy;
pub mod server;
#[cfg(unix)]
pub mod uds;
