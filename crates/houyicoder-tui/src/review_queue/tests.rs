use super::*;

fn finding(id: &str) -> ReviewFinding {
    ReviewFinding {
        id: id.to_string(),
        lens: "correctness".to_string(),
        verdict: "real".to_string(),
        severity: "high".to_string(),
        hunk_id: "h".to_string(),
        spec_clause_id: "c".to_string(),
        test_id: "t".to_string(),
        adversarial: String::new(),
        note: String::new(),
        signoff: Verdict::Pending,
    }
}

#[test]
fn test_signoff_appends_audit() {
    let mut q = ReviewQueue {
        findings: vec![finding("f1")],
        focus: 0,
        audit_trail: vec![],
    };
    q.signoff_focused("you", "now");
    assert_eq!(q.findings[0].signoff, Verdict::Approved);
    assert_eq!(q.audit_trail.len(), 1);
}

#[test]
fn test_reject_writes_org_eval() {
    let mut q = ReviewQueue {
        findings: vec![finding("f1")],
        focus: 0,
        audit_trail: vec![],
    };
    let note = q.reject_focused("you", "now");
    assert!(note.is_some());
    assert_eq!(q.findings[0].signoff, Verdict::Rejected);
    assert_eq!(q.audit_trail.len(), 1);
}

#[test]
fn test_signoff_noop_when_empty() {
    let mut q = ReviewQueue::default();
    q.signoff_focused("you", "now");
    assert!(q.audit_trail.is_empty());
}
