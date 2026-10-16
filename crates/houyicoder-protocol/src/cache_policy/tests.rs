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

#[test]
fn test_cache_format_defaults() {
    // An endpoint that declares nothing reads as the automatic-prefix form,
    // and a declaration round-trips in the settings spelling.
    assert_eq!(PromptCacheFormat::default(), PromptCacheFormat::AutoPrefix);
    assert_eq!(
        serde_json::to_string(&PromptCacheFormat::PositionalMarkers).expect("serialize"),
        "\"positionalMarkers\""
    );
    let declared: PromptCacheFormat =
        serde_json::from_str("\"positionalMarkers\"").expect("deserialize");
    assert_eq!(declared, PromptCacheFormat::PositionalMarkers);
}
