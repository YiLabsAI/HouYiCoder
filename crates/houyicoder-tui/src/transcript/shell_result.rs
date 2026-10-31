//! What a user shell command's result reports beyond its text.
//!
//! Two facts reach the command row from the durable result rather than from
//! the bounded output text: what the host kept of the raw output, and the
//! block that replaced an oversized output in the model view. The row states
//! both, because a reader who sees only the bounded text cannot tell a
//! complete answer from a cut one.

use houyicoder_protocol::block_ref::block_sizes;
use houyicoder_protocol::capture_record::read_capture_reference;
use houyicoder_protocol::frontend::session_update::ToolCall;
use serde_json::Value;

use crate::records::{ToolOutcome, TranscriptLine};
use crate::row_id::RowId;
use crate::view::byte_size::format_byte_size;

use super::call_rows::tool_result_line;
use super::tool_updates::PendingUpdate;
use super::{LineBuilder, TranscriptFrame};

/// What the host kept of a command's raw output beyond the bounded text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellStorage {
    /// The raw output is in the block store.
    Saved,
    /// The bounded text survives, but the raw output above it was not kept.
    SaveFailed,
    /// The capture stopped the command, so the kept output is only its start.
    Truncated,
}

/// The storage state a result names, or None when it names no capture: an
/// output the model view carried whole has nothing kept and nothing lost.
///
/// A command that was stopped and whose stored copy also failed to land
/// reports the failed copy. The row names one storage fact per command, and
/// the missing record is the one the reader cannot otherwise learn: the
/// bounded text still shows where the capture stopped.
pub(crate) fn storage_state(output: &Value) -> Option<ShellStorage> {
    let capture = read_capture_reference(output)?;
    if capture.failed {
        return Some(ShellStorage::SaveFailed);
    }
    if capture.stop.is_some() {
        return Some(ShellStorage::Truncated);
    }
    capture.record.map(|_| ShellStorage::Saved)
}

/// The facts a shell result states beyond its text, gathered so the row reads
/// them from one place.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct ShellFacts {
    pub exit_code: Option<i64>,
    pub timed_out: bool,
    pub storage: Option<ShellStorage>,
    pub cas: Option<String>,
}

/// Read the facts a shell result states, or the defaults when the call has no
/// result yet. A timed-out call reports the flag and no exit code, so the row
/// reads the flag rather than inferring a reason from a missing code.
pub(crate) fn shell_facts(output: &Value) -> ShellFacts {
    ShellFacts {
        exit_code: output.get("exit_code").and_then(Value::as_i64),
        timed_out: output
            .get("timed_out")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        storage: storage_state(output),
        cas: cas_tag(output),
    }
}

/// The tag for a command row whose output carries a block marker: the first
/// eight characters of the block hash and the stored size. None when the
/// output holds no marker, or a marker that names no stored size, so the row
/// stays silent rather than guess a size it does not have.
pub(crate) fn cas_tag(output: &Value) -> Option<String> {
    // The walk is the protocol crate's, shared with the reclaim pass and the
    // retention pre-check, so every reader agrees on where a marker sits and
    // what key names its size.
    let (hash, bytes) = block_sizes(output)
        .into_iter()
        .find_map(|(hash, bytes)| bytes.map(|bytes| (hash, bytes)))?;
    let short: String = hash.chars().take(8).collect();
    Some(format!("CAS {short} ({})", format_byte_size(bytes)))
}

impl<'a, F: AsRef<TranscriptFrame>> LineBuilder<'a, F> {
    /// Emit a user-typed command as its own dollar-prompt row, then the result
    /// row the tool result produces. The call stays out of the turn's tool
    /// summary: it is the user's action, and a summary of what the agent ran
    /// would misplace it.
    pub(super) fn emit_shell_call(
        &mut self,
        tc: &ToolCall,
        id: &RowId,
        upd: Option<PendingUpdate>,
    ) {
        let call_id = tc.tool_call_id.0.clone();
        let command = tc
            .raw_input
            .as_ref()
            .and_then(|input| input.get("command"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let (outcome, output, duration_ms) = match &upd {
            Some(update) => (
                update.outcome.unwrap_or(ToolOutcome::Running),
                update.output.as_ref(),
                update.duration_ms,
            ),
            None => (ToolOutcome::Running, None, None),
        };
        let facts = output.map(shell_facts).unwrap_or_default();
        self.out.push(TranscriptLine::ShellCommand {
            command,
            outcome,
            duration_ms,
            exit_code: facts.exit_code,
            timed_out: facts.timed_out,
            storage: facts.storage,
            cas: facts.cas,
        });
        self.ids.push(id.clone());
        if let Some(output) = output {
            self.out.push(tool_result_line(
                &call_id,
                &tc.title,
                output,
                tc.raw_input.as_ref(),
            ));
            self.ids.push(id.clone());
        }
    }
}

#[cfg(test)]
mod tests;
