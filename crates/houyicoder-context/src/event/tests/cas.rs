//! Wire-shape tests for the block-store records: a record this build wrote
//! reads back whole, and a value a newer build wrote reads back as unknown
//! instead of failing the replay.

use super::*;

fn view(block_ref: &str, form: ViewForm, size: u64) -> CachedModelView {
    CachedModelView {
        block_ref: block_ref.to_string(),
        form,
        size,
    }
}

/// An assembly record survives a round trip, so a replayed session states the
/// same forms its request carried.
#[test]
fn test_model_view_round_trips() {
    let event = SessionEvent::CasModelView {
        assembly_id: 7,
        views: vec![
            view("block-a", ViewForm::Materialize, 900),
            view("block-b", ViewForm::Evict, 12),
        ],
        estimate_unit: "tokens".to_string(),
    };
    let json = serde_json::to_string(&event).expect("serialize");
    let back: SessionEvent = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, event);
}

/// The form labels are the log's compatibility contract, so a rename that
/// changes them breaks every old log's count.
#[test]
fn test_form_labels_are_stable() {
    let labels: Vec<String> = [
        ViewForm::Materialize,
        ViewForm::Summarize,
        ViewForm::Evict,
        ViewForm::Unavailable,
    ]
    .into_iter()
    .map(|form| serde_json::to_string(&form).expect("serialize"))
    .collect();
    assert_eq!(
        labels,
        vec![
            "\"materialize\"",
            "\"summarize\"",
            "\"evict\"",
            "\"unavailable\""
        ]
    );
}

/// A form a newer build wrote counts as unknown, so an old build reports it
/// unclassified rather than dropping the block or guessing a form.
#[test]
fn test_unknown_form_reads_unknown() {
    let back: ViewForm = serde_json::from_str("\"compressed\"").expect("deserialize");
    assert_eq!(back, ViewForm::Unknown);
}

/// A read record survives a round trip, including its byte range.
#[test]
fn test_read_round_trips() {
    let event = SessionEvent::CasRead {
        reads: vec![CachedRead {
            result_id: "result-1".to_string(),
            actor: ReadActor::Ui,
            start: 4096,
            bytes: 2048,
            outcome: ReadOutcome::Read,
        }],
    };
    let json = serde_json::to_string(&event).expect("serialize");
    let back: SessionEvent = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, event);
}

/// A reader or outcome a newer build wrote counts as unknown, so a replay
/// never charges another reader for a read it cannot attribute.
#[test]
fn test_unknown_read_labels() {
    let actor: ReadActor = serde_json::from_str("\"plugin\"").expect("deserialize");
    let outcome: ReadOutcome = serde_json::from_str("\"partial\"").expect("deserialize");
    assert_eq!(actor, ReadActor::Unknown);
    assert_eq!(outcome, ReadOutcome::Unknown);
}
