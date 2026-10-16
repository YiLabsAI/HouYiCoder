//! Tests for this module.

use super::*;

#[test]
fn test_breakpoint_round_trips() {
    let bp = CacheBreakpoint {
        kind: BreakpointKind::SystemStaticPrefix,
        hint: CacheHint::Ephemeral(CacheTtl::OneHour),
    };
    let json = serde_json::to_string(&bp).expect("serialize");
    assert!(
        json.contains("systemStaticPrefix"),
        "camelCase kind: {json}"
    );
    assert!(json.contains("ephemeral"), "kind tag: {json}");
    assert!(json.contains("oneHour"), "ttl: {json}");
    let back: CacheBreakpoint = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, bp);
}
