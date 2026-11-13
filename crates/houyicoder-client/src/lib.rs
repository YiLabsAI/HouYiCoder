//! Protocol client: the L4 layer a frontend (TUI, IDE, web) holds to speak
//! the wire protocol to a service. The client owns three concerns over the
//! connection: pairing a response to its request by id, tracking the resume
//! cursor for reconnect, and the transports that carry the frames. The
//! transports live here (an in-memory channel, and a stdio carrier that also
//! serves a detached session's local endpoint), backed by the wire types in
//! protocol. The client never imports engine types; it speaks typed wire
//! messages only.

pub mod client;
pub mod transport;

pub use client::Client;
pub use transport::{InProcTransport, StdioTransport, Transport};
