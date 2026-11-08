//! What the log and the turn's block-store events say about a stored result.
//!
//! The fold holds the durable log and nothing else: a capture's raw bytes are
//! a block in the store, which this reader never opens. So the facts it
//! gathers are the shape of the capture — its blocks, its kept byte counts,
//! what stopped it — and the model views and reads the turn itself recorded.

use std::fmt::Write as _;
use std::mem::take;

use houyicoder_context::{
    CachedModelView, CachedRead, ReadActor as LogReadActor, ReadOutcome as LogReadOutcome, ViewForm,
};
use houyicoder_protocol::block_ref::block_sizes;
use houyicoder_protocol::capture_record::read_capture_reference;
use houyicoder_tui::view::byte_size::format_byte_size;
use houyicoder_tui::view::trajectory_pane::{
    BlockForm, ReadActor, ReadOutcome, StorageFact, StorageRead, StoredBlock, TrajectoryRecordKind,
    short_address,
};
use serde_json::Value;

use super::TurnBuilder;

/// The newest assembly that carried one stored block: the form it took, which
/// assembly it was, and when that request was assembled.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct AssemblyFacts {
    pub form: BlockForm,
    pub assembly_id: u64,
    pub at_ms: u64,
}

/// One durable result's storage, and the block each of its streams lives in
/// so a later read of that stream can name the block it opened.
pub(super) struct StoredResult {
    pub fact: StorageFact,
    /// Result name to block address, in the names the catalog issues.
    pub streams: Vec<(String, String)>,
}

/// Read what one tool result says about the block store, or None when it says
/// nothing: an output the model view held whole kept nothing and lost nothing.
pub(super) fn of_result(call_id: &str, output: &Value) -> Option<StoredResult> {
    let capture = read_capture_reference(output);
    // Both stream lengths come from the same capture, so a total needs both.
    // One of them alone is a floor, and a floor printed as the total would
    // understate what was kept.
    let raw_bytes = capture.as_ref().and_then(|capture| {
        let out = capture.stdout_bytes?;
        let err = capture.stderr_bytes?;
        Some(out + err)
    });
    let block = block_sizes(output)
        .into_iter()
        .find_map(|(hash, bytes)| bytes.map(|bytes| (hash, bytes)))
        .map(|(hash, bytes)| StoredBlock {
            hash: hash.to_owned(),
            bytes,
        });
    let fact = StorageFact {
        raw_bytes,
        stopped: capture
            .as_ref()
            .and_then(|capture| capture.stop)
            .map(|stop| stop.to_string()),
        save_failed: capture.as_ref().is_some_and(|capture| capture.failed),
        block,
        ..StorageFact::default()
    };
    if fact.is_empty() {
        return None;
    }
    let mut streams = Vec::new();
    if let Some(capture) = capture {
        for (name, hash) in [("stdout", capture.stdout), ("stderr", capture.stderr)] {
            if let Some(hash) = hash {
                streams.push((format!("{call_id}:{name}"), hash));
            }
        }
    }
    Some(StoredResult { fact, streams })
}

/// The form one assembly record states for one stored block.
pub(super) fn form_of(view: &CachedModelView) -> BlockForm {
    match view.form {
        ViewForm::Materialize => BlockForm::Materialize,
        ViewForm::Summarize => BlockForm::Summarize,
        ViewForm::Evict => BlockForm::Evict,
        ViewForm::Unavailable => BlockForm::Unavailable,
        ViewForm::Unknown => BlockForm::Unknown,
    }
}

/// Fold one read record into the facts a row states.
pub(super) fn read_of(read: &CachedRead) -> StorageRead {
    StorageRead {
        result_id: read.result_id.clone(),
        block: None,
        actor: match read.actor {
            LogReadActor::Model => ReadActor::Model,
            LogReadActor::Ui => ReadActor::Ui,
            LogReadActor::Unknown => ReadActor::Unknown,
        },
        outcome: match read.outcome {
            LogReadOutcome::Read => ReadOutcome::Read,
            LogReadOutcome::Failed => ReadOutcome::Failed,
            LogReadOutcome::Unknown => ReadOutcome::Unknown,
        },
        start: read.start,
        bytes: read.bytes,
    }
}

/// The row one read states: the block it opened when the log names one, else
/// the result it addressed, then what it delivered. A failed read names no
/// bytes, because it delivered none.
pub(super) fn read_summary(read: &StorageRead) -> String {
    let target = match &read.block {
        Some(hash) => format!("CAS {}", short_address(hash)),
        None => read.result_id.clone(),
    };
    let mut summary = String::new();
    match read.outcome {
        ReadOutcome::Failed => {
            let _ = write!(summary, "read {target} failed");
        }
        ReadOutcome::Unknown => {
            let _ = write!(summary, "read {target} returned an unknown outcome");
        }
        ReadOutcome::Read if read.start > 0 => {
            let _ = write!(
                summary,
                "read {target} from {} ({})",
                read.start,
                format_byte_size(read.bytes)
            );
        }
        ReadOutcome::Read => {
            let _ = write!(summary, "read {target} ({})", format_byte_size(read.bytes));
        }
    }
    summary
}

impl TurnBuilder {
    /// Record the form the newest assembly carried for each block it names.
    pub(super) fn apply_model_view(
        &mut self,
        assembly_id: u64,
        views: &[CachedModelView],
        ts: u64,
    ) {
        for view in views {
            self.block_forms.insert(
                view.block_ref.clone(),
                AssemblyFacts {
                    form: form_of(view),
                    assembly_id,
                    at_ms: ts,
                },
            );
        }
    }

    /// Append one row per read the log recorded, with the facts that row
    /// states. A read lands before the result that answers it, so the row's
    /// text is completed when the turn closes rather than at its position in
    /// the log.
    pub(super) fn push_reads(&mut self, reads: &[CachedRead], ts: u64) {
        for read in reads {
            self.touch(ts);
            let fact = read_of(read);
            let mut record = self.record(
                TrajectoryRecordKind::Storage,
                Some(fact.actor.label().to_string()),
                read_summary(&fact),
                ts,
            );
            record.outcome = fact.outcome.outcome();
            record.storage = Some(StorageFact {
                reads: vec![fact.clone()],
                ..StorageFact::default()
            });
            let index = self.push(record);
            self.stored_reads.push((index, fact));
        }
    }

    /// Complete the storage rows now that every event of the turn is folded:
    /// the form an assembly took for a block, the block a read opened, and the
    /// reads of each stored result.
    ///
    /// The three joins cannot run where the events arrive. An assembly lands
    /// after the result that produced its block, a read lands before the
    /// result that answers it, and a read of a result this turn never stored
    /// stays unknown rather than being counted as a miss.
    pub(super) fn resolve_storage(&mut self) {
        let stored_results = take(&mut self.stored_results);
        let mut stored_reads = take(&mut self.stored_reads);
        for (index, call_id) in &stored_results {
            let Some(record) = self.records.get_mut(*index) else {
                tracing::warn!(
                    index,
                    records_len = self.records.len(),
                    "trajectory storage result record index out of bounds"
                );
                continue;
            };
            let Some(fact) = record.storage.as_mut() else {
                continue;
            };
            let address = format!("{call_id}:");
            fact.reads = stored_reads
                .iter()
                .filter(|(_, read)| read.result_id.starts_with(&address))
                .map(|(_, read)| read.clone())
                .collect();
            if let Some(facts) = fact
                .block
                .as_ref()
                .and_then(|block| self.block_forms.get(&block.hash))
            {
                fact.form = Some(facts.form);
                fact.assembly_id = Some(facts.assembly_id);
                fact.assembly_at_ms = Some(facts.at_ms);
            }
        }
        for (index, read) in &mut stored_reads {
            if let Some(hash) = self.stream_blocks.get(&read.result_id) {
                read.block = Some(hash.clone());
            }
            let Some(record) = self.records.get_mut(*index) else {
                tracing::warn!(
                    index,
                    records_len = self.records.len(),
                    "trajectory storage read record index out of bounds"
                );
                continue;
            };
            record.summary = read_summary(read);
            if let Some(fact) = record.storage.as_mut() {
                fact.reads = vec![read.clone()];
            }
        }
    }
}
