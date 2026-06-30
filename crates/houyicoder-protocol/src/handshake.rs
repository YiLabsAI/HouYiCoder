//! Hello handshake — the first frame both ends exchange on connect. It
//! carries the protocol version and capability flags; a version mismatch or
//! an unsupported required capability fails the handshake explicitly so a
//! peer never enters a half-working session.

use crate::wire::{WireError, WireErrorKind};
use serde::{Deserialize, Serialize};

/// The wire protocol version. Bumped only on a breaking change to the
/// message set or framing; a peer that sees a different version fails the
/// handshake rather than guessing.
pub const PROTOCOL_VERSION: u16 = 1;

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

/// The Hello frame: protocol version plus advertised capabilities. Both ends
/// send this as their first frame; the handshake succeeds when versions match
/// and each side's required capabilities are met by the other. A client
/// additionally declares how many trajectory events it has already rendered
/// (last_event_count) so a fresh or reconnecting client gets only the events it
/// missed replayed, not the whole transcript again; None means a fresh client
/// that wants the full transcript from the start.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub protocol_version: u16,
    pub capabilities: Capabilities,
    /// The count of trajectory events the client has already rendered. The
    /// server uses it as the replay start index (skip this many events from
    /// the current trajectory snapshot) because the trajectory mirror is
    /// append-only with no per-event seq the server could map a client seq to.
    /// A reconnecting client gets only what it missed; None means a fresh
    /// client that wants the whole transcript. Default None keeps the field
    /// backward-compatible with peers that predate it.
    #[serde(default)]
    pub last_event_count: Option<u64>,
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
            last_event_count: None,
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
/// (not retriable — the peer must upgrade) with a ProtocolVersion kind. The
/// caller applies its own required-capability checks on top of the negotiated
/// peer capabilities.
pub fn negotiate(local: &Hello, peer: &Hello) -> Result<Negotiated, WireError> {
    if peer.protocol_version != local.protocol_version {
        return Err(WireError::new(
            WireErrorKind::ProtocolVersion,
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
mod tests {
    use super::*;

    #[test]
    fn test_same_version_negotiates() {
        let local = Hello::local();
        let peer = Hello::local();
        let n = negotiate(&local, &peer).expect("same version ok");
        assert_eq!(n.peer_capabilities, local.capabilities);
    }

    #[test]
    fn test_version_mismatch_not_retriable() {
        let local = Hello::local();
        let peer = Hello {
            protocol_version: PROTOCOL_VERSION + 1,
            capabilities: Capabilities::default(),
            last_event_count: None,
        };
        let err = negotiate(&local, &peer).expect_err("mismatch fails");
        assert_eq!(err.kind, WireErrorKind::ProtocolVersion);
        assert!(!err.retriable, "version mismatch is not retriable");
    }

    #[test]
    fn test_hello_round_trips() {
        let h = Hello::local();
        let json = serde_json::to_string(&h).expect("serialize");
        let back: Hello = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, h);
    }
}
