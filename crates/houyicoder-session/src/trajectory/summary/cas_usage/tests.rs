//! What the stored-block account reports from the records a session wrote:
//! the figures it can state, the ones it must leave unknown, and the replay
//! that must not count an assembly twice.

use super::*;

use serde_json::json;

/// A tool result whose capture published its record and reported stream sizes.
fn captured_result(stdout: u64, stderr: u64) -> Value {
    json!({
        CAPTURE_KEY: {
            RECORD_KEY: { "block_ref": "record-block" },
            STDOUT_BYTES_KEY: stdout,
            STDERR_BYTES_KEY: stderr,
        }
    })
}

/// A tool result whose capture refused to publish, after stopping the command.
fn failed_capture() -> Value {
    json!({
        CAPTURE_KEY: {
            CAPTURE_FAILED_KEY: "the raw output above the bound was not kept",
            STOP_REASON_KEY: "byte_budget",
        }
    })
}

fn view(block_ref: &str, form: ViewForm, size: u64) -> CachedModelView {
    CachedModelView {
        block_ref: block_ref.to_string(),
        form,
        size,
    }
}

#[test]
fn test_fold_counts_published_capture() {
    let mut fold = CasUsageFold::default();
    fold.observe_writer(true);
    fold.fold_tool_result(&captured_result(100, 20));
    let summary = fold.snapshot();
    assert_eq!(summary.results, Some(1), "one capture published");
    assert_eq!(summary.raw_bytes, Some(120), "both streams counted");
    assert_eq!(summary.unmeasured_results, Some(0));
    assert_eq!(summary.save_failed, Some(0));
    assert!(summary.tracked);
}

#[test]
fn test_failed_capture_not_stored() {
    let mut fold = CasUsageFold::default();
    fold.observe_writer(true);
    fold.fold_tool_result(&failed_capture());
    let summary = fold.snapshot();
    assert_eq!(summary.results, Some(0), "a refused capture stored nothing");
    assert_eq!(summary.save_failed, Some(1), "and is reported as a failure");
    assert_eq!(summary.truncated, Some(1), "the command was stopped");
    assert_eq!(summary.raw_bytes, None, "no capture reported a size");
}

/// A capture of a shape this build does not know counts as neither stored nor
/// failed: only the record block proves a capture published, so an unknown
/// shape cannot inflate the stored count.
#[test]
fn test_unrecorded_capture_not_stored() {
    let mut fold = CasUsageFold::default();
    fold.observe_writer(true);
    fold.fold_tool_result(&json!({
        CAPTURE_KEY: { STOP_REASON_KEY: "byte_budget" }
    }));
    let summary = fold.snapshot();
    assert_eq!(summary.results, Some(0), "no record block, nothing stored");
    assert_eq!(summary.truncated, Some(1), "the stop is still reported");
    assert_eq!(summary.save_failed, Some(0));
}

/// A record block without a block address proves no publish: the address is
/// the part a later read resolves, so a capture missing it cannot count as
/// stored.
#[test]
fn test_record_without_address() {
    let mut fold = CasUsageFold::default();
    fold.observe_writer(true);
    fold.fold_tool_result(&json!({
        CAPTURE_KEY: { RECORD_KEY: {} }
    }));
    let summary = fold.snapshot();
    assert_eq!(summary.results, Some(0), "no address, nothing to read");
    assert_eq!(summary.save_failed, Some(0), "and no failure was reported");
}

/// A capture record alone proves its writer observed the store, so a log whose
/// usage record was lost still reports a measured count rather than unknown.
#[test]
fn test_capture_record_marks_writer() {
    let mut fold = CasUsageFold::default();
    fold.fold_tool_result(&captured_result(100, 20));
    let summary = fold.snapshot();
    assert!(summary.tracked, "the capture record speaks for its writer");
    assert_eq!(summary.results, Some(1));
}

/// A log written before the capture record existed names stored blocks but
/// not how many results they came from, so the count is unknown rather than
/// zero.
#[test]
fn test_without_writer_results_unknown() {
    let mut fold = CasUsageFold::default();
    fold.fold_tool_result(&json!({ "block_ref": "old-block" }));
    let summary = fold.snapshot();
    assert_eq!(summary.results, None, "an old log states no count");
    assert_eq!(summary.blocks, 1, "the marker is evidence of one block");
    assert_eq!(summary.unmeasured_results, None, "no count to qualify");
    assert_eq!(summary.save_failed, None, "no count to qualify");
    assert!(!summary.tracked);
}

/// A capture that published but recorded no size leaves the byte total absent
/// and the result counted as unmeasured, so the total reads as a floor.
#[test]
fn test_unmeasured_capture_total_absent() {
    let mut fold = CasUsageFold::default();
    fold.fold_tool_result(&json!({
        CAPTURE_KEY: { RECORD_KEY: { "block_ref": "record-block" } }
    }));
    let summary = fold.snapshot();
    assert_eq!(summary.results, Some(1));
    assert_eq!(summary.raw_bytes, None, "no size was measured");
    assert_eq!(summary.unmeasured_results, Some(1));
}

#[test]
fn test_newest_assembly_reported() {
    let mut fold = CasUsageFold::default();
    fold.fold_model_view(
        1,
        &[
            view("a", ViewForm::Materialize, 10),
            view("b", ViewForm::Summarize, 5),
        ],
        "tokens",
    );
    fold.fold_model_view(
        2,
        &[
            view("a", ViewForm::Materialize, 10),
            view("b", ViewForm::Summarize, 5),
            view("c", ViewForm::Evict, 2),
            view("d", ViewForm::Unavailable, 1),
            view("e", ViewForm::Unknown, 3),
        ],
        "tokens",
    );
    let assembly = fold.snapshot().assembly.expect("an assembly is recorded");
    assert_eq!(assembly.assembly_id, 2, "the newest assembly is reported");
    assert_eq!(assembly.full, 1);
    assert_eq!(assembly.preview, 1);
    assert_eq!(assembly.pointer, 1);
    assert_eq!(assembly.unavailable, 1);
    assert_eq!(assembly.unclassified, 1, "an unknown form is not guessed");
    assert_eq!(assembly.size, 21, "every served form sums into the size");
    assert_eq!(assembly.unit, CasSizeUnit::Tokens);
}

/// The same assembly replayed replaces its record instead of adding to it, so
/// a continuation that reuses its assembly id reports the request it last
/// sent, not a sum of both.
#[test]
fn test_replayed_assembly_replaces() {
    let mut fold = CasUsageFold::default();
    let first = [
        view("a", ViewForm::Materialize, 10),
        view("b", ViewForm::Summarize, 5),
    ];
    fold.fold_model_view(7, &first, "tokens");
    fold.fold_model_view(7, &[view("a", ViewForm::Materialize, 4)], "tokens");
    let assembly = fold.snapshot().assembly.expect("an assembly is recorded");
    assert_eq!(assembly.full, 1, "the later record is the one reported");
    assert_eq!(assembly.preview, 0, "the earlier block is not kept");
    assert_eq!(assembly.size, 4, "replaced, not summed");
}

#[test]
fn test_reads_split_by_actor() {
    let mut fold = CasUsageFold::default();
    fold.fold_reads(&[
        CachedRead {
            result_id: "r1".into(),
            actor: ReadActor::Model,
            start: 0,
            bytes: 10,
            outcome: ReadOutcome::Read,
        },
        CachedRead {
            result_id: "r2".into(),
            actor: ReadActor::Ui,
            start: 8,
            bytes: 4,
            outcome: ReadOutcome::Read,
        },
        CachedRead {
            result_id: "r3".into(),
            actor: ReadActor::Model,
            start: 0,
            bytes: 0,
            outcome: ReadOutcome::Failed,
        },
        CachedRead {
            result_id: "r4".into(),
            actor: ReadActor::Unknown,
            start: 0,
            bytes: 0,
            outcome: ReadOutcome::Read,
        },
    ]);
    let summary = fold.snapshot();
    assert_eq!(summary.model_reads, 1);
    assert_eq!(summary.ui_reads, Some(1));
    assert_eq!(summary.failed_reads, 1);
    assert_eq!(summary.unknown_reads, 1, "an unknown actor is not guessed");
    assert!(summary.reads_recorded);
}

/// A log with no read record reports no record, which a reader tells apart
/// from a session that read nothing.
#[test]
fn test_no_read_record_reported() {
    let fold = CasUsageFold::default();
    let summary = fold.snapshot();
    assert!(!summary.reads_recorded);
    assert_eq!(summary.model_reads, 0);
    assert_eq!(summary.ui_reads, None, "no view read was recorded");
}

/// An assembly record alone proves its writer observed the store, so a lost
/// usage record does not turn a measured zero into an unknown count.
#[test]
fn test_assembly_record_marks_writer() {
    let mut fold = CasUsageFold::default();
    fold.fold_model_view(1, &[view("a", ViewForm::Materialize, 10)], "tokens");
    let summary = fold.snapshot();
    assert!(summary.tracked, "the assembly record speaks for its writer");
    assert_eq!(summary.results, Some(0), "a tracking writer measured zero");
}

/// A unit this build does not know is reported as unknown, so a byte count is
/// never printed as a token count.
#[test]
fn test_fold_unknown_unit_reported() {
    let mut fold = CasUsageFold::default();
    fold.fold_model_view(1, &[view("a", ViewForm::Materialize, 10)], "furlongs");
    let assembly = fold.snapshot().assembly.expect("an assembly is recorded");
    assert_eq!(assembly.unit, CasSizeUnit::Unknown);
}
