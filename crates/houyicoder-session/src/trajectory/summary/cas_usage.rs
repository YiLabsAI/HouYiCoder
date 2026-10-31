//! What the session's stored blocks cost and how its assemblies carried them.
//!
//! Every figure folds from a durable record: a tool result names what the
//! capture kept, an assembly record names what the request carried, and a read
//! record names what left the store. A record the log does not carry leaves its
//! figure absent or unknown rather than zero.

use std::collections::HashSet;

use houyicoder_api::session::{CasAssemblySummary, CasSizeUnit, CasUsageSummary};
use houyicoder_context::{CachedModelView, CachedRead, ReadActor, ReadOutcome, ViewForm};
use houyicoder_protocol::block_ref::{BLOCK_REF_KEY, block_sizes};
use houyicoder_protocol::capture_record::{
    CAPTURE_FAILED_KEY, CAPTURE_KEY, RECORD_KEY, STDERR_BYTES_KEY, STDOUT_BYTES_KEY,
    STOP_REASON_KEY,
};
use serde_json::Value;

/// The session's block-store account, folded one record at a time.
#[derive(Default)]
pub(super) struct CasUsageFold {
    result_count: usize,
    blocks: HashSet<String>,
    raw_bytes: Option<u64>,
    unmeasured_results: usize,
    save_failed: usize,
    truncated: usize,
    assembly: Option<CasAssemblySummary>,
    model_reads: usize,
    ui_reads: Option<usize>,
    failed_reads: usize,
    unknown_reads: usize,
    reads_recorded: bool,
    tracked: bool,
}

impl CasUsageFold {
    /// Note whether the build that wrote this record reports the block store
    /// at all. A false record leaves the model view unknown instead of letting
    /// a reader infer that nothing was stored.
    pub(super) fn observe_writer(&mut self, records_block_store: bool) {
        self.tracked |= records_block_store;
    }

    /// Fold one tool result: what its capture kept, and which blocks it
    /// addresses. A result with no capture still contributes its blocks,
    /// because a log written before captures existed names stored content
    /// without saying how much it cost.
    pub(super) fn fold_tool_result(&mut self, output: &Value) {
        for (address, _) in block_sizes(output) {
            self.blocks.insert(address.to_string());
        }
        let Some(capture) = output.get(CAPTURE_KEY) else {
            return;
        };
        // A capture record exists only in a log whose writer observed the
        // store, so it stands as evidence when the turn's usage record is lost.
        self.tracked = true;
        if capture.get(STOP_REASON_KEY).is_some() {
            self.truncated = self.truncated.saturating_add(1);
        }
        if capture.get(CAPTURE_FAILED_KEY).is_some() {
            self.save_failed = self.save_failed.saturating_add(1);
            return;
        }
        // The record block is what proves the capture published, and the
        // block address inside it is the part the index fold reads. A capture
        // of another shape counts as neither stored nor failed, so a shape
        // this build does not know cannot inflate the stored count.
        let published = capture
            .get(RECORD_KEY)
            .and_then(|record| record.get(BLOCK_REF_KEY))
            .and_then(Value::as_str)
            .is_some();
        if !published {
            return;
        }
        self.result_count = self.result_count.saturating_add(1);
        let stdout = capture.get(STDOUT_BYTES_KEY).and_then(Value::as_u64);
        let stderr = capture.get(STDERR_BYTES_KEY).and_then(Value::as_u64);
        match (stdout, stderr) {
            // A capture that reports neither size measured neither stream, so
            // the result joins the floor rather than reporting no bytes.
            (None, None) => self.unmeasured_results = self.unmeasured_results.saturating_add(1),
            (stdout, stderr) => {
                let measured = stdout.unwrap_or(0).saturating_add(stderr.unwrap_or(0));
                self.raw_bytes = Some(self.raw_bytes.unwrap_or(0).saturating_add(measured));
            }
        }
    }

    /// Fold one assembly record. The newest assembly replaces the earlier one:
    /// the view a reader wants is the request the session last sent, and
    /// adding assemblies together would describe a request that never existed.
    pub(super) fn fold_model_view(
        &mut self,
        assembly_id: u64,
        views: &[CachedModelView],
        estimate_unit: &str,
    ) {
        // An assembly record exists only in a log whose writer observed the
        // store, so it is evidence even when the turn's usage record is lost.
        self.tracked = true;
        let mut summary = CasAssemblySummary {
            assembly_id,
            unit: size_unit(estimate_unit),
            ..CasAssemblySummary::default()
        };
        for view in views {
            summary.size = summary.size.saturating_add(view.size);
            match view.form {
                ViewForm::Materialize => summary.full = summary.full.saturating_add(1),
                ViewForm::Summarize => summary.preview = summary.preview.saturating_add(1),
                ViewForm::Evict => summary.pointer = summary.pointer.saturating_add(1),
                ViewForm::Unavailable => {
                    summary.unavailable = summary.unavailable.saturating_add(1)
                }
                ViewForm::Unknown => {
                    summary.unclassified = summary.unclassified.saturating_add(1);
                }
            }
        }
        self.assembly = Some(summary);
    }

    /// Fold one read record. A read of unknown actor counts as unknown rather
    /// than being attributed to either reader.
    pub(super) fn fold_reads(&mut self, reads: &[CachedRead]) {
        // A read record exists only in a log whose writer observed the store,
        // so it is evidence even when the turn's usage record is lost.
        self.tracked = true;
        self.reads_recorded = true;
        for read in reads {
            let recorded = match read.outcome {
                ReadOutcome::Unknown => {
                    self.unknown_reads = self.unknown_reads.saturating_add(1);
                    continue;
                }
                // A read that delivered nothing is counted apart from the
                // reads that did, so a failing reader is visible as one.
                ReadOutcome::Failed => {
                    self.failed_reads = self.failed_reads.saturating_add(1);
                    continue;
                }
                ReadOutcome::Read => read.actor,
            };
            match recorded {
                ReadActor::Model => self.model_reads = self.model_reads.saturating_add(1),
                ReadActor::Ui => {
                    let count = self.ui_reads.get_or_insert(0);
                    *count = count.saturating_add(1);
                }
                ReadActor::Unknown => {
                    self.unknown_reads = self.unknown_reads.saturating_add(1);
                }
            }
        }
    }

    pub(super) fn snapshot(&self) -> CasUsageSummary {
        // A build that records the block store knows its result count even
        // when it is zero. An older log states a count only through a capture
        // it actually wrote, so a zero there is unknown, not empty.
        let results = (self.tracked || self.result_count > 0).then_some(self.result_count);
        CasUsageSummary {
            results,
            blocks: self.blocks.len(),
            raw_bytes: self.raw_bytes,
            unmeasured_results: results.map(|_| self.unmeasured_results),
            save_failed: results.map(|_| self.save_failed),
            truncated: results.map(|_| self.truncated),
            assembly: self.assembly,
            model_reads: self.model_reads,
            ui_reads: self.ui_reads,
            failed_reads: self.failed_reads,
            unknown_reads: self.unknown_reads,
            reads_recorded: self.reads_recorded,
            tracked: self.tracked,
        }
    }
}

/// The unit an assembly record names, or unknown when this build does not know
/// it, so a reader never prints bytes as tokens.
fn size_unit(label: &str) -> CasSizeUnit {
    match label {
        "tokens" => CasSizeUnit::Tokens,
        "bytes" => CasSizeUnit::Bytes,
        _ => CasSizeUnit::Unknown,
    }
}

#[cfg(test)]
mod tests;
