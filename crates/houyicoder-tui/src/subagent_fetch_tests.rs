//! Wired round-trip tests for the on-demand child transcript fetch. The pure
//! fill + projection are pinned in agent_dispatch_tests; these pin the
//! cross-layer wiring: a first expand fires a ChildTranscriptQuery, the server
//! replays the child log and projects, the driver forwards the reply, and the
//! fill arm swaps the child rows into the fold-group in place.
use super::*;
use crate::records::TranscriptLine;

/// A first expand of an unloaded Subagent line fires the fetch over the wire.
/// For a sid the store has no log for, the server returns an empty frame list
/// and the reply lands as the unavailable line in the fold-group. This proves
/// the full round-trip without a live child run; the projection itself is
/// pinned by the pure fill test.
#[test]
fn test_expand_fetches_child_wired() {
    let provider = Arc::new(FakeProvider::new(vec![]));
    let mut app = app_with_provider(provider, ToolRegistry::new());
    app.screen = crate::state::Screen::Working;
    app.transcript.push(TranscriptLine::Subagent {
        child_sid: "c1".into(),
        subagent_type: "explore".into(),
        summary: "found auth".into(),
        prompt: String::new(),
        folded_transcript: Vec::new(),
        color: None,
    });
    // First expand: fires a one-shot fetch (the line has no child rows yet).
    assert!(app.toggle_subagent_expand(), "toggle targeted the subagent");
    assert!(app.expanded_subagents.contains("c1"), "expanded");
    // Pump the driver round-trip until the fill arm swaps in the child rows
    // (the server reply for a sid with no log is an empty frame list, which
    // the fill surfaces as an unavailable line). A stuck wiring times out
    // here rather than passing silently.
    let mut filled = false;
    for _ in 0..2000 {
        app.poll_agent();
        if let TranscriptLine::Subagent {
            folded_transcript, ..
        } = &app.transcript[0]
            && !folded_transcript.is_empty()
        {
            filled = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    assert!(filled, "fetch round-trip populated the fold-group");
    match &app.transcript[0] {
        TranscriptLine::Subagent {
            folded_transcript, ..
        } => {
            assert_eq!(
                folded_transcript.len(),
                1,
                "one unavailable line for no-log sid"
            );
            assert!(
                matches!(&folded_transcript[0], TranscriptLine::System(s) if s.contains("unavailable")),
                "unavailable line for the no-log child sid, got {:?}",
                folded_transcript[0]
            );
        }
        other => panic!("subagent line preserved, got {other:?}"),
    }
}
