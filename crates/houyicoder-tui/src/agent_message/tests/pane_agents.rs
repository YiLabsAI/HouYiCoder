use super::super::*;
use crate::records::TranscriptLine;

fn subagent(sid: &str, summary: &str, fold: Vec<TranscriptLine>) -> TranscriptLine {
    TranscriptLine::Subagent {
        child_sid: sid.into(),
        subagent_type: "explore".into(),
        summary: summary.into(),
        prompt: String::new(),
        folded_transcript: fold,
        color: None,
    }
}

/// Refresh rebuilds the rows when the version moves and is a no-op when
/// it has not, so calling it on every agent message costs nothing.
#[test]
fn test_refresh_rebuilds_on_version() {
    let mut pane = PaneAgents::default();
    let t = vec![subagent("c1", "first", Vec::new())];
    pane.refresh(&t, 1);
    assert_eq!(pane.rows.len(), 1);
    assert_eq!(pane.rows[0].child_sid, "c1");
    // Same version: a second call does not rebuild even if the
    // transcript changed under it (the caller is the source of truth).
    let t2 = vec![
        subagent("c1", "first", Vec::new()),
        subagent("c2", "second", Vec::new()),
    ];
    pane.refresh(&t2, 1);
    assert_eq!(pane.rows.len(), 1, "same version = no rebuild");
    pane.refresh(&t2, 2);
    assert_eq!(pane.rows.len(), 2, "new version = rebuild");
}

/// A loaded fold is detected so the pane knows the drill-in needs no
/// fetch; an empty fold fires the on-demand load.
#[test]
fn test_refresh_marks_loaded() {
    let mut pane = PaneAgents::default();
    let t = vec![
        subagent("c1", "loaded", vec![TranscriptLine::Agent("reply".into())]),
        subagent("c2", "empty", Vec::new()),
    ];
    pane.refresh(&t, 1);
    assert!(pane.rows[0].loaded, "non-empty fold = loaded");
    assert!(!pane.rows[1].loaded, "empty fold = not loaded");
}
