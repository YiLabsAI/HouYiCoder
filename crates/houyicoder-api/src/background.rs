//! The boundary a long-running command crosses when it outlives its tool call.
//!
//! A tool call answers its model turn, so a command still running when the
//! foreground budget runs out has nowhere left to report. This module holds
//! the contract between the two sides: the tail a running command keeps for
//! display, the bounded answer a finished run produces, and the port a host
//! implements to take ownership of a run the foreground gives up.

use std::collections::VecDeque;
use std::mem::take;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use houyicoder_async::PFut;
use houyicoder_platform::host::text::{OutputEncoding, StreamDecoder};
use houyicoder_protocol::frontend::BackgroundTaskId;
use serde_json::Value;

use crate::capture::{CaptureVerdict, CapturedStream, RawOutputReceiver};

/// Output lines the tail keeps: enough for a reader to see progress, few
/// enough that the preview never outweighs the transcript row it sits under.
const TAIL_LINES: usize = 5;

/// Characters kept from one tail line. A build tool that writes a single very
/// long line (a compiler error with a full path and source quote) must not
/// push the other lines out of the window.
const TAIL_LINE_CHARS: usize = 200;

/// Characters buffered for a line no newline has ended yet. A command that
/// writes without newlines still yields tail lines rather than an unbounded
/// buffer.
const TAIL_PARTIAL_CHARS: usize = 4096;

#[derive(Default)]
struct TailState {
    lines: VecDeque<String>,
    partial: String,
    /// Characters in partial, carried as it grows. Counting the whole string
    /// per character would make a long line cost its own length squared.
    partial_chars: usize,
    /// Independent decoders keep a partial code unit on one stream from being
    /// completed by bytes arriving on the other.
    stdout: StreamDecoder,
    stderr: StreamDecoder,
}

/// A bounded window over the most recent lines a command printed.
///
/// The tail serves the running row and is not a record: it keeps the
/// last few lines, each shortened, and a reader that arrives late sees only
/// that window. Everything a command printed that matters is in the capture,
/// not here.
#[derive(Clone, Default)]
pub struct CommandTail {
    state: Arc<Mutex<TailState>>,
}

impl CommandTail {
    /// An empty tail.
    pub fn new() -> Self {
        Self::default()
    }

    /// A tail whose two output streams use the declared encoding.
    #[must_use]
    pub fn with_encoding(encoding: OutputEncoding) -> Self {
        Self {
            state: Arc::new(Mutex::new(TailState {
                stdout: StreamDecoder::new(encoding),
                stderr: StreamDecoder::new(encoding),
                ..TailState::default()
            })),
        }
    }

    /// Add one UTF-8 chunk of standard output.
    pub fn push_bytes(&self, bytes: &[u8]) {
        self.push_encoded(CapturedStream::Stdout, bytes);
    }

    /// Add one chunk from the named stream. Lines are split across chunk
    /// boundaries, and each stream's decoder retains incomplete code units
    /// until the rest arrives.
    pub fn push_encoded(&self, stream: CapturedStream, bytes: &[u8]) {
        let mut state = self.state.lock().expect("command tail lock");
        let decoder = match stream {
            CapturedStream::Stdout => &mut state.stdout,
            CapturedStream::Stderr => &mut state.stderr,
        };
        let text = decoder.push(bytes);
        Self::push_text(&mut state, &text);
    }

    fn push_text(state: &mut TailState, text: &str) {
        for ch in text.chars() {
            match ch {
                '\n' => {
                    let line = take(&mut state.partial);
                    state.partial_chars = 0;
                    Self::push_line(state, line);
                }
                // A carriage return redraws the line it returns to. Dropping
                // it keeps a progress bar from becoming a stream of fragments.
                '\r' => {}
                ch => {
                    state.partial.push(ch);
                    state.partial_chars += 1;
                    if state.partial_chars >= TAIL_PARTIAL_CHARS {
                        let line = take(&mut state.partial);
                        state.partial_chars = 0;
                        Self::push_line(state, line);
                    }
                }
            }
        }
    }

    fn push_line(state: &mut TailState, line: String) {
        let line: String = if line.chars().count() > TAIL_LINE_CHARS {
            line.chars().take(TAIL_LINE_CHARS).collect()
        } else {
            line
        };
        state.lines.push_back(line);
        while state.lines.len() > TAIL_LINES {
            state.lines.pop_front();
        }
    }

    /// Flush each decoder's buffered trailing unit into the window.
    ///
    /// The last chunk of a stream can cut a character in half: UTF-8 keeps
    /// the leading bytes, UTF-16 keeps an odd byte or a lone high surrogate.
    /// At end of stream those bytes never gain a partner, so they are decoded
    /// once here and the window ends with the character the command printed
    /// rather than dropping it.
    pub fn finish(&self) {
        let mut state = self.state.lock().expect("command tail lock");
        let stdout = state.stdout.finish();
        let stderr = state.stderr.finish();
        Self::push_text(&mut state, &stdout);
        Self::push_text(&mut state, &stderr);
    }

    /// The lines the window holds right now, oldest first, including a line
    /// no newline has ended yet. A partial line is shortened to the same cap
    /// as a finished one, so a snapshot never carries a line longer than the
    /// row that shows it.
    pub fn snapshot(&self) -> Vec<String> {
        let state = self.state.lock().expect("command tail lock");
        let mut lines: Vec<String> = state.lines.iter().cloned().collect();
        if !state.partial.is_empty() {
            let partial = if state.partial.chars().count() > TAIL_LINE_CHARS {
                state.partial.chars().take(TAIL_LINE_CHARS).collect()
            } else {
                state.partial.clone()
            };
            lines.push(partial);
        }
        if lines.len() > TAIL_LINES {
            lines.drain(..lines.len() - TAIL_LINES);
        }
        lines
    }
}

/// A raw-output receiver that feeds a tail and passes every chunk on to the
/// capture, so previewing a run costs no bytes from what the host keeps.
pub struct TailingReceiver {
    tail: CommandTail,
    capture: Option<Arc<dyn RawOutputReceiver>>,
}

impl TailingReceiver {
    /// Wrap the capture receiver, if the host supplied one.
    pub fn new(tail: CommandTail, capture: Option<Arc<dyn RawOutputReceiver>>) -> Self {
        Self { tail, capture }
    }
}

impl RawOutputReceiver for TailingReceiver {
    fn receive(&self, stream: CapturedStream, bytes: &[u8]) -> CaptureVerdict {
        self.tail.push_encoded(stream, bytes);
        match &self.capture {
            Some(capture) => capture.receive(stream, bytes),
            None => CaptureVerdict::Continue,
        }
    }

    fn end_of_stream(&self) {
        self.tail.finish();
        if let Some(capture) = &self.capture {
            capture.end_of_stream();
        }
    }
}

/// What a finished command produced, reduced to what the call's readers need.
///
/// The foreground answer and the notification a detached run produces are
/// built from this same value, so a command that ends inside the budget and
/// one that ends after the background move cannot report different endings for
/// the same exit status.
pub struct CommandCompletion {
    /// The process exit status. None when the command was stopped before it
    /// ended, or when a signal ended it.
    pub exit_code: Option<i32>,
    /// Standard output, bounded the way a tool result is.
    pub stdout: String,
    /// Standard error, bounded the way a tool result is, with the call's own
    /// notices folded in.
    pub stderr: String,
    /// Set when the command reached its own wall-clock budget.
    pub timed_out: bool,
    /// Set when the command never produced a normal result: the execution
    /// backend refused or failed. The text is the reason.
    pub error: Option<String>,
    /// The capture reference naming the stored raw streams, when they were
    /// kept. The answer that ends the tool call carries it, and so does the
    /// notification a run that outlived its tool call leaves for the next
    /// turn: the stored bytes are read back through it, so a copy that
    /// nothing names is released at the next reclaim.
    pub capture: Option<Value>,
}

impl CommandCompletion {
    /// Whether the command exited with a zero status.
    pub fn is_success(&self) -> bool {
        self.exit_code == Some(0)
    }
}

/// One command run passed to a host to finish after the tool call ended.
pub struct DetachedCommand {
    /// The command as issued, for the task row and the completion notice.
    pub command: String,
    /// When the command started, before the foreground budget elapsed. The
    /// host measures the run from here, so a task that finishes just after
    /// the background move does not report a duration shorter than the wait the user
    /// already watched.
    pub started_at: Instant,
    /// The most recent output lines, shared with the running command.
    pub tail: CommandTail,
    /// The run's body. It resolves when the command ends, holding everything
    /// the completion notice needs. Dropping it ends the command.
    pub completion: PFut<'static, CommandCompletion>,
}

/// A host that keeps commands running after the tool call that started them.
///
/// The engine passes the run over exactly once and never polls it again, so
/// the host owns the command's ending: it reports the final state to the
/// frontend and notifies the model at its next turn boundary. The acceptance
/// occurs only after the foreground budget ran out, never before.
pub trait BackgroundCommandLauncher: Send + Sync {
    /// Take ownership of a still-running command and return the number the
    /// host assigned it.
    fn launch(&self, command: DetachedCommand) -> BackgroundTaskId;
}

#[cfg(test)]
mod tests;
