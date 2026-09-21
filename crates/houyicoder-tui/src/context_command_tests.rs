//! /context command tests split from run_control_tests.rs for the file-size
//! gate. The /context cache fast-path + the ContextResult grid landing.
use super::*;

/// The second /context draws the cached grid at once and then swaps its payload
/// for the reply grid in place, so the transcript holds one grid either way. A
/// non-grid line landing between the two must not leave a second grid: the
/// reply carries a different total, so the grid can only show it by taking the
/// place of the one drawn before the line arrived.
#[test]
fn test_second_context_one_grid() {
    use houyicoder_protocol::frontend::SlashCommand;
    use houyicoder_protocol::frontend::context::stub_breakdown;
    let provider = Arc::new(FakeProvider::new(vec![]));
    let mut app = app_with_provider(provider, ToolRegistry::new());
    app.screen = crate::state::Screen::Working;
    // The cache a first /context filled stands in for that reply: the fast
    // path renders from it without a round trip.
    app.context_cache = Some(stub_breakdown());
    app.run_command(SlashCommand::Context);
    let grids = |app: &App| {
        app.transcript
            .iter()
            .filter(|l| matches!(l, TranscriptLine::ContextGrid(_)))
            .count()
    };
    assert_eq!(grids(&app), 1, "the fast path draws the cached grid");
    // A non-grid line lands between the fast-path push and the reply.
    app.system_line("concurrent event");
    let mut fresh = stub_breakdown();
    fresh.total_tokens = 4_242;
    app.handle_agent_message(SessionMessage::Response {
        request: RequestId(7),
        response: ServerResponse::Context { breakdown: fresh },
    });
    // Bug: the reply grid lands beside the fast-path one, leaving two grids.
    assert_eq!(
        grids(&app),
        1,
        "the reply grid takes the cached one's place"
    );
    let shows_fresh = app.transcript.iter().any(|l| {
        matches!(
            l,
            TranscriptLine::ContextGrid(view) if view.breakdown.total_tokens == 4_242
        )
    });
    assert!(shows_fresh, "the grid shows the numbers the reply carried");
    let grid_at = app
        .transcript
        .iter()
        .position(|l| matches!(l, TranscriptLine::ContextGrid(_)))
        .expect("a grid row");
    let line_at = app
        .transcript
        .iter()
        .position(|l| matches!(l, TranscriptLine::System(t) if t == "concurrent event"))
        .expect("the line that landed in between");
    assert!(
        grid_at < line_at,
        "the grid keeps the place it was drawn in, above the line that followed it"
    );
}

/// The second /context renders the cached breakdown immediately (no
/// "fetching" placeholder); the first /context populates the cache.
/// Also asserts the REAL dispatch output renders (②) — not just App state
/// (①). This is the default-gate integration test for dispatch→wire→render.
#[test]
fn test_context_cache_renders_repeat() {
    use houyicoder_protocol::frontend::SlashCommand;
    let provider = Arc::new(FakeProvider::new(vec![]));
    let mut app = app_with_provider(provider, ToolRegistry::new());
    app.run_command(SlashCommand::Context);
    for _ in 0..1000 {
        app.poll_agent();
        if app.context_cache.is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    assert!(
        app.context_cache.is_some(),
        "first /context populates cache"
    );
    // Assert the REAL dispatch output renders (② rendered buffer, not just ①
    // App state) — the default-gate wiring test for dispatch→wire→render.
    // Must be on the Working screen to render the transcript (app_with_provider
    // starts on Login).
    app.screen = crate::state::Screen::Working;
    use crate::test_harness::render_buffer;
    let buf = render_buffer(&app, 100, 40);
    let text: String = (0..buf.area().height)
        .map(|y| {
            (0..buf.area().width)
                .map(|x| buf.cell((x, y)).expect("cell").symbol().to_string())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        text.contains("Context Usage"),
        "header renders from real dispatch:\n{text}"
    );
    assert!(
        text.contains("Estimated usage by category"),
        "legend renders from real dispatch:\n{text}"
    );
    let before = app
        .transcript
        .iter()
        .filter(|l| matches!(l, TranscriptLine::ContextGrid(_)))
        .count();
    app.run_command(SlashCommand::Context);
    let after = app
        .transcript
        .iter()
        .filter(|l| matches!(l, TranscriptLine::ContextGrid(_)))
        .count();
    assert!(
        after > before,
        "second /context pushes the cached grid immediately"
    );
}

/// /undo on a connected app ships an UndoQuery + surfaces the reply. The undo
/// stack is empty on a fresh session, so the reply is "nothing to undo".
#[test]
fn test_undo_ships_connected() {
    use houyicoder_protocol::frontend::SlashCommand;
    let provider = Arc::new(FakeProvider::new(vec![]));
    let mut app = app_with_provider(provider, ToolRegistry::new());
    app.run_command(SlashCommand::Undo);
    assert!(
        app.transcript
            .iter()
            .any(|l| matches!(l, TranscriptLine::System(s) if s.contains("undo:"))),
        "/undo should surface a system line"
    );
    for _ in 0..1000 {
        app.poll_agent();
        if app
            .transcript
            .iter()
            .any(|l| matches!(l, TranscriptLine::System(s) if s.contains("undo:")))
            && app
                .transcript
                .iter()
                .filter(|l| matches!(l, TranscriptLine::System(s) if s.contains("undo:")))
                .count()
                >= 2
        {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    let undo_lines: Vec<String> = app
        .transcript
        .iter()
        .filter_map(|l| match l {
            TranscriptLine::System(s) if s.contains("undo:") => Some(s.clone()),
            _ => None,
        })
        .collect();
    assert!(
        undo_lines.len() >= 2,
        "should have the fetch line + the reply line"
    );
    assert!(
        undo_lines
            .iter()
            .any(|s| s.contains("nothing to undo") || s.contains("restored")),
        "reply should say what was undone or that the stack is empty"
    );
}
