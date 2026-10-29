//! Source capture: the raw output a command printed, kept before the tool
//! result is bounded for the model.
//!
//! A tool result that enters the model context is bounded, so bytes above the
//! bound exist nowhere unless the host keeps them while they are read. The
//! sandbox layer and the engine sit on opposite sides of that boundary, so the
//! contract lives here: the receiver an exec hands each chunk to, and the port
//! the engine asks for one receiver per dispatched call.

use std::error::Error;
use std::fmt;
use std::sync::Arc;

use houyicoder_async::PFut;
use houyicoder_context::{BlockHash, SessionId};

/// Which of a command's two output streams a chunk belongs to. The two are
/// recorded apart: their relative order is not known, and the caller must not
/// pretend it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapturedStream {
    /// Standard output.
    Stdout,
    /// Standard error.
    Stderr,
}

pub use houyicoder_protocol::capture_record::CaptureStopReason;

/// The state capture held when it stopped, so a reader learns how much of the
/// output survived and why the rest did not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureStop {
    /// What stopped the capture.
    pub reason: CaptureStopReason,
    /// Bytes kept from standard output.
    pub stdout_bytes: u64,
    /// Bytes kept from standard error.
    pub stderr_bytes: u64,
}

/// What the receiver asks the exec loop to do next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureVerdict {
    /// Keep reading.
    Continue,
    /// Stop the command and record why: the capture cannot take more.
    Stop(CaptureStop),
}

/// Consumes a command's raw output as it leaves the pipe.
///
/// The method is deliberately synchronous and cheap. The exec loop calls it
/// inline between pipe reads, so a receiver that takes long holds the pipe full
/// and the producer waits; that is the backpressure a capture over a bounded
/// buffer applies. A receiver that cannot keep the bytes returns Stop instead
/// of growing without end.
pub trait RawOutputReceiver: Send + Sync {
    /// Take one chunk read from the given stream.
    fn receive(&self, stream: CapturedStream, bytes: &[u8]) -> CaptureVerdict;
}

/// What a finished capture published. Each stream is one content-addressed
/// block, so the event that names it stays small and the reclaim pass follows
/// the same address the reader does.
#[derive(Debug, Clone)]
pub struct CapturedStreams {
    /// The record block: who captured what, for which call. The reader
    /// authorizes a request against it before handing back any stream block.
    pub record: BlockHash,
    /// The block holding everything read from standard output, absent when
    /// nothing was read from it.
    pub stdout: Option<BlockHash>,
    /// The block holding everything read from standard error, absent when
    /// nothing was read from it.
    pub stderr: Option<BlockHash>,
    /// Bytes kept from standard output.
    pub stdout_bytes: u64,
    /// Bytes kept from standard error.
    pub stderr_bytes: u64,
    /// Set when capture stopped before the command's output ended.
    pub stop: Option<CaptureStop>,
    /// What the record says ended the command early, which is the stop above
    /// or the end the caller reported. Absent when the kept bytes are the
    /// whole output, and also when a command ended without a status and
    /// nothing says why.
    pub stop_reason: Option<CaptureStopReason>,
}

/// How the call ended, as the caller observed it.
///
/// The capture keeps this apart from the stop it applies itself: either one
/// can end a command before its output does, and a reader of the record must
/// learn which happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallEnd {
    /// The command ended on its own. The status is absent when no status was
    /// observed, which includes a command a signal killed.
    Exited(Option<i32>),
    /// The execution strategy ended the command at its wall budget, so the
    /// bytes kept are the prefix it printed in time.
    WallBudget,
}

/// A capture of one call's raw output, from the first chunk to the published
/// blocks.
pub trait ResultCapture: RawOutputReceiver {
    /// The stop this capture applied, if any.
    ///
    /// A caller reads it before finish, so an outcome whose publication then
    /// fails still says why the command ended early. A capture that stops
    /// nothing returns None.
    fn stop(&self) -> Option<CaptureStop>;

    /// Publish what was captured and name the blocks it went to.
    ///
    /// How the call ended is recorded alongside the streams, so a reader
    /// resolving the record alone learns whether the kept bytes are the whole
    /// output: only a command that exited on its own with a status is
    /// recorded as complete.
    ///
    /// Returns None when nothing was read from either stream, so nothing was
    /// written and the caller keeps its ordinary result. A publication failure
    /// is an error, never a silent success: the caller still holds the bounded
    /// result and reports that the raw was not kept.
    ///
    /// The call is terminal. The exec hands over every chunk before it returns,
    /// so a capture receives nothing after this.
    fn finish(&self, end: CallEnd) -> PFut<'_, Result<Option<CapturedStreams>, CaptureError>>;
}

/// A capture that could not publish what it holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureError {
    /// A stream block could not be written. The detail is for the diagnostic
    /// log; the model sees only that the raw output was not kept.
    StreamWrite(String),
    /// The record could not be serialized or written.
    RecordWrite(String),
}

impl fmt::Display for CaptureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StreamWrite(detail) => write!(f, "block write failed: {detail}"),
            Self::RecordWrite(detail) => write!(f, "record write failed: {detail}"),
        }
    }
}

impl Error for CaptureError {}

/// Starts one capture per dispatched call.
///
/// Returns None when the host has no durable store to publish to, so a tool
/// without one pays nothing and produces its result as it otherwise would.
pub trait ResultCaptureSource: Send + Sync {
    /// Begin capturing the raw output of one call.
    fn begin(
        &self,
        session: SessionId,
        call_id: &str,
        tool: &str,
    ) -> Option<Arc<dyn ResultCapture>>;
}
