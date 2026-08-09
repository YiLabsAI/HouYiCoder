//! Tests for context_view, split from context_view.rs on the file-size gate.

use super::*;
use crate::composition;
use crate::state::Screen;
use crate::test_support::render_buffer;
use houyicoder_protocol::frontend::SlashCommand;

fn working_app() -> crate::state::App {
    let mut app = composition::app();
    app.screen = Screen::Working;
    app.run_command(SlashCommand::Context);
    app
}

fn dump(buf: &ratatui::buffer::Buffer) -> String {
    let area = buf.area();
    let mut rows: Vec<String> = Vec::with_capacity(area.height as usize);
    for y in 0..area.height {
        let mut row = String::with_capacity(area.width as usize);
        for x in 0..area.width {
            row.push_str(buf.cell((x, y)).expect("cell").symbol());
        }
        rows.push(row.trim_end().to_string());
    }
    rows.join("\n")
}

#[test]
fn test_grid_inline_renders_free() {
    let app = working_app();
    let buf = render_buffer(&app, 100, 40);
    let text = dump(&buf);
    assert!(
        text.contains('\u{26C1}') || text.contains('\u{26C0}'),
        "grid ball glyph missing from inline block"
    );
    assert!(text.contains('\u{26F6}'), "free-space glyph missing");
}

#[test]
fn test_grid_inline_renders_legend() {
    let app = working_app();
    let buf = render_buffer(&app, 100, 40);
    let text = dump(&buf);
    assert!(
        text.contains("Estimated usage by category"),
        "legend header missing"
    );
}

/// Fixture: one category + one grid cell (so the empty-grid guard does not
/// short-circuit before legend_rows runs) + all optional fields None. Each
/// test overrides the field(s) it exercises.
fn base_breakdown() -> houyicoder_protocol::frontend::context::ContextBreakdown {
    use houyicoder_protocol::frontend::context::{CategoryBreakdown, ContextBreakdown, GridSquare};
    ContextBreakdown {
        model: "test".into(),
        total_tokens: 100_000,
        context_window: 200_000,
        categories: vec![CategoryBreakdown {
            label: "System prompt".into(),
            color_hint: 244,
            tokens: 21_900,
            is_deferred: false,
            is_reserved: false,
        }],
        // One cell so the empty-grid guard (bd.grid.is_empty() → "no data
        // yet") does not short-circuit before legend_rows runs.
        grid: vec![vec![GridSquare {
            category_idx: 0,
            fullness: 1.0,
        }]],
        cache_breakpoint: None,
        compact_summary: None,
        cache_prefix_tokens: None,
        cache_hit_rate: None,
    }
}

fn rows_text(bd: houyicoder_protocol::frontend::context::ContextBreakdown) -> String {
    let view = ContextView {
        breakdown: bd,
        drill: crate::records::ContextDrillDown::default(),
        suggestions: vec![],
    };
    render_as_rows(&view)
        .iter()
        .map(|(p, _)| p.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Cache prefix must render even when hit rate is absent (no prior provider
/// turn). This is the stub/zero-turn PTY case — pairing the two would hide
/// the prefix line whenever rate is None. Pure fn(ContextBreakdown)->rows,
/// default-gate so a future legend_rows change that drops the field goes red.
#[test]
fn test_grid_renders_cache_prefix() {
    let mut bd = base_breakdown();
    bd.cache_prefix_tokens = Some(21_900);
    bd.cache_hit_rate = None;
    let text = rows_text(bd);
    assert!(
        text.contains("Cache prefix: 21.9K"),
        "prefix renders without rate (decoupled):\n{text}"
    );
    assert!(
        !text.contains("hit rate"),
        "no rate suffix when hit_rate is None:\n{text}"
    );
}

#[test]
fn test_grid_renders_rate_suffix() {
    let mut bd = base_breakdown();
    bd.cache_prefix_tokens = Some(21_900);
    bd.cache_hit_rate = Some(0.75);
    let text = rows_text(bd);
    assert!(
        text.contains("Cache prefix: 21.9K") && text.contains("hit rate 75%"),
        "prefix + rate suffix render together:\n{text}"
    );
}

#[test]
fn test_grid_renders_folded_summary() {
    let mut bd = base_breakdown();
    bd.compact_summary = Some("3 compacts · 5 turns folded · \"preview\"".into());
    let text = rows_text(bd);
    assert!(
        text.contains("folded:") && text.contains("3 compacts"),
        "folded summary row renders:\n{text}"
    );
}

/// Negative case: when cache_prefix_tokens + compact_summary are both None
/// (the base fixture, = a pre-compact session with no prior turn), neither
/// row renders. Guards against a regression that draws the rows
/// unconditionally (e.g. "Cache prefix: 0") — the positive tests alone
/// would stay green under that regression.
#[test]
fn test_grid_omits_rows_none() {
    let text = rows_text(base_breakdown());
    assert!(
        !text.contains("Cache prefix"),
        "prefix row should be absent when cache_prefix_tokens is None:\n{text}"
    );
    assert!(
        !text.contains("folded:"),
        "folded row should be absent when compact_summary is None:\n{text}"
    );
}

/// The combination cell: prefix Some + rate None + compact_summary Some
/// (a post-compact session with zero new turns — /context run after a
/// compact but before the next provider call so no cache hit yet). All
/// three rows must render independently: prefix (no rate suffix) + folded.
/// This is the state-derivation uncovered cell: the three are tested
/// individually above, but the simultaneous combination (prefix has value +
/// rate None + manifest exists) was not — the kind of gap any→same hides.
#[test]
fn test_grid_renders_prefix_folded() {
    let mut bd = base_breakdown();
    bd.cache_prefix_tokens = Some(21_900);
    bd.cache_hit_rate = None;
    bd.compact_summary = Some("3 compacts · 5 turns folded · \"preview\"".into());
    let text = rows_text(bd);
    assert!(
        text.contains("Cache prefix: 21.9K"),
        "prefix renders with folded present + rate absent:\n{text}"
    );
    assert!(
        !text.contains("hit rate"),
        "no rate suffix when hit_rate is None even with folded present:\n{text}"
    );
    assert!(
        text.contains("folded:") && text.contains("3 compacts"),
        "folded renders with prefix present:\n{text}"
    );
}

/// The model line: "test · 100.0K/200.0K tokens (50%)". The most-seen
/// legend row but never asserted — guards a regression that drops it or
/// misformats the pct.
#[test]
fn test_legend_renders_model_line() {
    let text = rows_text(base_breakdown());
    assert!(
        text.contains("test ") && text.contains("tokens (50%)"),
        "model line missing/misformatted (want model + 50% pct):\n{text}"
    );
}

/// A non-free category row: the base fixture has "System prompt" but no
/// test asserted its render — only Free space (at the PTY layer) covered
/// the category-row mechanism. This pins the non-free label + token line.
#[test]
fn test_legend_renders_category_row() {
    let text = rows_text(base_breakdown());
    assert!(
        text.contains("System prompt:"),
        "non-free category label missing:\n{text}"
    );
    assert!(
        text.contains("21.9K tokens"),
        "category token line missing:\n{text}"
    );
}

/// A reserved category row + the reserved grid glyph. Zero coverage today
/// — the reserved branch (grid_row_spans + legend reserved section) was
/// never asserted.
#[test]
fn test_legend_renders_reserved_row() {
    use houyicoder_protocol::frontend::context::{CategoryBreakdown, ContextBreakdown, GridSquare};
    let bd = ContextBreakdown {
        model: "test".into(),
        total_tokens: 100_000,
        context_window: 200_000,
        categories: vec![CategoryBreakdown {
            label: "Output cap".into(),
            color_hint: 196,
            tokens: 8_000,
            is_deferred: false,
            is_reserved: true,
        }],
        grid: vec![vec![GridSquare {
            category_idx: 0,
            fullness: 1.0,
        }]],
        cache_breakpoint: None,
        compact_summary: None,
        cache_prefix_tokens: None,
        cache_hit_rate: None,
    };
    let text = rows_text(bd);
    assert!(
        text.contains("Output cap:") && text.contains("8.0K tokens"),
        "reserved category row missing:\n{text}"
    );
    assert!(
        text.contains('\u{26DD}'),
        "reserved glyph missing from grid:\n{text}"
    );
}

/// Grid glyph selection by fullness: ≥0.7 → filled ◱, >0 → hollow ◐,
/// 0.0 → empty ▢. The 0.6/0.7 boundary (FULL_GLYPH_THRESHOLD) is where a
/// regression mis-picks. Tests asserted glyph presence before, not which
/// glyph for which fullness.
#[test]
fn test_grid_glyph_picks_fullness() {
    use houyicoder_protocol::frontend::context::GridSquare;
    fn bd_fullness(fullness: f32) -> houyicoder_protocol::frontend::context::ContextBreakdown {
        let mut bd = base_breakdown();
        bd.grid = vec![vec![GridSquare {
            category_idx: 0,
            fullness,
        }]];
        bd
    }
    // fullness 1.0 → filled ball ◱
    assert!(
        rows_text(bd_fullness(1.0)).contains('\u{26C1}'),
        "fullness 1.0 should pick filled ball"
    );
    // boundary 0.7 → filled ball ◱ (≥ threshold)
    assert!(
        rows_text(bd_fullness(0.7)).contains('\u{26C1}'),
        "fullness 0.7 boundary should pick filled ball"
    );
    // fullness 0.6 → hollow ball ◐
    assert!(
        rows_text(bd_fullness(0.6)).contains('\u{26C0}'),
        "fullness 0.6 should pick hollow ball"
    );
    // fullness 0.0 → empty ▢
    assert!(
        rows_text(bd_fullness(0.0)).contains('\u{26F6}'),
        "fullness 0.0 should pick empty glyph"
    );
}

/// The cache breakpoint draws a vertical bar at the cell where the cached
/// prefix ends, so the user can see how much of the grid is the stable
/// cached prefix vs the per-turn fresh suffix. Pins the marker renders at
/// the right flat cell index across a multi-cell grid.
#[test]
fn test_grid_renders_breakpoint_marker() {
    use houyicoder_protocol::frontend::context::GridSquare;
    let mut bd = base_breakdown();
    // A 5-cell single-row grid; breakpoint at cell 2 (cells 0,1 cached,
    // cell 2 onward fresh). base_breakdown's one category is idx 0.
    bd.grid = vec![vec![
        GridSquare {
            category_idx: 0,
            fullness: 1.0
        };
        5
    ]];
    bd.cache_breakpoint = Some(2);
    let text = rows_text(bd);
    assert!(
        text.contains('\u{2502}'),
        "cache breakpoint bar should render at cell 2:\n{text}"
    );
}

/// A realistic near-capacity breakdown so suggestions_for fires on
/// real-ish data, not just the canned stub (Messages=120k) that
/// grid_inline_renders_suggestions_section uses. pct=85 (>= 80), Messages
/// 20k (>= 15%), System tools 6k (>= 5%) -- all three triggers met.
fn full_context_breakdown() -> houyicoder_protocol::frontend::context::ContextBreakdown {
    use houyicoder_protocol::frontend::context::{CategoryBreakdown, ContextBreakdown, GridSquare};
    ContextBreakdown {
        model: "test".into(),
        total_tokens: 85_000,
        context_window: 100_000,
        categories: vec![
            CategoryBreakdown {
                label: "System prompt".into(),
                color_hint: 244,
                tokens: 2_000,
                is_deferred: false,
                is_reserved: false,
            },
            CategoryBreakdown {
                label: "System tools".into(),
                color_hint: 245,
                tokens: 6_000,
                is_deferred: false,
                is_reserved: false,
            },
            CategoryBreakdown {
                label: "Messages".into(),
                color_hint: 246,
                tokens: 20_000,
                is_deferred: false,
                is_reserved: false,
            },
            CategoryBreakdown {
                label: "Free space".into(),
                color_hint: 247,
                tokens: 15_000,
                is_deferred: false,
                is_reserved: false,
            },
        ],
        grid: vec![vec![GridSquare {
            category_idx: 0,
            fullness: 1.0,
        }]],
        cache_breakpoint: None,
        compact_summary: None,
        cache_prefix_tokens: None,
        cache_hit_rate: None,
    }
}

/// Suggestions fire + render on real-ish data (not just the canned stub).
/// Pins the data -> suggestions_for -> render path on a near-capacity
/// breakdown, the gap left by the zero-turn PTY journey (its prospective
/// breakdown never crosses the suggestion thresholds).
#[test]
fn test_real_data_renders_suggestions() {
    let bd = full_context_breakdown();
    let suggestions = crate::composition::suggestions_for(&bd);
    assert!(
        !suggestions.is_empty(),
        "suggestions fire on a near-capacity breakdown"
    );
    assert!(
        suggestions.iter().any(|s| s.title.contains("Context is")),
        "pct-full suggestion fires: {:?}",
        suggestions.iter().map(|s| &s.title).collect::<Vec<_>>()
    );
    // The category suggestions must match the category they describe, not
    // a per-tool label borrowed from another agent (Messages is the
    // conversation, System tools is the tool schemas).
    assert!(
        suggestions
            .iter()
            .any(|s| s.title.contains("Conversation using")),
        "Messages suggestion is labeled Conversation: {:?}",
        suggestions.iter().map(|s| &s.title).collect::<Vec<_>>()
    );
    assert!(
        suggestions
            .iter()
            .any(|s| s.title.contains("Tool schemas using")),
        "System-tools suggestion is labeled Tool schemas: {:?}",
        suggestions.iter().map(|s| &s.title).collect::<Vec<_>>()
    );
    // The suggestion titles land in the rendered rows (suggestions_rows).
    let view = ContextView {
        breakdown: bd,
        drill: crate::records::ContextDrillDown::default(),
        suggestions,
    };
    let text: String = render_as_rows(&view)
        .iter()
        .map(|(p, _)| p.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        text.contains("Suggestions"),
        "suggestions section header renders:\n{text}"
    );
    assert!(
        text.contains("Context is"),
        "pct-full suggestion title renders in the rows:\n{text}"
    );
}

#[test]
fn test_grid_inline_renders_usage() {
    let app = working_app();
    let buf = render_buffer(&app, 100, 40);
    let text = dump(&buf);
    assert!(text.contains("Context Usage"), "bold header missing");
}

#[test]
fn test_grid_inline_renders_memory() {
    let app = working_app();
    let buf = render_buffer(&app, 100, 40);
    let text = dump(&buf);
    assert!(text.contains("Memory files"), "memory files header missing");
    assert!(text.contains("MEMORY.md"), "memory file row missing");
}

#[test]
fn test_grid_inline_renders_skills() {
    let app = working_app();
    let buf = render_buffer(&app, 100, 40);
    let text = dump(&buf);
    assert!(text.contains("Skills"), "skills header missing");
    assert!(
        text.contains("Built-in"),
        "skills source group label missing"
    );
}

#[test]
fn test_grid_inline_renders_suggestions() {
    let app = working_app();
    let buf = render_buffer(&app, 100, 50);
    let text = dump(&buf);
    assert!(text.contains("Suggestions"), "suggestions header missing");
    assert!(
        text.contains('\u{26A0}') || text.contains('\u{2139}'),
        "suggestion glyph missing"
    );
}

#[test]
fn test_grid_variant_pushed_inline() {
    let app = working_app();
    let has_grid = app
        .transcript
        .iter()
        .any(|l| matches!(l, crate::records::TranscriptLine::ContextGrid(_)));
    assert!(has_grid, "transcript missing ContextGrid line");
}

#[test]
fn test_context_block_persists_line() {
    // The grid block must stay visible when new content arrives after it.
    // Push ContextGrid, then a System line, render, and assert both the
    // grid text and the system line are present in the same render.
    let mut app = composition::app();
    app.screen = Screen::Working;
    app.run_command(SlashCommand::Context);
    app.system_line("done reading files");
    let buf = render_buffer(&app, 100, 50);
    let text = dump(&buf);
    assert!(
        text.contains("Context Usage"),
        "grid header missing after subsequent line"
    );
    assert!(
        text.contains("Estimated usage by category"),
        "legend text missing after subsequent line"
    );
    assert!(
        text.contains("done reading files"),
        "subsequent system line missing from render"
    );
}

#[test]
fn test_block_renders_continuation_glyph() {
    let app = working_app();
    let buf = render_buffer(&app, 100, 40);
    let text = dump(&buf);
    assert!(
        text.contains('\u{23BF}'),
        "continuation glyph missing from header"
    );
}

#[test]
fn test_legend_tight_no_gap() {
    // The grid column must be fixed-width (cells * 2), not spread across
    // the whole row which would leave a gap before the legend. With 10
    // cells per row the grid column is 20 display columns; the legend
    // starts right after the 2-col gap, so the legend header text should
    // appear within the first ~28 display columns.
    let app = working_app();
    let buf = render_buffer(&app, 100, 40);
    let text = dump(&buf);
    let legend_row = text
        .lines()
        .find(|l| l.contains("Estimated usage by category"))
        .expect("legend header row");
    let col = legend_row
        .chars()
        .position(|ch| ch == 'E')
        .expect("Estimated text in legend row");
    // Grid is 20 display cols + 2 gap + 5 indent = 27; legend starts by
    // column 28.
    assert!(
        col < 30,
        "legend starts at col {col}, expected < 30 (tight layout); row: {legend_row}"
    );
}

#[test]
fn test_fmt_tokens_compact_shape() {
    assert_eq!(fmt_tokens(999), "999");
    assert_eq!(fmt_tokens(1_800), "1.8K");
    assert_eq!(fmt_tokens(120_000), "120.0K");
    assert_eq!(fmt_tokens(2_000_000), "2.0M");
}

#[test]
fn test_grid_rows_selectable_plain() {
    // The grid renders as plain transcript rows tagged selectable (not
    // widget placeholders): the row set the selection path reads carries
    // the grid glyphs and legend text as real content.
    let app = working_app();
    let _buf = render_buffer(&app, 100, 50);
    let rows = app.last_all_rows.borrow();
    let joined: String = rows
        .iter()
        .map(|(_, s)| s.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        rows.iter()
            .all(|(t, _)| !crate::selection::is_non_selectable(*t)),
        "no /context row may be a non-selectable widget row"
    );
    assert!(
        joined.contains('\u{26C1}') || joined.contains('\u{26C0}'),
        "grid glyph missing from the selectable row text"
    );
    assert!(
        joined.contains("Estimated usage by category"),
        "legend text missing from the selectable row text"
    );
}

#[test]
fn test_empty_grid_renders_row() {
    let view = ContextView::default();
    let rows = render_as_rows(&view);
    assert_eq!(rows.len(), 1, "empty grid yields a single no-data row");
    assert!(rows[0].0.contains("no data yet"));
}
