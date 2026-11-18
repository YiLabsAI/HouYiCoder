//! Clipboard delivery for a completed selection.
//!
//! The write runs on a background thread: the native clipboard API is fast,
//! but a subprocess utility or a contended clipboard can hold for the whole
//! bounded wait, and the release gesture must not freeze the interface for
//! that. A typed outcome reports what happened, delivered to the loop
//! through the copy result queue the loop polls each pass.

use std::env::var_os;
use std::io::stdout;
use std::io::{Result, Write as _};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::spawn;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use houyicoder_platform::desktop::clipboard::{CopyOutcome, copy_to_native_clipboard};

use crate::app::wake::ui_wake;

/// What one copy accomplished, from the caller that owns the terminal.
///
/// The platform reports what the native paths did; this report adds the
/// terminal escape the caller may have written, so a notice never claims
/// an escape that was not sent or a copy that did not happen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyReport {
    /// The native clipboard accepted the text.
    NativeCopied,
    /// The tmux paste buffer holds the text.
    TmuxBufferCopied,
    /// The terminal escape was written as the only path. Whether the
    /// terminal applies it is outside this process's view, so the report is
    /// neutral rather than a success claim.
    TerminalRequested,
    /// The native write failed and the terminal escape was written as a
    /// fallback.
    NativeFailedTerminalRequested,
    /// No path delivered the text.
    CopyFailed,
}

/// One completed copy the loop has not reported yet: the copied text's
/// length and the outcome the write reported.
pub struct CopyReceipt {
    /// The character count of the copied text, for the toast message.
    pub chars: usize,
    /// What the write accomplished.
    pub outcome: CopyReport,
}

/// The queue a background copy reports through. The loop drains it each
/// pass; the wake raised with every send is what guarantees the drain
/// happens on the frame the result lands. One channel serves the queue's
/// whole life: the sender is cloned to every writer and the receiver is
/// shared, so a receipt sent before the loop polls is still waiting when
/// it does.
#[derive(Clone)]
pub struct CopyResultQueue {
    sender: Sender<CopyReceipt>,
    receiver: Arc<Mutex<Receiver<CopyReceipt>>>,
}

impl CopyResultQueue {
    /// A queue wired to the process-wide wake.
    pub fn new() -> Self {
        let (sender, receiver) = channel();
        Self {
            sender,
            receiver: Arc::new(Mutex::new(receiver)),
        }
    }

    /// Drain every receipt waiting on the queue, in send order. A receipt
    /// that lands between two drains is read by the next one.
    pub fn drain(&self) -> Vec<CopyReceipt> {
        let receiver = self
            .receiver
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut receipts = Vec::new();
        while let Ok(receipt) = receiver.try_recv() {
            receipts.push(receipt);
        }
        receipts
    }

    /// Report one completed copy, waking the loop to drain it.
    pub fn send(&self, receipt: CopyReceipt) {
        let _ = self.sender.send(receipt);
        ui_wake().wake();
    }
}

impl Default for CopyResultQueue {
    fn default() -> Self {
        Self::new()
    }
}

/// Pluggable clipboard sink so adversarial selection tests can capture what a
/// drag-to-copy would write, without touching the real OS clipboard or
/// emitting OSC 52. Production holds a SystemClipboard; tests inject a
/// RecordingClipboard and assert the exact copied text.
pub trait ClipboardWriter: Send + Sync {
    /// Start one copy and report the outcome through the queue when it
    /// completes. Returns at once: the write runs on a background thread,
    /// so a slow or hung utility never freezes the release gesture.
    fn write(&self, text: &str, results: &CopyResultQueue);
}

/// The copy body one writer runs: what the native paths did plus the
/// terminal escape this process owns. A test substitutes its own body so
/// the async plumbing can be exercised without a real clipboard.
type CopyBody = dyn Fn(&str) -> CopyReport + Send + Sync;

/// The production clipboard writer: the host's native clipboard where the
/// platform offers one, since it works even when the terminal refuses the
/// escape; the terminal escape carries the text over a remote session.
pub struct SystemClipboard {
    copy: Arc<CopyBody>,
}

impl SystemClipboard {
    /// A writer that performs the real copy.
    pub fn new() -> Self {
        Self {
            copy: Arc::new(write_clipboard),
        }
    }

    /// A writer whose copy body is supplied by the caller, so a test can
    /// drive the asynchronous path without touching a real clipboard.
    #[cfg(test)]
    pub fn with_copy(copy: impl Fn(&str) -> CopyReport + Send + Sync + 'static) -> Self {
        Self {
            copy: Arc::new(copy),
        }
    }
}

impl Default for SystemClipboard {
    fn default() -> Self {
        Self::new()
    }
}

impl ClipboardWriter for SystemClipboard {
    fn write(&self, text: &str, results: &CopyResultQueue) {
        let text = text.to_string();
        let results = results.clone();
        let copy = Arc::clone(&self.copy);
        spawn(move || {
            let outcome = copy(&text);
            results.send(CopyReceipt {
                chars: text.chars().count(),
                outcome,
            });
        });
    }
}

#[cfg(test)]
/// Test-only clipboard that records every write so adversarial selection
/// tests can assert the exact copied text without touching the OS clipboard,
/// and reports a native success synchronously.
pub struct RecordingClipboard {
    pub captured: Arc<Mutex<Vec<String>>>,
}

#[cfg(test)]
impl Default for RecordingClipboard {
    fn default() -> Self {
        Self {
            captured: Arc::new(Mutex::new(Vec::new())),
        }
    }
}
#[cfg(test)]
impl ClipboardWriter for RecordingClipboard {
    fn write(&self, text: &str, results: &CopyResultQueue) {
        self.captured
            .lock()
            .expect("recording clipboard")
            .push(text.to_string());
        let chars = text.chars().count();
        results.send(CopyReceipt {
            chars,
            outcome: CopyReport::NativeCopied,
        });
    }
}

/// Write text through the native paths, falling back to the terminal escape
/// when they cannot deliver. A native success suppresses the escape so the
/// two writers never compete.
fn write_clipboard(text: &str) -> CopyReport {
    finish_copy(copy_to_native_clipboard(text), text, emit_raw)
}

/// Turn the platform's outcome into the report the notice reads, writing
/// the terminal escape whenever no native path delivered the text. The
/// escape writer is a parameter so a test can prove the escape is written
/// on exactly the outcomes that need it.
pub(crate) fn finish_copy(
    native: CopyOutcome,
    text: &str,
    emit: impl Fn(&str) -> Result<()>,
) -> CopyReport {
    match native {
        CopyOutcome::NativeCopied => CopyReport::NativeCopied,
        CopyOutcome::TmuxBufferCopied => CopyReport::TmuxBufferCopied,
        CopyOutcome::NativeFailed => match emit_osc52(text, &emit) {
            CopyReport::TerminalRequested => CopyReport::NativeFailedTerminalRequested,
            failed => failed,
        },
        CopyOutcome::NoNativePath => emit_osc52(text, &emit),
    }
}

/// Emit the OSC 52 escape and report whether it reached the terminal.
fn emit_osc52(text: &str, emit: &impl Fn(&str) -> Result<()>) -> CopyReport {
    let b64 = STANDARD.encode(text.as_bytes());
    let seq = osc_sequence(&b64, var_os("TMUX").is_some());
    match emit(&seq) {
        Ok(()) => CopyReport::TerminalRequested,
        Err(_) => CopyReport::CopyFailed,
    }
}

/// The escape bytes for one copy. Inside tmux a bare sequence is consumed by
/// the multiplexer before the outer terminal sees it, so it is wrapped in
/// the passthrough envelope; outside, the raw form is what the terminal
/// reads.
pub(crate) fn osc_sequence(b64: &str, in_tmux: bool) -> String {
    if in_tmux {
        tmux_passthrough_osc52(b64)
    } else {
        raw_osc52(b64)
    }
}

/// Raw OSC 52: ESC ] 52 ; c ; <base64> BEL. BEL is more widely parsed than
/// ST (ESC backslash) for OSC 52 across terminals.
pub(crate) fn raw_osc52(b64: &str) -> String {
    format!("\x1b]52;c;{b64}\x07")
}

/// OSC 52 wrapped in the tmux passthrough envelope, with every inner escape
/// doubled as the envelope requires.
fn tmux_passthrough_osc52(b64: &str) -> String {
    let escaped = raw_osc52(b64).replace('\x1b', "\x1b\x1b");
    format!("\x1bPtmux;{escaped}\x1b\\")
}

/// Write a byte sequence to stdout (locked, flushed). OSC 52 is a one-way
/// escape the terminal does not acknowledge; a stdout write failure is
/// reported so the caller can choose a failure outcome.
fn emit_raw(seq: &str) -> Result<()> {
    let mut out = stdout().lock();
    out.write_all(seq.as_bytes())?;
    out.flush()
}
