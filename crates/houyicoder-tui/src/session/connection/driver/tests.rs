use super::UnknownReportBudget;

#[test]
fn test_unknown_report_budget_spends() {
    // The bound's false branch has no other observable: the driver's
    // only report for a dropped notification is a diagnostic line.
    let mut budget = UnknownReportBudget::new();
    for _ in 0..UnknownReportBudget::LIMIT {
        assert!(budget.grant());
    }
    assert!(!budget.grant());
}
