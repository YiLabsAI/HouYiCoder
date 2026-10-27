//! The away recap as the terminal shows it.

use super::*;
use crate::records::RECAP_MARKER;
use crate::transcript::TranscriptFrame;
use crate::transcript::transcript_from_frames;
use houyicoder_protocol::acpx::AcpxMethod;
use houyicoder_protocol::acpx::AcpxNotification;

fn recap_line(text: &str) -> TranscriptLine {
    let frames = vec![TranscriptFrame::Acpx(AcpxNotification::new(
        AcpxMethod::ContextRecap,
        serde_json::json!({ "text": text }),
    ))];
    let mut lines = transcript_from_frames(&frames, 0..frames.len(), false);
    assert_eq!(lines.len(), 1, "one recap notification is one line");
    lines.remove(0)
}

/// A recap notice without its text draws nothing rather than an empty row:
/// a bare marker would read as the host summarizing the work and then
/// saying nothing. A payload the type cannot decode and a payload whose text
/// is empty or blank both leave the transcript untouched.
#[test]
fn test_recap_needs_text() {
    for payload in [serde_json::json!({}), serde_json::json!({ "text": "" })] {
        let frames = vec![TranscriptFrame::Acpx(AcpxNotification::new(
            AcpxMethod::ContextRecap,
            payload,
        ))];
        assert!(
            transcript_from_frames(&frames, 0..frames.len(), false).is_empty(),
            "a recap with no text leaves no row"
        );
    }
    let frames = vec![TranscriptFrame::Acpx(AcpxNotification::new(
        AcpxMethod::ContextRecap,
        serde_json::json!({ "text": "   \n\t " }),
    ))];
    assert!(
        transcript_from_frames(&frames, 0..frames.len(), false).is_empty(),
        "a blank recap leaves no row"
    );
    // Blank lines around the sentence are not part of it: kept, they would
    // draw rows holding only the marker.
    match recap_line("\n  fixed the parser  \n") {
        TranscriptLine::Recap(text) => assert_eq!(text, "fixed the parser"),
        other => panic!("a padded sentence is still a recap: {other:?}"),
    }
}

fn working_app(lines: Vec<TranscriptLine>) -> App {
    let mut app = composition::app();
    app.ui.navigation.screen = Screen::Working;
    app.sessions.current.transcript = lines.into();
    app
}

/// The summary reads as one marker-led sentence at the transcript tail, in
/// the dim style a notice carries. A control sequence the model emitted is
/// removed before the terminal is given the text: the answer is model output,
/// and a summary that could repaint the frame is not a summary.
#[test]
fn test_recap_draws_dim_marker() {
    let app = working_app(vec![
        TranscriptLine::User("hi".to_string()),
        recap_line("fixing the parser \u{1b}[31mnext: run the failing test"),
    ]);

    let out = render_text(&app, 80, 24);
    assert!(
        out.contains("※ recap: fixing the parser next: run the failing test"),
        "the summary draws under its marker with control sequences removed:\n{out}"
    );

    let buf = render_buffer(&app, 80, 24);
    let row = (0..24u16)
        .find(|y| {
            (0..80u16)
                .filter_map(|x| buf.cell((x, *y)))
                .any(|cell| cell.symbol() == "※")
        })
        .expect("the marker is drawn");
    let marker = (0..80u16)
        .filter_map(|x| buf.cell((x, row)))
        .find(|cell| cell.symbol() == "※")
        .expect("marker cell");
    assert_eq!(
        marker.fg,
        Color::DarkGray,
        "the summary reads as a notice, not a turn"
    );
}

/// A long summary wraps by display columns inside the transcript region, and
/// the row count follows it. Nothing below the transcript moves: the summary
/// grows the conversation, never the rows the caret hangs off.
#[test]
fn test_recap_wraps_within_transcript() {
    let short = working_app(vec![TranscriptLine::User("hi".to_string())]);
    let before = render_text(&short, 60, 24);

    let mut long = working_app(vec![
        TranscriptLine::User("hi".to_string()),
        recap_line(
            "fixing the argument parser so a quoted flag keeps its quotes, \
             and the next step is running the failing test",
        ),
    ]);
    let out = render_text(&long, 60, 24);
    // The rows below the transcript — the status row and the input box the
    // caret hangs off — draw the same cells as before the summary landed, so
    // the summary grows the conversation and nothing else.
    let tail = |text: &str| -> Vec<String> {
        let lines: Vec<&str> = text.lines().collect();
        lines[lines.len().saturating_sub(4)..]
            .iter()
            .map(|row| row.to_string())
            .collect()
    };
    assert_eq!(
        tail(&out),
        tail(&before),
        "a summary must not move the rows below the transcript:\n{out}"
    );
    let rows = out
        .lines()
        .filter(|row| row.contains("recap:") || row.contains("failing test"))
        .count();
    assert!(
        rows >= 2,
        "a summary wider than the pane wraps instead of truncating:\n{out}"
    );
    // The draw publishes its total; clearing it forces the count path to walk
    // the same lines for itself, so a row the draw made and the count missed
    // shows up as a difference.
    let drawn = long.whole_scroll().total.get();
    long.active_scroll_mut().total.set(0);
    assert_eq!(
        long.fold_aware_rows(None),
        drawn,
        "the count path must see the same rows the draw produced"
    );
}

/// A summary longer than one row wraps under its own words rather than under
/// the marker: every continuation row starts at the column the sentence
/// starts at, so the notice reads as one sentence and not as a stack of
/// repeated markers.
#[test]
fn test_recap_hangs_continuation_rows() {
    let app = working_app(vec![
        TranscriptLine::User("hi".to_string()),
        recap_line(
            "fixing the argument parser so a quoted flag keeps its quotes, \
             and the next step is running the failing test",
        ),
    ]);
    let out = render_text(&app, 60, 24);
    let lines: Vec<&str> = out.lines().collect();
    let marker_row = lines
        .iter()
        .position(|row| row.contains("※ recap: "))
        .expect("the marker row is drawn");
    let marker_line = lines[marker_row];
    let start = marker_line.find("fixing").expect("the sentence starts");
    let text_col = UnicodeWidthStr::width(&marker_line[..start]);
    let first =
        UnicodeWidthStr::width(&marker_line[..marker_line.len() - marker_line.trim_start().len()]);
    let continuation: Vec<&str> = lines[marker_row + 1..]
        .iter()
        .take_while(|row| !row.trim().is_empty())
        .copied()
        .collect();
    assert!(
        continuation.len() >= 2,
        "the summary wraps onto continuation rows:\n{out}"
    );
    // The sentence starts past the marker, counted from the row's own indent.
    assert_eq!(
        text_col,
        first + UnicodeWidthStr::width(RECAP_MARKER),
        "the sentence starts beside the marker's end:\n{out}"
    );
    for row in continuation {
        let indent = &row[..row.len() - row.trim_start().len()];
        assert_eq!(
            UnicodeWidthStr::width(indent),
            text_col,
            "a continuation row hangs under the sentence, not under the marker:\n{out}"
        );
    }
}

/// The summary is written in the user's language, so a wide character must
/// wrap the same way a narrow one does: the whole sentence lands across
/// several rows rather than being clipped, and the count path sees the rows
/// the draw produced.
#[test]
fn test_recap_wraps_wide_text() {
    let text = "修复解析器：下一步运行失败的测试，然后检查引号参数的边界";
    let short = working_app(vec![TranscriptLine::User("hi".to_string())]);
    let short_rows = render_text(&short, 30, 24)
        .lines()
        .filter(|row| !row.trim().is_empty())
        .count();
    let mut app = working_app(vec![
        TranscriptLine::User("hi".to_string()),
        recap_line(text),
    ]);
    let out = render_text(&app, 30, 24);
    // A wide glyph occupies two cells, so the buffer dump shows it beside its
    // empty continuation cell: compare the glyphs, not the spacing.
    let drawn: String = out.chars().filter(|c| !c.is_whitespace()).collect();
    let expected: String = format!("※recap:{text}")
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    assert!(
        drawn.contains(&expected),
        "the whole summary lands, not a clipped prefix:\n{out}"
    );
    let long_rows = out.lines().filter(|row| !row.trim().is_empty()).count();
    assert!(
        long_rows >= short_rows + 3,
        "a summary this wide wraps instead of staying on one row:\n{out}"
    );
    // Clearing the published total makes the count path walk the wide rows
    // for itself, so a wrap that drops a row here reads as a difference.
    let drawn = app.whole_scroll().total.get();
    app.active_scroll_mut().total.set(0);
    assert_eq!(
        app.fold_aware_rows(None),
        drawn,
        "the count path must see the same rows the draw produced"
    );
}

/// A reader who scrolled up keeps their place when a recap lands. The
/// summary is transcript content, not a notice that steals the view from
/// someone reading earlier work.
#[test]
fn test_recap_keeps_scrolled_reader() {
    let mut lines = vec![TranscriptLine::User("hi".to_string())];
    for i in 0..40 {
        lines.push(TranscriptLine::Agent(format!("line {i}")));
    }
    let mut app = working_app(lines);
    drop(render_text(&app, 80, 12));
    app.park_scroll_at(3);
    let before = render_text(&app, 80, 12);
    let top = app.whole_scroll().top_offset();
    app.push_transcript_line(recap_line("next: run the failing test"));
    let after = render_text(&app, 80, 12);
    assert!(
        !app.whole_scroll().is_following_tail(),
        "the reader stays scrolled up after the summary lands"
    );
    assert_eq!(
        app.whole_scroll().top_offset(),
        top,
        "the summary must not move the view"
    );
    assert_eq!(
        before.lines().next(),
        after.lines().next(),
        "the top of the screen is the same row it was"
    );
}
