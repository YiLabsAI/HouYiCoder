//! Tests for this module.

use super::*;

fn fixture() -> ContextBreakdown {
    let categories = vec![
        CategoryBreakdown {
            label: "system".into(),
            color_hint: 0,
            tokens: 1200,
            is_deferred: false,
            is_reserved: false,
        },
        CategoryBreakdown {
            label: "user".into(),
            color_hint: 1,
            tokens: 8400,
            is_deferred: false,
            is_reserved: false,
        },
        CategoryBreakdown {
            label: "tools".into(),
            color_hint: 2,
            tokens: 2400,
            is_deferred: false,
            is_reserved: true,
        },
        CategoryBreakdown {
            label: "free space".into(),
            color_hint: 3,
            tokens: 0,
            is_deferred: false,
            is_reserved: false,
        },
    ];
    let grid = vec![
        vec![
            GridCell {
                category_idx: 0,
                fullness: 1.0,
            },
            GridCell {
                category_idx: 1,
                fullness: 1.0,
            },
            GridCell {
                category_idx: 1,
                fullness: 0.4,
            },
        ],
        vec![
            GridCell {
                category_idx: 2,
                fullness: 1.0,
            },
            GridCell {
                category_idx: 3,
                fullness: 1.0,
            },
            GridCell {
                category_idx: 3,
                fullness: 1.0,
            },
        ],
    ];
    ContextBreakdown {
        model: "test-model".into(),
        total_tokens: 12000,
        context_window: 200000,
        categories,
        grid,
        cache_breakpoint: Some(2),
        compact_summary: None,
        cache_prefix_tokens: None,
        cache_hit_rate: None,
    }
}

#[test]
fn test_context_breakdown_round_trips() {
    let original = fixture();
    let json = serde_json::to_string(&original).expect("serialize");
    let back: ContextBreakdown = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, original);
}

#[test]
fn test_breakdown_uses_camel_case() {
    let json = serde_json::to_string(&fixture()).expect("serialize");
    assert!(
        json.contains("\"totalTokens\""),
        "camelCase key expected: {json}"
    );
    assert!(
        json.contains("\"contextWindow\""),
        "camelCase key expected: {json}"
    );
    assert!(
        json.contains("\"colorHint\""),
        "camelCase key expected: {json}"
    );
    assert!(
        json.contains("\"categoryIdx\""),
        "camelCase key expected: {json}"
    );
    assert!(
        json.contains("\"cacheBreakpoint\""),
        "camelCase key expected: {json}"
    );
    // snake_case must not leak.
    assert!(!json.contains("total_tokens"), "snake_case leaked: {json}");
}

#[test]
fn test_cache_breakpoint_round_trips() {
    let mut bd = fixture();
    bd.cache_breakpoint = None;
    let json = serde_json::to_string(&bd).expect("serialize");
    let back: ContextBreakdown = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, bd);
    assert!(json.contains("null"));
}
