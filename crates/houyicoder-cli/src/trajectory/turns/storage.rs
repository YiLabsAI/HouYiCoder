//! What the log and the turn's block-store events say about a stored result.
//!
//! The fold holds the durable log and nothing else: a capture's raw bytes are
//! a block in the store, which this reader never opens. So the facts it
//! gathers are the shape of the capture — its blocks, its kept byte counts,
//! what stopped it — and the model views and reads the turn itself recorded.

use std::fmt::Write as _;

use houyicoder_context::{
    CachedModelView, CachedRead, ReadActor as LogReadActor, ReadOutcome as LogReadOutcome, ViewForm,
};
use houyicoder_protocol::block_ref::block_sizes;
use houyicoder_protocol::capture_record::read_capture_reference;
use houyicoder_tui::view::byte_size::format_byte_size;
use houyicoder_tui::view::trajectory_pane::{
    BlockForm, ReadActor, ReadOutcome, StorageFact, StorageRead, StoredBlock, short_address,
};
use serde_json::Value;

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
