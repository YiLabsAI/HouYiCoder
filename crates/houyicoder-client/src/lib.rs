//! Protocol client: the L4 layer a frontend (TUI, IDE, web) holds to speak
//! the wire protocol to a service. The client owns three concerns over the
//! connection: pairing a response to its request by id, tracking the resume
//! cursor for reconnect, and the transports that carry the frames. The
//! transports live here (in-memory channel for mode A, pipes for mode B,
//! domain socket for detach), backed by the wire types in protocol. The
//! client never imports engine types; it speaks typed wire messages only.

pub mod client;
pub mod transport;

pub use client::Client;
#[cfg(unix)]
pub use transport::UdsTransport;
pub use transport::{InProcTransport, Transport};
