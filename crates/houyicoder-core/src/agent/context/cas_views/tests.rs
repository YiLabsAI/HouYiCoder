//! The record an assembly reports the forms it carried to: one entry per
//! block, drained lowest assembly first.

use super::*;

#[test]
fn test_record_replaces_same_block() {
    let views = CasViewState::new("tokens");
    views.record(4, "hash-a", ViewForm::Materialize, 10);
    views.record(4, "hash-a", ViewForm::Evict, 2);
    let taken = views.take();
    assert_eq!(taken.len(), 1, "one assembly");
    assert_eq!(taken[0].1.len(), 1, "one entry for the block");
    assert_eq!(
        taken[0].1[0].form,
        ViewForm::Evict,
        "the later form is the one kept"
    );
    assert_eq!(taken[0].1[0].size, 2);
}

#[test]
fn test_take_drains_and_orders() {
    let views = CasViewState::new("tokens");
    views.record(9, "hash-b", ViewForm::Summarize, 5);
    views.record(2, "hash-a", ViewForm::Materialize, 10);
    let taken = views.take();
    assert_eq!(taken.len(), 2);
    assert_eq!(taken[0].0, 2, "the lower assembly is written first");
    assert_eq!(taken[1].0, 9);
    assert!(views.take().is_empty(), "the record is left empty");
}

#[test]
fn test_record_keeps_unit() {
    let views = CasViewState::new("bytes");
    assert_eq!(views.estimate_unit(), "bytes");
}
