//! What the context view is told about the stored-block account: an absent
//! figure stays absent, and each half comes from the record that owns it.

use super::{map_cas_usage, reports_cas};

use houyicoder_api::session::{CasAssemblySummary, CasSizeUnit, CasUsageSummary};
use houyicoder_protocol::frontend::context::CasSizeUnit as WireSizeUnit;
use houyicoder_protocol::result_catalog::ResultStats;

/// A log that records nothing about the store reports unknown rather than
/// zero, so a reader never prints a measured zero for an unmeasured session.
#[test]
fn test_absent_figures_stay_absent() {
    let usage = map_cas_usage(&CasUsageSummary::default(), None);
    assert_eq!(usage.results, None, "no result count is claimed");
    assert_eq!(usage.raw_bytes, None, "no byte total is claimed");
    assert_eq!(usage.unmeasured_results, None);
    assert_eq!(usage.save_failed, None);
    assert_eq!(usage.truncated, None);
    assert_eq!(usage.assembly, None, "no assembly is claimed");
    assert_eq!(usage.ui_reads, None, "no view read is claimed");
    assert_eq!(usage.indexed, None, "no index is claimed");
    assert!(!usage.tracked);
    assert!(!usage.reads_recorded);
}

/// Each figure the log states reaches the view unchanged, and the assembly
/// keeps its identity beside its per-form counts.
#[test]
fn test_figures_and_assembly_mapped() {
    let summary = CasUsageSummary {
        results: Some(3),
        blocks: 4,
        raw_bytes: Some(170_000),
        unmeasured_results: Some(1),
        save_failed: Some(0),
        truncated: Some(2),
        assembly: Some(CasAssemblySummary {
            assembly_id: 9,
            full: 2,
            preview: 4,
            pointer: 7,
            unavailable: 1,
            unclassified: 0,
            size: 1800,
            unit: CasSizeUnit::Tokens,
        }),
        model_reads: 3,
        ui_reads: Some(1),
        failed_reads: 0,
        unknown_reads: 2,
        reads_recorded: true,
        tracked: true,
    };
    let usage = map_cas_usage(&summary, None);
    assert_eq!(usage.results, Some(3));
    assert_eq!(usage.blocks, 4);
    assert_eq!(usage.raw_bytes, Some(170_000));
    assert_eq!(usage.unmeasured_results, Some(1));
    assert_eq!(usage.truncated, Some(2));
    assert_eq!(usage.model_reads, 3);
    assert_eq!(usage.ui_reads, Some(1));
    assert_eq!(usage.unknown_reads, 2, "an unknown actor is not guessed");
    assert!(usage.reads_recorded);
    assert!(usage.tracked);
    let assembly = usage.assembly.expect("the assembly is mapped");
    assert_eq!(assembly.assembly_id, 9);
    assert_eq!(assembly.full, 2);
    assert_eq!(assembly.preview, 4);
    assert_eq!(assembly.pointer, 7);
    assert_eq!(assembly.unavailable, 1);
    assert_eq!(assembly.size, 1800);
    assert_eq!(assembly.unit, WireSizeUnit::Tokens);
}

/// The index half comes from the host's catalog, so a host that serves
/// stored-result requests reports what its fold covered and whether the
/// figures are whole.
#[test]
fn test_index_half_from_catalog() {
    let stats = ResultStats {
        results: 13,
        corrupt: 1,
        complete: false,
        as_of: Some("event-2010".to_string()),
        ..ResultStats::default()
    };
    let usage = map_cas_usage(&CasUsageSummary::default(), Some(&stats));
    let index = usage.indexed.expect("the catalog half is mapped");
    assert_eq!(index.indexed, 13);
    assert_eq!(index.corrupt, 1);
    assert!(!index.complete, "a partial fold is reported as partial");
    assert_eq!(index.as_of.as_deref(), Some("event-2010"));
}

/// A session that never used the store has nothing to report, so the context
/// view is not handed a row of zeros.
#[test]
fn test_unused_store_omitted() {
    assert!(!reports_cas(&CasUsageSummary::default()));
}

/// A session whose every save failed still reports: the failure is what the
/// account exists to show, so a stored-block count of zero cannot hide it.
#[test]
fn test_failed_saves_report() {
    let summary = CasUsageSummary {
        results: Some(0),
        save_failed: Some(2),
        ..CasUsageSummary::default()
    };
    assert!(reports_cas(&summary));
}

/// A stored result reports even before any assembly or read was recorded.
#[test]
fn test_stored_result_reports() {
    let summary = CasUsageSummary {
        results: Some(1),
        ..CasUsageSummary::default()
    };
    assert!(reports_cas(&summary));
}

/// A log that predates the capture record reports on its markers alone, so
/// the view can say the model view is unknown instead of staying silent.
#[test]
fn test_markers_alone_report() {
    let summary = CasUsageSummary {
        blocks: 3,
        ..CasUsageSummary::default()
    };
    assert!(reports_cas(&summary));
}

/// A command stopped at the output bound reports even when nothing else about
/// the store is known, because the stop is the store's answer.
#[test]
fn test_stopped_capture_reports() {
    let summary = CasUsageSummary {
        results: Some(0),
        truncated: Some(1),
        ..CasUsageSummary::default()
    };
    assert!(reports_cas(&summary));
}

/// An assembly that carried block views reports on those views alone, so a
/// session whose results predate the capture record still shows what its
/// request carried.
#[test]
fn test_assembly_alone_reports() {
    let summary = CasUsageSummary {
        assembly: Some(CasAssemblySummary::default()),
        ..CasUsageSummary::default()
    };
    assert!(reports_cas(&summary));
}

/// A unit this build does not know reaches the view as unknown, so bytes are
/// never printed as tokens.
#[test]
fn test_unknown_unit_mapped() {
    let summary = CasUsageSummary {
        assembly: Some(CasAssemblySummary {
            unit: CasSizeUnit::Unknown,
            ..CasAssemblySummary::default()
        }),
        ..CasUsageSummary::default()
    };
    let assembly = map_cas_usage(&summary, None)
        .assembly
        .expect("the assembly is mapped");
    assert_eq!(assembly.unit, WireSizeUnit::Unknown);
}
