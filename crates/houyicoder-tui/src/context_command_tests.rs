//! /context command tests split from run_control_tests.rs for the file-size
//! gate. The /context cache fast-path + the ContextResult grid landing.
use super::*;

/// The dedup in agent_dispatch (pop stale ContextGrid before pushing the fresh
/// one) must handle a non-grid line landing between the cache fast-path push
/// and the ContextResult reply. The prior check was transcript.last() is
/// ContextGrid -- if a System line intervened, the pop was skipped and a
/// duplicate grid appeared. This test injects a System line between the two
/// and asserts only one grid from the second call (the stale fast-path grid
/// was popped).
#[test]
fn test_dedup_handles_intervening_line() {
    use houyicoder_protocol::frontend::SlashCommand;
    let provider = Arc::new(FakeProvider::new(vec![]));
    let mut app = app_with_provider(provider, ToolRegistry::new());
    app.screen = crate::state::Screen::Working;
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
    let grids_after_first: usize = app
        .transcript
        .iter()
        .filter(|l| matches!(l, TranscriptLine::ContextGrid(_)))
        .count();
    // Second /context: cache hit -> fast-path pushes a grid + sends query.
    app.run_command(SlashCommand::Context);
    // A non-grid line lands between the fast-path push and the ContextResult.
    app.system_line("concurrent event");
    // Poll until the ContextResult arrives (grid count changes).
    for _ in 0..1000 {
        app.poll_agent();
        let grids: usize = app
            .transcript
            .iter()
            .filter(|l| matches!(l, TranscriptLine::ContextGrid(_)))
            .count();
        if grids != grids_after_first + 1 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    let grids_final: usize = app
        .transcript
        .iter()
        .filter(|l| matches!(l, TranscriptLine::ContextGrid(_)))
        .count();
    // Desired: fast-path grid was popped by dedup -> only the reply grid
    // remains -> total = grids_after_first + 1.
    // Bug: dedup skipped (System line blocks the last-line check) -> both
    // grids remain -> total = grids_after_first + 2.
    assert_eq!(
        grids_final,
        grids_after_first + 1,
        "dedup should pop stale fast-path grid even when a non-grid line intervenes"
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
    use crate::test_support::render_buffer;
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

/// /undo on a wired app ships an UndoQuery + surfaces the reply. The undo
/// stack is empty on a fresh session, so the reply is "nothing to undo".
#[test]
fn test_undo_ships_when_wired() {
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
