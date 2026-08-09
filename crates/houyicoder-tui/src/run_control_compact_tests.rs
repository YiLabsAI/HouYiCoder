use super::*;

/// A ContextResult lands as a ContextGrid transcript line (the inline grid),
/// not a flat system string. Guards the /context handler regression where the
/// breakdown routed to system_line instead of the widget.
#[test]
fn test_context_result_pushes_line() {
    use houyicoder_protocol::frontend::context::stub_breakdown;
    let provider = Arc::new(FakeProvider::new(vec![]));
    let mut app = app_with_provider(provider, ToolRegistry::new());
    app.handle_agent_message(AgentMessage::ContextResult {
        breakdown: stub_breakdown(),
    });
    assert!(
        app.transcript
            .iter()
            .any(|l| matches!(l, TranscriptLine::ContextGrid(_))),
        "ContextResult must render as the inline grid, not a flat system line"
    );
}

/// A CompactResult lands as an honest one-line system message (Compacted N
/// events + token drop). The checkpoint id is internal (a future rewind
/// handle) so it stays out of the transcript. Pins the /compact dispatch
/// round-trip so a later refactor cannot drop the reply rendering.
#[test]
fn test_result_renders_system_line() {
    let provider = Arc::new(FakeProvider::new(vec![]));
    let mut app = app_with_provider(provider, ToolRegistry::new());
    app.handle_agent_message(AgentMessage::CompactResult {
        reply: houyicoder_protocol::frontend::compact::CompactReply::new(
            true,
            12,
            "ckpt_abc",
            Some(8000),
            Some(3000),
        ),
    });
    let line = app
        .transcript
        .iter()
        .find_map(|l| match l {
            TranscriptLine::System(s) => Some(s.clone()),
            _ => None,
        })
        .expect("compact reply lands a system line");
    assert!(line.contains("Compacted 12 events"), "folded count: {line}");
    assert!(line.contains("8000 → 3000 tokens"), "token drop: {line}");
    // The checkpoint id is internal, kept out of the transcript.
    assert!(
        !line.contains("ckpt_abc"),
        "checkpoint id should not be in transcript: {line}"
    );
}

/// A no-progress CompactResult surfaces honestly ("Not enough messages to
/// compact.") instead of a silent failure, so the user knows compaction was
/// a no-op. The wording is exact: "Not enough messages to compact."
#[test]
fn test_result_no_progress_honest() {
    let provider = Arc::new(FakeProvider::new(vec![]));
    let mut app = app_with_provider(provider, ToolRegistry::new());
    app.handle_agent_message(AgentMessage::CompactResult {
        reply: houyicoder_protocol::frontend::compact::CompactReply::new(
            false,
            0,
            "ckpt_empty",
            None,
            None,
        ),
    });
    assert!(
        app.transcript
            .iter()
            .any(|l| matches!(l, TranscriptLine::System(s) if s.contains("Not enough messages to compact"))),
        "no-progress reply honest"
    );
}

/// /compact ships a CompactQuery over the wire when a session is wired, and
/// the reply (a no-progress outcome on an empty session) lands as a system
/// line. No "compacting…" spinner lands in the transcript — a compact is a
/// meta-operation, not a conversation event, so its transient state stays
/// out; only the outcome line lands.
#[test]
fn test_compact_dispatch_round_trip() {
    use houyicoder_protocol::frontend::SlashCommand;
    let provider = Arc::new(FakeProvider::new(vec![]));
    let mut app = app_with_provider(provider, ToolRegistry::new());
    app.run_command(SlashCommand::Compact);
    assert!(
        !app.transcript
            .iter()
            .any(|l| matches!(l, TranscriptLine::System(s) if s.contains("compacting"))),
        "no compacting spinner in transcript (transient state stays out)"
    );
    // The reply lands within a bounded poll window (the server's runner.compact
    // on an empty session returns a no-progress outcome quickly).
    for _ in 0..200 {
        app.poll_agent();
        if app.transcript.iter().any(|l| {
            matches!(
                l,
                TranscriptLine::System(s) if s.contains("compact")
            )
        }) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(
        app.transcript
            .iter()
            .any(|l| matches!(l, TranscriptLine::System(s) if s.contains("Not enough messages to compact"))),
        "compact reply should land the no-op outcome line"
    );
}
