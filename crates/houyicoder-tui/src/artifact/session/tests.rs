use super::*;

#[test]
fn test_session_annotate_appends_annotation() {
    let mut s = ArtifactSession::from_lines(&["a", "b"]);
    let ann = s.push_annotation("note".to_string());
    assert!(ann.is_some());
    assert_eq!(s.annotation_count(), 1);
    assert_eq!(s.annotations_for(0).len(), 1);
}

#[test]
fn test_session_annotate_empty_text() {
    let mut s = ArtifactSession::from_lines(&["a"]);
    assert!(s.push_annotation(String::new()).is_none());
    assert!(s.push_annotation("   ".to_string()).is_none());
    assert_eq!(s.annotation_count(), 0);
}

#[test]
fn test_session_annotate_empty_lines() {
    let mut s = ArtifactSession::from_lines(&[]);
    assert!(s.push_annotation("note".to_string()).is_none());
    assert_eq!(s.annotation_count(), 0);
}

#[test]
fn test_session_focus_down_wraps() {
    let mut s = ArtifactSession::from_lines(&[]);
    s.focus_down();
    s.focus_up();
    assert_eq!(s.focus(), 0);
}

#[test]
fn test_session_focus_wraps() {
    let mut s = ArtifactSession::from_lines(&["a", "b", "c"]);
    s.focus_down();
    assert_eq!(s.focus(), 1);
    s.focus_down();
    s.focus_down();
    assert_eq!(s.focus(), 0);
    s.focus_up();
    assert_eq!(s.focus(), 2);
}

#[test]
fn test_approve_pending_applies() {
    let mut s = ArtifactSession::from_lines(&["a", "b"]);
    s.set_pending(Some(ProposedChange {
        id: "pc-1".to_string(),
        line_start: 0,
        original: vec!["a".to_string()],
        proposed: vec!["X".to_string()],
        rationale: "replace".to_string(),
        verdict: Verdict::Pending,
    }));
    let applied = s.approve_pending();
    assert!(applied.is_some());
    assert_eq!(s.applied_count(), 1);
    assert!(s.pending_proposal().is_none());
}

#[test]
fn test_session_reject_pending_clears() {
    let mut s = ArtifactSession::from_lines(&["a"]);
    s.set_pending(Some(ProposedChange {
        id: "pc-1".to_string(),
        line_start: 0,
        original: vec!["a".to_string()],
        proposed: vec!["X".to_string()],
        rationale: "replace".to_string(),
        verdict: Verdict::Pending,
    }));
    s.reject_pending();
    assert!(s.pending_proposal().is_none());
    assert_eq!(s.applied_count(), 0);
}

#[test]
fn test_session_approve_pending_none() {
    let mut s = ArtifactSession::from_lines(&["a"]);
    assert!(s.approve_pending().is_none());
    assert_eq!(s.applied_count(), 0);
}

#[test]
fn test_session_current_lines_replays() {
    let mut s = ArtifactSession::from_lines(&["a", "b", "c"]);
    s.set_pending(Some(ProposedChange {
        id: "pc-1".to_string(),
        line_start: 1,
        original: vec!["b".to_string()],
        proposed: vec!["B".to_string()],
        rationale: "replace".to_string(),
        verdict: Verdict::Pending,
    }));
    s.approve_pending();
    assert_eq!(s.current_lines(), vec!["a", "B", "c"]);
}

#[test]
fn test_load_missing_path_err() {
    let result = ArtifactSession::load("/no/such/path/here.rs");
    assert!(matches!(result, Err(TuiError::Io)));
}

#[test]
fn test_set_pending_replace_builds() {
    let mut s = ArtifactSession::from_lines(&["a", "b"]);
    s.focus_down();
    s.set_pending_replace("BETA".to_string());
    let p = s.pending_proposal().expect("replace builds a proposal");
    assert_eq!(p.line_start, 1);
    assert_eq!(p.original, vec!["b".to_string()]);
    assert_eq!(p.proposed, vec!["BETA".to_string()]);
}

#[test]
fn test_set_pending_insert_builds() {
    let mut s = ArtifactSession::from_lines(&["a", "b"]);
    s.focus_down();
    s.set_pending_insert("INS".to_string());
    let p = s.pending_proposal().expect("insert builds a proposal");
    assert_eq!(p.line_start, 2);
    assert!(p.original.is_empty());
    assert_eq!(p.proposed, vec!["INS".to_string()]);
}

#[test]
fn test_set_pending_delete_builds() {
    let mut s = ArtifactSession::from_lines(&["a", "b", "c"]);
    s.focus_down();
    s.set_pending_delete();
    let p = s.pending_proposal().expect("delete builds a proposal");
    assert_eq!(p.line_start, 1);
    assert_eq!(p.original, vec!["b".to_string()]);
    assert!(p.proposed.is_empty());
}

#[test]
fn test_mode_enter_and_cancel() {
    let mut s = ArtifactSession::from_lines(&["a"]);
    assert!(s.mode().is_normal());
    s.enter_replace();
    assert_eq!(s.mode(), ArtifactMode::Replace);
    s.cancel_mode();
    assert!(s.mode().is_normal());
    s.enter_insert();
    assert_eq!(s.mode(), ArtifactMode::Insert);
    s.enter_nl();
    assert_eq!(s.mode(), ArtifactMode::NaturalLanguage);
    s.cancel_mode();
    assert!(s.mode().is_normal());
}

#[test]
fn test_set_pending_replace_empty() {
    let mut s = ArtifactSession::from_lines(&[]);
    s.set_pending_replace("x".to_string());
    assert!(s.pending_proposal().is_none());
}

#[test]
fn test_session_insert_applied_grows() {
    let mut s = ArtifactSession::from_lines(&["a", "b"]);
    s.set_pending(Some(ProposedChange {
        id: "pc-1".to_string(),
        line_start: 1,
        original: Vec::new(),
        proposed: vec!["X".to_string()],
        rationale: "insert".to_string(),
        verdict: Verdict::Pending,
    }));
    s.approve_pending();
    assert_eq!(s.current_lines(), vec!["a", "X", "b"]);
    assert_eq!(s.line_count(), 3);
}

#[test]
fn test_session_delete_applied_shrinks() {
    let mut s = ArtifactSession::from_lines(&["a", "b", "c"]);
    s.set_pending(Some(ProposedChange {
        id: "pc-1".to_string(),
        line_start: 1,
        original: vec!["b".to_string()],
        proposed: Vec::new(),
        rationale: "delete".to_string(),
        verdict: Verdict::Pending,
    }));
    s.approve_pending();
    assert_eq!(s.current_lines(), vec!["a", "c"]);
    assert_eq!(s.line_count(), 2);
}

#[test]
fn test_session_save_round_trip() {
    let dir = std::env::temp_dir();
    let path = dir.join("houyicoder_artifact_test.md");
    let mut s = ArtifactSession::from_lines(&["a", "b", "c"]);
    s.set_pending(Some(ProposedChange {
        id: "pc-1".to_string(),
        line_start: 0,
        original: vec!["a".to_string()],
        proposed: vec!["Z".to_string()],
        rationale: "replace".to_string(),
        verdict: Verdict::Pending,
    }));
    s.approve_pending();
    s.save(path.to_str().unwrap()).expect("save writes");
    let reloaded = ArtifactSession::load(path.to_str().unwrap()).expect("load reads");
    assert_eq!(reloaded.current_lines(), vec!["Z", "b", "c"]);
    drop(std::fs::remove_file(path));
}

#[test]
fn test_session_focus_page_clamps() {
    let mut s = ArtifactSession::from_lines(&["a", "b", "c", "d"]);
    s.focus_page_down(10);
    assert_eq!(s.focus(), 3); // clamps at last line, no wrap
    s.focus_page_up(10);
    assert_eq!(s.focus(), 0); // clamps at first line
}

#[test]
fn test_focus_wraps_at_end() {
    // Insert grows the line count; single-line focus wrap must use the new
    // count, not the stale base count.
    let mut s = ArtifactSession::from_lines(&["a", "b"]);
    s.set_pending(Some(ProposedChange {
        id: "pc-1".to_string(),
        line_start: 1,
        original: Vec::new(),
        proposed: vec!["X".to_string()],
        rationale: "insert".to_string(),
        verdict: Verdict::Pending,
    }));
    s.approve_pending();
    s.focus_down();
    s.focus_down();
    s.focus_down();
    assert_eq!(s.focus(), 0); // wraps over 3 lines, not 2
}

#[test]
fn test_applied_marks_flag_replaced() {
    let mut s = ArtifactSession::from_lines(&["a", "b", "c"]);
    s.set_pending(Some(ProposedChange {
        id: "pc-1".to_string(),
        line_start: 1,
        original: vec!["b".to_string()],
        proposed: vec!["B".to_string()],
        rationale: "replace".to_string(),
        verdict: Verdict::Pending,
    }));
    s.approve_pending();
    assert_eq!(s.current_lines(), vec!["a", "B", "c"]);
    assert_eq!(s.applied_marks(), vec![false, true, false]);
}

#[test]
fn test_insert_grows_marks() {
    let mut s = ArtifactSession::from_lines(&["a", "b"]);
    // insert below line 0 -> marks grow to 3, new line marked true
    s.set_pending(Some(ProposedChange {
        id: "pc-1".to_string(),
        line_start: 1,
        original: Vec::new(),
        proposed: vec!["X".to_string()],
        rationale: "insert".to_string(),
        verdict: Verdict::Pending,
    }));
    s.approve_pending();
    assert_eq!(s.current_lines(), vec!["a", "X", "b"]);
    assert_eq!(s.applied_marks(), vec![false, true, false]);
    // now delete line 2 (b) -> marks shrink to 2, deleted mark gone
    s.set_pending(Some(ProposedChange {
        id: "pc-2".to_string(),
        line_start: 2,
        original: vec!["b".to_string()],
        proposed: Vec::new(),
        rationale: "delete".to_string(),
        verdict: Verdict::Pending,
    }));
    s.approve_pending();
    assert_eq!(s.current_lines(), vec!["a", "X"]);
    assert_eq!(s.applied_marks(), vec![false, true]);
}
