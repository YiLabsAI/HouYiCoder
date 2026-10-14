//! Tests for this module.

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
        last_event_seq: None,
    };
    let err = negotiate(&local, &peer).expect_err("mismatch fails");
    assert_eq!(err.category, ErrorCategory::ProtocolVersion);
    assert!(!err.retriable, "version mismatch is not retriable");
}

#[test]
fn test_hello_uses_protocol_version() {
    assert_eq!(Hello::local().protocol_version, PROTOCOL_VERSION);
}

#[test]
fn test_previous_version_is_rejected() {
    let local = Hello::local();
    let peer = Hello {
        protocol_version: 3,
        capabilities: Capabilities::default(),
        last_event_seq: None,
    };
    let err = negotiate(&local, &peer).expect_err("superseded version fails");
    assert_eq!(err.category, ErrorCategory::ProtocolVersion);
    assert!(
        !err.retriable,
        "the peer must upgrade, retrying cannot help"
    );
}

#[test]
fn test_hello_round_trips() {
    let h = Hello::local();
    let json = serde_json::to_string(&h).expect("serialize");
    let back: Hello = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, h);
}

#[test]
fn test_hello_serializes_exactly() {
    let json = serde_json::to_string(&Hello::local()).expect("serialize");
    assert_eq!(
        json,
        r#"{"protocol_version":7,"capabilities":{"streaming":true,"cas":false,"detach":false},"last_event_seq":null}"#
    );
}

/// A v4 peer (the pre-ModelCatalog-shape build) is refused at the
/// handshake, not later at payload decode: the v4 model payloads carry
/// different field names, so a passthrough would half-work.
#[test]
fn test_v4_peer_refused() {
    let v4 = Hello {
        protocol_version: 4,
        capabilities: Capabilities::default(),
        last_event_seq: None,
    };
    let err = negotiate(&Hello::local(), &v4).expect_err("v4 must fail");
    assert_eq!(err.category, ErrorCategory::ProtocolVersion);
}

/// A peer on the pre-split version is refused: it has no operation
/// catch-all, so a mixed pair would drop every memory-change frame at
/// decode rather than failing at the handshake.
#[test]
fn test_v6_peer_refused() {
    let v6 = Hello {
        protocol_version: 6,
        capabilities: Capabilities::default(),
        last_event_seq: None,
    };
    let err = negotiate(&Hello::local(), &v6).expect_err("v6 must fail");
    assert_eq!(err.category, ErrorCategory::ProtocolVersion);
}
