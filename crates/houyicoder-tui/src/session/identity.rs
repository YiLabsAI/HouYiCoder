//! The presented session's identity, reset when the session changes.

use houyicoder_protocol::frontend::SessionId;

/// The identity of the presented session: its wire routing id and the
/// working directory the session started in. Both reset together on a
/// session switch, so they travel as one value rather than two loose App
/// fields a swap must reset in step.
pub(crate) struct SessionIdentity {
    /// The active session id passed to the server over the wire.
    pub(crate) session_id: SessionId,
    /// The original working directory the session started in, shown at the
    /// top of the Workspace tab in /permissions.
    pub(crate) working_dir: String,
}
