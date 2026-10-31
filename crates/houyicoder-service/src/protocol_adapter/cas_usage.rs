//! Map the session's stored-block account to the form the context view
//! reads.

use houyicoder_api::session::{CasAssemblySummary, CasSizeUnit, CasUsageSummary};
use houyicoder_protocol::frontend::context::{
    CasAssemblyUsage, CasIndexUsage, CasSizeUnit as WireSizeUnit, CasUsage,
};
use houyicoder_protocol::result_catalog::ResultStats;

/// Whether the session has anything to report about the block store.
///
/// A session that never used the store reports nothing. Every state that says
/// the store was reached and did not behave plainly must report, because a
/// store outcome the user cannot see is exactly what the account exists to
/// show: a save that failed, a command stopped at the output bound, and an
/// assembly that carried block views all report. So does a log whose only
/// evidence is the block markers its tool results carry.
pub(crate) fn reports_cas(summary: &CasUsageSummary) -> bool {
    summary.results.is_some_and(|count| count > 0)
        || summary.blocks > 0
        || summary.save_failed.is_some_and(|count| count > 0)
        || summary.truncated.is_some_and(|count| count > 0)
        || summary.assembly.is_some()
}

/// Map the folded account to the form the context view reads. The index half
/// comes from the host's catalog when it serves stored-result requests; None
/// there means the host reports no index, not an index holding nothing.
pub(crate) fn map_cas_usage(summary: &CasUsageSummary, index: Option<&ResultStats>) -> CasUsage {
    CasUsage {
        results: summary.results.map(|count| count as u64),
        blocks: summary.blocks as u64,
        raw_bytes: summary.raw_bytes,
        unmeasured_results: summary.unmeasured_results.map(|count| count as u64),
        save_failed: summary.save_failed.map(|count| count as u64),
        truncated: summary.truncated.map(|count| count as u64),
        assembly: summary.assembly.as_ref().map(map_assembly),
        model_reads: summary.model_reads as u64,
        ui_reads: summary.ui_reads.map(|count| count as u64),
        failed_reads: summary.failed_reads as u64,
        unknown_reads: summary.unknown_reads as u64,
        reads_recorded: summary.reads_recorded,
        tracked: summary.tracked,
        indexed: index.map(|stats| CasIndexUsage {
            indexed: stats.results as u64,
            complete: stats.complete,
            corrupt: stats.corrupt as u64,
            as_of: stats.as_of.clone(),
        }),
    }
}

fn map_assembly(assembly: &CasAssemblySummary) -> CasAssemblyUsage {
    CasAssemblyUsage {
        assembly_id: assembly.assembly_id,
        full: assembly.full as u64,
        preview: assembly.preview as u64,
        pointer: assembly.pointer as u64,
        unavailable: assembly.unavailable as u64,
        unclassified: assembly.unclassified as u64,
        size: assembly.size,
        unit: match assembly.unit {
            CasSizeUnit::Tokens => WireSizeUnit::Tokens,
            CasSizeUnit::Bytes => WireSizeUnit::Bytes,
            CasSizeUnit::Unknown => WireSizeUnit::Unknown,
        },
    }
}

#[cfg(test)]
mod tests;
