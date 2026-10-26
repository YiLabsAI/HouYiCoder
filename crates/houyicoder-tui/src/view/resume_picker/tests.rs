use super::*;
use crate::resume_picker::{SessionRow, TitleSource};
use crate::state::App;
use crate::test_harness::working_app;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use unicode_width::UnicodeWidthStr;

/// A row renders the time, the title, and the cwd basename, and never the
/// sid (the design density decision).
#[test]
fn test_format_row_layout() {
    let row = SessionRow {
        sid_str: "secret-sid".into(),
        title: "login flow".into(),
        cwd_basename: "app".into(),
        last_active: 120,
        ..Default::default()
    };
    let line = format_line(&row, 240, 80, false);
    let rendered: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(rendered.contains("2m"), "time column: {rendered}");
    assert!(rendered.contains("login flow"), "title: {rendered}");
    assert!(rendered.contains("app"), "cwd basename: {rendered}");
    assert!(
        !rendered.contains("secret-sid"),
        "sid must not show: {rendered}"
    );
}

/// Truncation appends an ellipsis when the title exceeds the column budget.
#[test]
fn test_truncate_appends_ellipsis() {
    let s = "abcdefghij";
    assert_eq!(truncate_width(s, 10), "abcdefghij");
    assert_eq!(truncate_width(s, 0), "");
    let out = truncate_width(s, 5);
    assert!(out.ends_with('\u{2026}'));
    assert!(
        UnicodeWidthStr::width(out.as_str()) <= 5,
        "width overflow: {out}"
    );
}

/// The selected row carries a marker, and it is the only row that does: the
/// other rows keep their records readable without a second highlight.
#[test]
fn test_marker_marks_selected_row() {
    let buf = render_picker(two_rows(), 0, 80, 8);
    let marks: Vec<(u16, u16)> = cells_matching(&buf, '❯');
    assert_eq!(marks.len(), 1, "one marker for the one selection");
    assert_eq!(marks[0].1, 1, "the marker sits on the selected row");
}

/// The topic line is the secondary fact, so it stays dim even under the
/// selection: the highlight belongs to the title it explains.
#[test]
fn test_topic_line_stays_dim() {
    let buf = render_picker(two_rows(), 0, 80, 8);
    let (y, x) = find_text(&buf, "topic:").expect("the selected row shows its topic");
    let end = x + UnicodeWidthStr::width("topic:") as u16;
    for col in x..end {
        let cell = buf.cell((col, y)).expect("topic cell");
        assert_eq!(
            cell.fg,
            Color::DarkGray,
            "topic cell {col} is dim, not part of the highlight"
        );
        assert!(
            !cell.modifier.contains(Modifier::BOLD),
            "topic cell {col} carries no highlight modifier"
        );
    }
}

/// The topic line starts where the title starts, so the two read as one
/// column of text rather than two starting points.
#[test]
fn test_topic_shares_title_column() {
    let buf = render_picker(two_rows(), 0, 80, 8);
    let (title_y, title_x) =
        find_text(&buf, "wire the cache table").expect("the title renders in its column");
    let (topic_y, topic_x) = find_text(&buf, "topic:").expect("the topic renders below it");
    assert!(
        topic_y > title_y,
        "the topic sits under the row it explains"
    );
    assert_eq!(
        topic_x, title_x,
        "the topic aligns with the title column, not the marker gutter"
    );
}

fn two_rows() -> Vec<SessionRow> {
    vec![
        SessionRow {
            sid_str: "sid-a".into(),
            title: "wire the cache table".into(),
            title_source: TitleSource::LastPrompt,
            cwd_basename: "app".into(),
            first_prompt: Some("open the repo".into()),
            last_prompt: Some("wire the cache table".into()),
            last_active: 120,
            log_size: 4096,
        },
        SessionRow {
            sid_str: "sid-b".into(),
            title: "rework the retry budget".into(),
            title_source: TitleSource::LastPrompt,
            cwd_basename: "lib".into(),
            last_active: 60,
            log_size: 1024,
            ..Default::default()
        },
    ]
}

fn render_picker(rows: Vec<SessionRow>, sel: usize, width: u16, height: u16) -> Buffer {
    let mut app: App = working_app();
    let picker = &mut app.ui.navigation.resume_picker;
    picker.open = true;
    picker.rows = rows;
    picker.sel = sel;
    let backend = TestBackend::new(width, height);
    let mut term = Terminal::new(backend).expect("test backend");
    term.draw(|f| draw_content(f, f.area(), &app))
        .expect("draw picker");
    term.backend().buffer().clone()
}

fn row_text(buf: &Buffer, y: u16) -> String {
    let area = buf.area();
    (0..area.width)
        .map(|x| buf.cell((x, y)).expect("cell").symbol().to_string())
        .collect()
}

fn find_text(buf: &Buffer, needle: &str) -> Option<(u16, u16)> {
    let area = buf.area();
    (0..area.height).find_map(|y| {
        let text = row_text(buf, y);
        text.find(needle)
            .map(|byte_col| (y, UnicodeWidthStr::width(&text[..byte_col]) as u16))
    })
}

fn cells_matching(buf: &Buffer, want: char) -> Vec<(u16, u16)> {
    let area = buf.area();
    let mut found = Vec::new();
    for y in 0..area.height {
        for x in 0..area.width {
            let cell = buf.cell((x, y)).expect("cell");
            if cell.symbol() == want.to_string() {
                found.push((x, y));
            }
        }
    }
    found
}
