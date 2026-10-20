//! Tests for the turn list's columns: what each cell states and where it sits
//! across the rows.

use super::super::list;
use super::super::*;
use super::fixtures::{lines_text, turn, view};
use ratatui::layout::Rect;
use ratatui::text::Line;
use std::slice::from_ref;

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

/// The display column where needle first appears in the line's text. Byte
/// offsets inside the spans are counted as display columns, so a glyph wider
/// than one column on an earlier span does not shift the answer.
fn text_column(line: &Line<'static>, needle: &str) -> usize {
    let mut col = 0usize;
    for span in &line.spans {
        let text = span.content.as_ref();
        if let Some(at) = text.find(needle) {
            return col + UnicodeWidthStr::width(&text[..at]);
        }
        col += UnicodeWidthStr::width(text);
    }
    panic!("no {needle:?} in the row: {:?}", line.spans);
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

/// The two token counts are subcolumns of one cell: the in and out keywords
/// hold the same display column on every row, so the numbers line up whatever
/// each figure is wide. A short figure is padded onto its subcolumn rather
/// than left where it lands.
///
/// The two rows deliberately differ in figure width on both counts: with
/// figures of one width the two subcolumns have one width between them, and a
/// left-aligned cell would hold the same columns, so the assertions below
/// would pass without the code under test.
#[test]
fn test_token_columns_hold() {
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
            TrajectoryRow::Turn(mk(2, 45, 5)),
        ])
    };
    let (_, body, _, _) = list::draw_turn_list(&view, 0, Rect::new(0, 0, 160, 20));
    assert_eq!(
        text_column(&body[0], " in "),
        text_column(&body[1], " in "),
        "the in keyword holds one display column across rows: {}",
        lines_text(&body)
    );
    assert_eq!(
        text_column(&body[0], " out"),
        text_column(&body[1], " out"),
        "the out keyword holds one display column across rows: {}",
        lines_text(&body)
    );
    assert!(
        lines_text(from_ref(&body[1])).contains("  45 in    5 out"),
        "the short counts sit on their subcolumns: {}",
        lines_text(&body)
    );
}

/// A session with no failures states the count quietly, and a count above zero
/// keeps the red that draws the eye to it.
#[test]
fn test_header_fail_count_color() {
    let rows = |failures: usize| TrajectoryView {
        failures,
        tokens_in: Some(1_000),
        tokens_out: Some(200),
        ..view(vec![TrajectoryRow::Turn(TrajectoryTurn {
            tokens_in: Some(1_000),
            tokens_out: Some(200),
            tool_fail: failures,
            duration_ms: 100,
            ..turn(1, "title")
        })])
    };
    let failures_cell = |failures: usize| {
        let (header, _, _, _) = list::draw_turn_list(&rows(failures), 0, Rect::new(0, 0, 160, 10));
        let (i, _) = cell_of(&header[0], |c| c.contains("fail"));
        header[0].spans[i].style.fg
    };
    assert_eq!(
        failures_cell(0),
        Some(Color::DarkGray),
        "a session with no failures does not raise an alarm"
    );
    assert_eq!(
        failures_cell(2),
        Some(Color::Red),
        "a failure count above zero keeps its red"
    );
}

/// A session that has run no turns yet says so in the body: an empty area
/// under the header would read as a list that failed to load.
#[test]
fn test_empty_session_states_none() {
    let (_, body, _, _) = list::draw_turn_list(&view(vec![]), 0, Rect::new(0, 0, 160, 10));
    assert!(
        lines_text(&body).contains("no turns recorded yet"),
        "an empty list states that it is empty: {}",
        lines_text(&body)
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

/// A turn that switched models keeps the switch visible in the model cell: the
/// cell names the first id and counts the rest, so a long first id cannot
/// spend the whole column and hide that a second model ran.
#[test]
fn test_model_cell_names_switch() {
    let turn = TrajectoryTurn {
        models: vec![
            "deepseek-v4-pro-0813".to_string(),
            "deepseek-v4.1-flash".to_string(),
        ],
        duration_ms: 100,
        ..turn(1, "long answer")
    };
    let view = TrajectoryView {
        models_used: 2,
        ..view(vec![TrajectoryRow::Turn(turn)])
    };
    let (_, body, _, _) = list::draw_turn_list(&view, 0, Rect::new(0, 0, 200, 10));
    let text: String = body[0].spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(
        text.contains("+1"),
        "the model cell hides the second model: {text}"
    );
    assert!(
        !text.contains("deepseek-v4-pro-0813,deepseek"),
        "the cell still joins every id and truncates: {text}"
    );
}

/// Model names shorten against the set they appear in: a same-vendor pair
/// keeps its distinguishing tail, and a mixed-vendor pair keeps full names
/// because they share no boundary-safe prefix.
#[test]
fn test_model_names_shorten() {
    let deepseek = vec![
        "deepseek-v4-pro-0813".to_string(),
        "deepseek-v4.1-flash".to_string(),
    ];
    assert_eq!(
        short_model_names(&deepseek),
        vec!["v4-pro-0813", "v4.1-flash"]
    );
    let mixed = vec![
        "deepseek-v4-pro-0813".to_string(),
        "qwen3.8-max".to_string(),
    ];
    assert_eq!(short_model_names(&mixed), mixed);
    let same = vec!["qwen3.8-max".to_string(), "qwen3.8-flash".to_string()];
    assert_eq!(short_model_names(&same), vec!["max", "flash"]);
    assert_eq!(
        short_model_name("not-in-the-set", &deepseek),
        "not-in-the-set",
        "an unknown model keeps its full name"
    );
}
