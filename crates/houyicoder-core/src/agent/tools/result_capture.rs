//! Keeps a command's raw output before the tool result is bounded.
//!
//! The sandbox exec feeds every chunk here as it leaves the pipe. Bytes
//! accumulate under a fixed ceiling that ends a runaway command, and finish
//! publishes them as content-addressed blocks the tool names. Publishing runs
//! after the command ended, so a full store is reported as an error: it
//! cannot stop a producer that already finished.

use std::mem::take;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use houyicoder_api::capture::{
    CallEnd, CaptureError, CaptureStop, CaptureVerdict, CapturedStream, CapturedStreams,
    RawOutputReceiver, ResultCapture, ResultCaptureSource,
};
use houyicoder_api::session::SessionLog;
use houyicoder_async::PFut;
use houyicoder_context::{BlockHash, SessionId};
use houyicoder_protocol::capture_record::{
    BlockMarker, CAPTURE_RECORD_VERSION, CaptureCompleteness, CaptureRecord, CaptureStopReason,
    StreamEncoding, StreamRecord,
};

/// The bytes one capture keeps before it stops the command. A command that
/// prints more than this is producing output nobody will read back through the
/// model, and keeping it would let one runaway command consume memory and
/// store without end.
pub(crate) const CAPTURE_BUDGET_BYTES: usize = 8 * 1024 * 1024;

/// Starts a capture for every dispatched call, publishing into the session
/// log's block store.
pub(crate) struct SessionResultCapture {
    log: Arc<dyn SessionLog>,
    budget: usize,
}

impl SessionResultCapture {
    /// Capture into the given log's block store.
    pub(crate) fn new(log: Arc<dyn SessionLog>) -> Self {
        Self {
            log,
            budget: CAPTURE_BUDGET_BYTES,
        }
    }

    /// A source with a ceiling small enough for a test to reach the stop
    /// path without holding the production budget of bytes.
    #[cfg(test)]
    pub(crate) fn with_budget(log: Arc<dyn SessionLog>, budget: usize) -> Self {
        Self { log, budget }
    }
}

impl ResultCaptureSource for SessionResultCapture {
    fn begin(
        &self,
        session: SessionId,
        call_id: &str,
        tool: &str,
    ) -> Option<Arc<dyn ResultCapture>> {
        Some(Arc::new(CallCapture {
            log: self.log.clone(),
            session,
            call_id: call_id.to_string(),
            tool: tool.to_string(),
            created_ms: now_ms(),
            budget: self.budget,
            state: Mutex::new(CaptureState::default()),
        }))
    }
}

/// Everything one call's capture holds. Both streams sit behind one lock so a
/// single budget covers them and the two readers cannot interleave a size
/// decision.
#[derive(Default)]
struct CaptureState {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    stop: Option<CaptureStop>,
}

impl CaptureState {
    fn stream_mut(&mut self, stream: CapturedStream) -> &mut Vec<u8> {
        match stream {
            CapturedStream::Stdout => &mut self.stdout,
            CapturedStream::Stderr => &mut self.stderr,
        }
    }

    fn record_stop(&mut self, reason: CaptureStopReason) -> CaptureStop {
        let stop = CaptureStop {
            reason,
            stdout_bytes: self.stdout.len() as u64,
            stderr_bytes: self.stderr.len() as u64,
        };
        self.stop = Some(stop.clone());
        stop
    }
}

struct CallCapture {
    log: Arc<dyn SessionLog>,
    session: SessionId,
    call_id: String,
    tool: String,
    created_ms: Option<u64>,
    budget: usize,
    state: Mutex<CaptureState>,
}

impl CallCapture {
    /// Publish one stream's bytes as a block. An empty stream publishes
    /// nothing: there is no block to read back.
    async fn publish(&self, bytes: &[u8]) -> Result<Option<BlockHash>, CaptureError> {
        if bytes.is_empty() {
            return Ok(None);
        }
        self.log
            .backend()
            .block_put(bytes.to_vec())
            .await
            .map(Some)
            .map_err(|e| CaptureError::StreamWrite(e.to_string()))
    }

    /// Write the record that binds the stream blocks to the call that
    /// produced them. It is written after the streams, so a record the store
    /// holds always names blocks the store holds too.
    ///
    /// Only a command that exited on its own with a status is recorded as
    /// complete. A stop this capture applied is the reason bytes are missing
    /// ahead of anything the caller reports; with no such stop, a wall-budget
    /// end or an absent status still means the kept bytes are a prefix.
    async fn write_record(
        &self,
        stdout: &Option<BlockHash>,
        stderr: &Option<BlockHash>,
        kept: KeptBytes,
        stop: &Option<CaptureStop>,
        end: CallEnd,
    ) -> Result<(BlockHash, Option<CaptureStopReason>), CaptureError> {
        let exit = match end {
            CallEnd::Exited(code) => code,
            CallEnd::WallBudget => None,
        };
        let (completeness, stop_reason) = match (stop, end) {
            (Some(stop), _) => (CaptureCompleteness::Truncated, Some(stop.reason)),
            (None, CallEnd::WallBudget) => (
                CaptureCompleteness::Truncated,
                Some(CaptureStopReason::WallBudget),
            ),
            (None, CallEnd::Exited(Some(_))) => (CaptureCompleteness::Complete, None),
            (None, CallEnd::Exited(None)) => (CaptureCompleteness::Truncated, None),
        };
        let record = CaptureRecord {
            version: CAPTURE_RECORD_VERSION,
            session: self.session.to_string(),
            call: self.call_id.clone(),
            tool: self.tool.clone(),
            created_ms: self.created_ms,
            exit,
            completeness,
            stop_reason,
            stdout: stream_record(stdout, kept.stdout),
            stderr: stream_record(stderr, kept.stderr),
        };
        let bytes =
            serde_json::to_vec(&record).map_err(|e| CaptureError::RecordWrite(e.to_string()))?;
        let hash = self
            .log
            .backend()
            .block_put(bytes)
            .await
            .map_err(|e| CaptureError::RecordWrite(e.to_string()))?;
        Ok((hash, stop_reason))
    }
}

impl RawOutputReceiver for CallCapture {
    fn receive(&self, stream: CapturedStream, bytes: &[u8]) -> CaptureVerdict {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(stop) = &state.stop {
            return CaptureVerdict::Stop(stop.clone());
        }
        let held = state.stdout.len() + state.stderr.len();
        let room = self.budget.saturating_sub(held);
        if bytes.len() > room {
            state.stream_mut(stream).extend_from_slice(&bytes[..room]);
            let stop = state.record_stop(CaptureStopReason::ByteBudget);
            return CaptureVerdict::Stop(stop);
        }
        state.stream_mut(stream).extend_from_slice(bytes);
        CaptureVerdict::Continue
    }
}

impl ResultCapture for CallCapture {
    fn stop(&self) -> Option<CaptureStop> {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .stop
            .clone()
    }

    fn finish(&self, end: CallEnd) -> PFut<'_, Result<Option<CapturedStreams>, CaptureError>> {
        Box::pin(async move {
            let (stdout, stderr, stop) = {
                let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
                (
                    take(&mut state.stdout),
                    take(&mut state.stderr),
                    state.stop.clone(),
                )
            };
            let kept = KeptBytes {
                stdout: stdout.len() as u64,
                stderr: stderr.len() as u64,
            };
            if kept.stdout == 0 && kept.stderr == 0 {
                return Ok(None);
            }
            match &stop {
                Some(stop) => tracing::warn!(
                    reason = %stop.reason,
                    stdout_bytes = stop.stdout_bytes,
                    stderr_bytes = stop.stderr_bytes,
                    "capture stopped the command at its byte budget"
                ),
                None if end == CallEnd::WallBudget => tracing::warn!(
                    stdout_bytes = kept.stdout,
                    stderr_bytes = kept.stderr,
                    "capture kept a prefix because the wall budget ended the command"
                ),
                None => {}
            }
            let stdout_block = self.publish(&stdout).await?;
            let stderr_block = self.publish(&stderr).await?;
            let (record, stop_reason) = self
                .write_record(&stdout_block, &stderr_block, kept, &stop, end)
                .await?;
            Ok(Some(CapturedStreams {
                record,
                stdout: stdout_block,
                stderr: stderr_block,
                stdout_bytes: kept.stdout,
                stderr_bytes: kept.stderr,
                stop,
                stop_reason,
            }))
        })
    }
}

/// Bytes kept from each stream, counted apart so one stream's length cannot be
/// charged to the other.
#[derive(Debug, Clone, Copy)]
struct KeptBytes {
    stdout: u64,
    stderr: u64,
}

/// What one stream contributes to the record. The bytes are kept exactly as
/// the pipe delivered them, so the entry records that rather than claiming a
/// text encoding the capture never applied.
fn stream_record(block: &Option<BlockHash>, bytes: u64) -> StreamRecord {
    StreamRecord {
        block: block.as_ref().map(|hash| BlockMarker::new(hash.0.clone())),
        bytes,
        encoding: StreamEncoding::Raw,
    }
}

/// The clock reading a record carries, absent when the system clock is set
/// before the epoch. A record written without one names the field as null,
/// never zero: a zero would read as a measurement.
fn now_ms() -> Option<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis() as u64)
}

#[cfg(test)]
mod tests;
