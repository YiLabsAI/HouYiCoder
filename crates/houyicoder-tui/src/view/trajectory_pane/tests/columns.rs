//! Tests for the turn list's columns: what each cell states and where it sits
//! across the rows.

use super::super::list;
use super::super::*;
use super::fixtures::{turn, view};
use ratatui::layout::Rect;
use ratatui::text::Line;

/// The index of the first cell matching probe, and the display column it starts
/// at. A cell is found by its content rather than by its position, so a probe
/// that stops matching fails the test instead of passing vacuously.
fn cell_of(line: &Line<'static>, probe: impl Fn(&str) -> bool) -> (usize, usize) {
    let mut col = 0usize;
    for (i, span) in line.spans.iter().enumerate() {
        if probe(span.content.as_ref()) {
            return (i, col);
        }
        col += UnicodeWidthStr::width(span.content.as_ref());
    }
    panic!("no matching cell in the row: {:?}", line.spans);
}

/// The display column where the first cell matching probe starts.
fn column_of(line: &Line<'static>, probe: impl Fn(&str) -> bool) -> usize {
    cell_of(line, probe).1
}

/// The token column starts at the same display column whatever the title holds,
/// so a wide-glyph title cannot shift the numbers on the rows below it.
#[test]
fn test_columns_hold_across_titles() {
    let turn = |n: usize, title: &str| TrajectoryTurn {
        tokens_in: Some(1_200),
        tokens_out: Some(500),
        duration_ms: 100,
        ..turn(n, title)
    };
    let view = TrajectoryView {
        models_used: 2,
        tokens_in: Some(2_400),
        tokens_out: Some(1_000),
        ..view(vec![
            TrajectoryRow::Turn(turn(1, "宽的标题")),
            TrajectoryRow::Turn(turn(2, "ascii")),
        ])
    };
    let (_, body, _, _) = list::draw_turn_list(&view, 0, Rect::new(0, 0, 200, 10));
    // The token column is found by the cell that carries the in/out counts,
    // not by a glyph that may be reworded: a probe that no longer matches
    // would silently make this assertion vacuous.
    assert_eq!(
        column_of(&body[0], |c| c.contains(" in ")),
        column_of(&body[1], |c| c.contains(" in ")),
        "the token column sits at one display column on both rows"
    );
}

/// A title that fills its column still leaves a gap before the token cell, so
/// the ellipsis cannot glue onto the numbers.
#[test]
fn test_title_keeps_gap() {
    let view = TrajectoryView {
        tokens_in: Some(1_200),
        tokens_out: Some(45),
        ..view(vec![TrajectoryRow::Turn(TrajectoryTurn {
            title: "a title that fills the whole title column".into(),
            tokens_in: Some(1_200),
            tokens_out: Some(45),
            duration_ms: 100,
            ..turn(1, "unused")
        })])
    };
    let (_, body, _, _) = list::draw_turn_list(&view, 0, Rect::new(0, 0, 160, 10));
    let text: String = body
        .iter()
        .flat_map(|l| l.spans.iter())
        .map(|s| s.content.as_ref())
        .collect();
    assert!(
        !text.contains("…1.2k"),
        "the ellipsis is not glued to the token cell: {text}"
    );
    let (title_cell, _) = cell_of(&body[0], |c| c.contains('…'));
    assert!(
        body[0].spans[title_cell].content.ends_with(' '),
        "the truncated title keeps a gap before the next cell: {text}"
    );
}

/// The token column is sized to its widest row, so a session that reached the
/// millions cannot push the columns after it out of line, and a million or more
/// rolls up to m rather than printing hundreds of thousands off a thousands
/// count.
#[test]
fn test_token_column_sized() {
    let mk = |n: usize, tin: usize, tout: usize| TrajectoryTurn {
        tokens_in: Some(tin),
        tokens_out: Some(tout),
        duration_ms: 100,
        ..turn(n, "title")
    };
    let view = TrajectoryView {
        tokens_in: Some(1_117_800),
        tokens_out: Some(2_700),
        ..view(vec![
            TrajectoryRow::Turn(mk(1, 1_117_800, 2_700)),
            TrajectoryRow::Turn(mk(2, 1_200, 45)),
        ])
    };
    let (_, body, _, _) = list::draw_turn_list(&view, 0, Rect::new(0, 0, 160, 20));
    let text: String = body
        .iter()
        .flat_map(|l| l.spans.iter())
        .map(|s| s.content.as_ref())
        .collect();
    assert!(
        text.contains("1.1m in 2.7k out"),
        "a million or more rolls up to m: {text}"
    );
    assert!(
        !text.contains("1117800"),
        "the full count is gone once the short figure carries the magnitude: {text}"
    );
    assert_eq!(
        column_of(&body[0], |c| c.contains('✓')),
        column_of(&body[1], |c| c.contains('✓')),
        "the rows close at one display column whatever the token counts"
    );
}

/// A row that carried no cache ratio, no reasoning, and no call count spends no
/// width on those columns, so the figures it does have sit together instead of
/// being separated by a stretch of blanks.
#[test]
fn test_empty_columns_dropped() {
    let view = TrajectoryView {
        tokens_in: Some(1_000),
        tokens_out: Some(200),
        ..view(vec![TrajectoryRow::Turn(TrajectoryTurn {
            tokens_in: Some(1_000),
            tokens_out: Some(200),
            duration_ms: 100,
            ..turn(1, "title")
        })])
    };
    // Below the call column's own width gate, so the only columns left are the
    // ones whose figures this turn has.
    let (_, body, _, _) = list::draw_turn_list(&view, 0, Rect::new(0, 0, 90, 10));
    let (ti, tcol) = cell_of(&body[0], |c| c.contains(" in "));
    let tw = UnicodeWidthStr::width(body[0].spans[ti].content.as_ref());
    assert_eq!(
        column_of(&body[0], |c| c.contains("ms")),
        tcol + tw,
        "the duration follows the token cell directly when nothing between them has a figure"
    );
    // At a width where the reasoning column's own gate passes, a session whose
    // turns never reported reasoning still spends no width on it: the token
    // cell, the call cell, and the duration are adjacent.
    let (_, wide, _, _) = list::draw_turn_list(&view, 0, Rect::new(0, 0, 200, 10));
    let (wi, wcol) = cell_of(&wide[0], |c| c.contains(" in "));
    let ww = UnicodeWidthStr::width(wide[0].spans[wi].content.as_ref());
    let (ci, _) = cell_of(&wide[0], |c| c.trim() == "direct");
    let cw = UnicodeWidthStr::width(wide[0].spans[ci].content.as_ref());
    assert_eq!(
        column_of(&wide[0], |c| c.contains("ms")),
        wcol + ww + cw,
        "no column is reserved between the figures this turn has"
    );
}

/// When one row carries a figure and another does not, the column stays for
/// both, so the columns of the row below the filled one stay in line with it.
#[test]
fn test_column_held_when_blank() {
    let view = TrajectoryView {
        tokens_in: Some(2_000),
        tokens_out: Some(400),
        cache_read: Some(900),
        ..view(vec![
            TrajectoryRow::Turn(TrajectoryTurn {
                tokens_in: Some(1_000),
                tokens_out: Some(200),
                cache_read: Some(900),
                reasoning_tokens: Some(50),
                duration_ms: 100,
                ..turn(1, "measured")
            }),
            TrajectoryRow::Turn(TrajectoryTurn {
                tokens_in: Some(1_000),
                tokens_out: Some(200),
                cache_read: Some(0),
                duration_ms: 100,
                ..turn(2, "unmeasured")
            }),
        ])
    };
    let (_, body, _, _) = list::draw_turn_list(&view, 0, Rect::new(0, 0, 200, 20));
    assert_eq!(
        column_of(&body[0], |c| c.contains("ms")),
        column_of(&body[1], |c| c.contains("ms")),
        "the row without the figure keeps the column the row above it filled"
    );
}

/// A model id wider than its column is cut with an ellipsis, and the cells
/// after it stay in line on both rows.
#[test]
fn test_long_model_cut() {
    let mk = |n: usize, model: &str| TrajectoryTurn {
        tokens_in: Some(1_000),
        tokens_out: Some(200),
        models: vec![model.to_string()],
        duration_ms: 100,
        ..turn(n, "title")
    };
    let view = TrajectoryView {
        models_used: 2,
        tokens_in: Some(2_000),
        tokens_out: Some(400),
        ..view(vec![
            TrajectoryRow::Turn(mk(1, "qwen3.7-max")),
            TrajectoryRow::Turn(mk(2, "a-model-id-far-longer-than-its-column")),
        ])
    };
    let (_, body, _, _) = list::draw_turn_list(&view, 0, Rect::new(0, 0, 200, 20));
    let text: String = body
        .iter()
        .flat_map(|l| l.spans.iter())
        .map(|s| s.content.as_ref())
        .collect();
    assert!(
        text.contains('…'),
        "an id wider than its column is cut: {text}"
    );
    assert_eq!(
        column_of(&body[0], |c| c.contains("ms")),
        column_of(&body[1], |c| c.contains("ms")),
        "the duration stays put whatever the id length"
    );
}

/// A count of one is stated in the singular, on the row and in the session
/// header, so a single call does not read as a defect.
#[test]
fn test_counts_read_singular() {
    let view = TrajectoryView {
        total_turns: 1,
        tool_calls: 1,
        tokens_in: Some(1_200),
        tokens_out: Some(45),
        ..view(vec![TrajectoryRow::Turn(TrajectoryTurn {
            tokens_in: Some(1_200),
            tokens_out: Some(45),
            tool_count: 1,
            duration_ms: 100,
            ..turn(1, "title")
        })])
    };
    let (header, body, _, _) = list::draw_turn_list(&view, 0, Rect::new(0, 0, 160, 10));
    let head: String = header
        .iter()
        .flat_map(|l| l.spans.iter())
        .map(|s| s.content.as_ref())
        .collect();
    let text: String = body
        .iter()
        .flat_map(|l| l.spans.iter())
        .map(|s| s.content.as_ref())
        .collect();
    assert!(
        text.contains("1 call") && !text.contains("1 calls"),
        "the row counts one call: {text}"
    );
    assert!(
        head.contains("1 turn ·") && !head.contains("1 turns"),
        "the header counts one turn: {head}"
    );
    assert!(
        head.contains("1 call") && !head.contains("1 calls"),
        "the header counts one call: {head}"
    );
}
