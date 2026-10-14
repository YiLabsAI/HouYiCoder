use super::*;

#[test]
fn test_diff_focus_wrap_empty() {
    let mut d = DiffData {
        path: "x".into(),
        hunks: Vec::new(),
        focus: 0,
    };
    d.focus_down();
    assert_eq!(d.focus, 0);
}

#[test]
fn test_diff_approve_reject() {
    let mut d = DiffData {
        path: "x".into(),
        hunks: vec![Hunk {
            id: "h1".into(),
            file: "f".into(),
            range: "1-2".into(),
            patch: "".into(),
            evidence: HunkEvidence {
                spec_clause_id: "c1".into(),
                spec_clause_desc: "".into(),
                finding_id: "".into(),
                finding_desc: "".into(),
                test_id: "".into(),
                why: "".into(),
            },
            approved: Verdict::Pending,
        }],
        focus: 0,
    };
    assert!(d.approve_focused());
    assert_eq!(d.current().unwrap().approved, Verdict::Approved);
    assert!(!d.approve_focused());
    assert!(d.reject_focused());
    assert_eq!(d.current().unwrap().approval_label(), "rejected");
}
