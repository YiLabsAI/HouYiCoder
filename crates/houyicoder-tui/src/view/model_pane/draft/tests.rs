use super::*;

#[test]
fn test_effort_forward_cycles_auto() {
    let levels = vec![EffortLevel::Low, EffortLevel::Medium, EffortLevel::High];
    let mut draft = ModelDraft::default();
    draft.adjust_effort(&levels, true);
    assert_eq!(draft.effort, Some(EffortLevel::Low));
    draft.adjust_effort(&levels, true);
    assert_eq!(draft.effort, Some(EffortLevel::Medium));
    draft.adjust_effort(&levels, true);
    assert_eq!(draft.effort, Some(EffortLevel::High));
    draft.adjust_effort(&levels, true);
    assert_eq!(draft.effort, None);
}

#[test]
fn test_effort_backward_cycles_auto() {
    let levels = vec![EffortLevel::Low, EffortLevel::Medium, EffortLevel::High];
    let mut draft = ModelDraft::default();
    draft.adjust_effort(&levels, false);
    assert_eq!(draft.effort, Some(EffortLevel::High));
    draft.adjust_effort(&levels, false);
    assert_eq!(draft.effort, Some(EffortLevel::Medium));
    draft.adjust_effort(&levels, false);
    assert_eq!(draft.effort, Some(EffortLevel::Low));
    draft.adjust_effort(&levels, false);
    assert_eq!(draft.effort, None);
}

#[test]
fn test_adjust_effort_marks_dirty() {
    let levels = vec![EffortLevel::Low, EffortLevel::High];
    let mut draft = ModelDraft::default();
    assert!(!draft.effort_touched);
    assert!(!draft.dirty);
    draft.adjust_effort(&levels, true);
    assert!(draft.effort_touched);
    assert!(draft.dirty);
}

#[test]
fn test_adjust_effort_unsupported_noop() {
    let mut draft = ModelDraft::default();
    draft.adjust_effort(&[], true);
    assert_eq!(draft.effort, None);
    assert!(!draft.dirty);
}
