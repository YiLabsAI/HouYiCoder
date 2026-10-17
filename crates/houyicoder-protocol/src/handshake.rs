//! Hello handshake — the first frame both ends exchange on connect. It
//! carries the protocol version and capability flags; a version mismatch or
//! an unsupported required capability fails the handshake explicitly so a
//! peer never enters a half-working session.

use crate::envelope::EventSeq;
use crate::error::{ErrorCategory, ProtocolError};
use serde::{Deserialize, Serialize};

/// The protocol version. Bumped only on a breaking change to the message
/// set or framing; a peer that sees a different version fails the handshake
/// rather than guessing. v5: ModelInfo/ModelResult payloads changed shape
/// (selected/applied/resolved_default entries and ModelApplyResult replace
/// the v4 active_id/catalog and ModelApplied forms) — old and new v4 peers
/// would handshake through and then fail payload decode. v6: the
/// run-completion notification carries its duration as ms in place of secs.
/// The old field is gone rather than supplemented, so a mixed-version peer
/// would handshake through and then silently drop every turn's duration
/// label; an added variant can degrade on its own, a renamed field cannot.
/// v7: the memory-operation tag Stored was replaced by Created and Updated. A
/// same-version peer without the operation enum's catch-all would drop every
/// memory-change frame at decode; the version gate keeps such a peer from
/// half-working, and a new receiver still reads a legacy stored tag as
/// Unknown. v8: the agents reply payload changed shape, from the prompt's
/// Markdown string to structured entries. A same-version peer would handshake
/// through and then fail every agents reply at decode, leaving the pane
/// waiting on a reply that cannot arrive.
pub const PROTOCOL_VERSION: u16 = 8;

/// Capabilities a peer advertises in Hello. Added only when a real optional
/// feature needs negotiation; absent means the peer does not support it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Capabilities {
    /// Streaming token deltas (vs non-streaming complete turns).
    pub streaming: bool,
    /// Large-output CAS isolation (block_put / block_get) support.
    pub cas: bool,
    /// Mode-B detach support (the peer can survive a frontend disconnect).
    pub detach: bool,
}

/// The Hello frame: protocol version, capabilities, and the client's event
/// replay cursor. A fresh client requests the complete reliable journal; a
/// reconnecting client resumes after its highest processed sequence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub protocol_version: u16,
    pub capabilities: Capabilities,
    /// The highest event sequence processed by this client. New peers use this
    /// cursor because one trajectory entry may project zero, one, or multiple
    /// wire events. None requests the complete reliable event journal.
    #[serde(default)]
    pub last_event_seq: Option<EventSeq>,
}

impl Hello {
    /// The local Hello for this build.
    pub fn local() -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            capabilities: Capabilities {
                streaming: true,
                cas: false,
                detach: false,
            },
            last_event_seq: None,
        }
    }
}

/// The result of a successful handshake: the peer's advertised capabilities,
/// so the local side can branch on what the peer supports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Negotiated {
    pub peer_capabilities: Capabilities,
}

/// Validate the peer's Hello against the local one. Fails on version mismatch
/// (not retriable — the peer must upgrade) with a ProtocolVersion category.
/// The caller applies its own required-capability checks on top of the
/// negotiated peer capabilities.
pub fn negotiate(local: &Hello, peer: &Hello) -> Result<Negotiated, ProtocolError> {
    if peer.protocol_version != local.protocol_version {
        return Err(ProtocolError::new(
            ErrorCategory::ProtocolVersion,
            format!(
                "version mismatch: local {} peer {}",
                local.protocol_version, peer.protocol_version,
            ),
            false,
        ));
    }
    Ok(Negotiated {
        peer_capabilities: peer.capabilities.clone(),
    })
}

#[cfg(test)]
mod tests;
