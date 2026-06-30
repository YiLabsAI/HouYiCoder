//! The wire form of a context-window breakdown, returned to the frontend
//! over the wire so the TUI renders /context without importing the engine
//! or context crate. The engine breakdown carries a flat grid of filled
//! cells for the proportional-area visualization; the wire form owns the
//! same nested vec so it crosses any carrier with no lifetime leakage.
//! cache_breakpoint is a flat cell index (None until the cache tracker
//! lands); cells before it are cached prefix.

use serde::{Deserialize, Serialize};

/// One category footprint in the context view, wire form. color_hint is a
/// palette index the renderer maps to a cell shade; is_deferred marks
/// categories the loop pushes out of the active window, is_reserved marks
/// categories the runtime carves out of the token budget.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CategoryBreakdown {
    /// The display label (system / user / tools / free space / ...).
    pub label: String,
    /// Palette index the renderer maps to a shade for this category.
    pub color_hint: u8,
    /// Tokens this category occupies in the current window.
    pub tokens: u32,
    /// True when the category is deferred out of the active window.
    pub is_deferred: bool,
    /// True when the category is reserved against the token budget.
    pub is_reserved: bool,
}

/// One grid cell in the proportional-area view, wire form. category_idx
/// picks the row in categories that fills this cell; fullness is the share
/// of that cell the category covers (a boundary cell is partial).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GridSquare {
    /// Index into categories for the category that fills this cell.
    pub category_idx: usize,
    /// Share of this cell the category covers (1.0 for a whole cell, the
    /// fractional part for a boundary cell).
    pub fullness: f32,
}

/// The data the context visualization renders, wire form. The grid is a
/// 2-D vec of cells whose proportions reflect each category's token share
/// of the window; cache_breakpoint is a flat cell index separating cached
/// prefix from the rest (None until the cache tracker lands).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextBreakdown {
    /// The model id the runner drives.
    pub model: String,
    /// Tokens used across every category in the current window.
    pub total_tokens: u32,
    /// The provider-reported context window for the model.
    pub context_window: u32,
    /// Per-category token footprints.
    pub categories: Vec<CategoryBreakdown>,
    /// The proportional-area grid the renderer draws.
    pub grid: Vec<Vec<GridSquare>>,
    /// Flat cell index of the cache breakpoint (cached prefix before it).
    pub cache_breakpoint: Option<usize>,
    /// When a compaction has run, a human-readable summary of what was folded
    /// (e.g. "2 compacts · 8 turns → 3.2k summary · recoverable via /search").
    /// None before the first compaction. Surfaces the lossless/recoverable
    /// nature of houyi's view-over-raw compaction in the /context view itself.
    #[serde(default)]
    pub compact_summary: Option<String>,
    /// Cache prefix token count (the byte-stable system prompt + tools that
    /// repeat across turns). Populated from the System prompt + Tools section
    /// token counts. None before the first build.
    #[serde(default)]
    pub cache_prefix_tokens: Option<u32>,
    /// Cache hit rate (cache_read / input_tokens, 0.0-1.0). None before the
    /// first provider response.
    #[serde(default)]
    pub cache_hit_rate: Option<f64>,
}

/// Build the proportional-area grid the renderer draws: each category's cell
/// count reflects its token share of the window, with reserved categories
/// allocated first and free space filling the rest. Pure-data over the wire
/// types, so the frontend can render the viz without an engine dependency.
pub fn build_grid(
    categories: &[CategoryBreakdown],
    context_window: u32,
    terminal_cols: u16,
) -> Vec<Vec<GridSquare>> {
    if context_window == 0 || categories.is_empty() {
        return Vec::new();
    }
    let narrow = terminal_cols < 80;
    let (w, h) = if context_window >= 1_000_000 {
        (if narrow { 5 } else { 20 }, 10)
    } else {
        (if narrow { 5 } else { 10 }, if narrow { 5 } else { 10 })
    };
    let total = (w as usize) * (h as usize);
    let reserved_count: usize = categories
        .iter()
        .filter(|c| c.is_reserved)
        .map(|c| alloc_squares(c, context_window, total))
        .sum();
    let free_target = total.saturating_sub(reserved_count);
    let free_idx = categories.iter().position(|c| c.label == "Free space");
    let mut cells: Vec<GridSquare> = Vec::with_capacity(total);
    for (idx, c) in categories.iter().enumerate() {
        if c.is_reserved || c.is_deferred || c.label == "Free space" {
            continue;
        }
        push_cells(&mut cells, idx, c, total, context_window);
        if cells.len() >= free_target {
            break;
        }
    }
    while cells.len() < free_target {
        cells.push(GridSquare {
            category_idx: free_idx.unwrap_or(usize::MAX),
            fullness: 1.0,
        });
    }
    for (idx, c) in categories.iter().enumerate() {
        if !c.is_reserved {
            continue;
        }
        push_cells(&mut cells, idx, c, total, context_window);
        if cells.len() >= total {
            break;
        }
    }
    cells.truncate(total);
    cells
        .chunks(w as usize)
        .map(|chunk| chunk.to_vec())
        .collect()
}

/// Cells a non-free, non-reserved category gets: max(1, round(exact)).
fn alloc_squares(c: &CategoryBreakdown, context_window: u32, total: usize) -> usize {
    if c.tokens == 0 || c.label == "Free space" || c.is_reserved {
        return 0;
    }
    let exact = c.tokens as f64 / context_window as f64 * total as f64;
    (exact.round() as usize).max(1)
}

/// Push a category's cells: whole full cells plus one partial boundary cell
/// whose fullness is the fractional part (round-down drops the fraction).
fn push_cells(
    cells: &mut Vec<GridSquare>,
    idx: usize,
    c: &CategoryBreakdown,
    total: usize,
    context_window: u32,
) {
    if c.tokens == 0 {
        return;
    }
    let exact = c.tokens as f64 / context_window as f64 * total as f64;
    let whole = exact.floor() as usize;
    let frac = (exact - exact.floor()) as f32;
    let squares = alloc_squares(c, context_window, total);
    for i in 0..squares {
        let fullness = if i == whole && frac > 0.0 { frac } else { 1.0 };
        cells.push(GridSquare {
            category_idx: idx,
            fullness,
        });
    }
}

/// A canned ContextBreakdown for the no-runner / preview path so the /context
/// viz renders a real-shape grid before the live breakdown is wired. The
/// numbers mirror a typical session footprint; the grid is built via
/// build_grid.
pub fn stub_breakdown() -> ContextBreakdown {
    let window: u32 = 200_000;
    let cats: Vec<CategoryBreakdown> = vec![
        CategoryBreakdown {
            label: "System prompt".into(),
            color_hint: 244,
            tokens: 1_800,
            is_deferred: false,
            is_reserved: false,
        },
        CategoryBreakdown {
            label: "System tools".into(),
            color_hint: 244,
            tokens: 19_000,
            is_deferred: false,
            is_reserved: false,
        },
        CategoryBreakdown {
            label: "Memory files".into(),
            color_hint: 203,
            tokens: 2_500,
            is_deferred: false,
            is_reserved: false,
        },
        CategoryBreakdown {
            label: "Skills".into(),
            color_hint: 221,
            tokens: 1_800,
            is_deferred: false,
            is_reserved: false,
        },
        CategoryBreakdown {
            label: "Messages".into(),
            color_hint: 61,
            tokens: 120_000,
            is_deferred: false,
            is_reserved: false,
        },
        CategoryBreakdown {
            label: "Free space".into(),
            color_hint: 245,
            tokens: window - 145_100,
            is_deferred: false,
            is_reserved: false,
        },
    ];
    let total: u32 = cats.iter().map(|c| c.tokens).sum();
    let grid = build_grid(&cats, window, 100);
    ContextBreakdown {
        model: "glm-5.2".into(),
        total_tokens: total,
        context_window: window,
        categories: cats,
        grid,
        cache_breakpoint: None,
        compact_summary: None,
        cache_prefix_tokens: None,
        cache_hit_rate: None,
    }
}
#[cfg(test)]
mod tests {
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
                GridSquare {
                    category_idx: 0,
                    fullness: 1.0,
                },
                GridSquare {
                    category_idx: 1,
                    fullness: 1.0,
                },
                GridSquare {
                    category_idx: 1,
                    fullness: 0.4,
                },
            ],
            vec![
                GridSquare {
                    category_idx: 2,
                    fullness: 1.0,
                },
                GridSquare {
                    category_idx: 3,
                    fullness: 1.0,
                },
                GridSquare {
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
}
