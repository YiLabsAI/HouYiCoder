//! The live-delta sink: streams token-level deltas onto the wire as
//! acpx/llm/* notifications. A self-contained concern split out of server.rs
//! so server.rs stays under the file-size gate. The sink-builder is extracted
//! from the installer so it can be unit-tested without a Runner.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use futures::channel::mpsc;

use houyicoder_api::live::LiveEvent;
use houyicoder_core::agent::Runner;
use houyicoder_protocol::acpx::{AcpxMethod, AcpxNotification};
use houyicoder_protocol::envelope::{EventEnvelope, EventSeq, ServerFrame};
use houyicoder_protocol::framing::encode;
use houyicoder_protocol::frontend::FrontendEventKind;

/// Build the live-delta sink: a closure that encodes each engine delta as a
/// ServerFrame::Event and try_sends it to the outbound channel, minting the
/// event seq from the shared counter the server also uses for durable events
/// (one monotonic seq stream; the driver resume cursor relies on it). try_send
/// is non-blocking: a full channel drops the delta (ephemeral preview — the
/// authoritative AssistantMessage replaces it).
///
/// The bounded Sender's try_send takes &mut but the sink is a plain Fn, so
/// the sender is wrapped in a mutex for interior mutability. The lock is
/// uncontended (only the sink fires deltas; the server's durable send path
/// uses its own &mut io) and held only for the non-blocking try_send, so it
/// never blocks the engine run thread. The sink captures its own sender + seq
/// counter clone, so it touches neither Server nor ServerIo at fire time (no
/// borrow entanglement with the run future).
pub fn build_delta_sink(
    out_tx: mpsc::Sender<String>,
    next_seq: Arc<std::sync::atomic::AtomicU64>,
) -> houyicoder_api::live::LiveSink {
    let out_tx = std::sync::Mutex::new(out_tx);
    Arc::new(move |ev: &LiveEvent| {
        // MemorySaved is not a token delta: it carries a count + a kind, not
        // text. Build a typed FrontendEventKind::MemorySaved frame (the
        // frontend renders the verb + plural), not an acpx/llm/* notification.
        // The drop is best-effort like deltas, but a notice is not
        // replaceable by a later authoritative frame, so log on a full
        // channel instead of failing silently.
        let frame = match ev {
            LiveEvent::AssistantDelta { text } => {
                let notification = AcpxNotification::new(
                    AcpxMethod::LlmTextDelta,
                    serde_json::json!({ "text": text }),
                );
                FrontendEventKind::Acpx { notification }
            }
            LiveEvent::ReasoningDelta { text } => {
                let notification = AcpxNotification::new(
                    AcpxMethod::LlmReasoningDelta,
                    serde_json::json!({ "text": text }),
                );
                FrontendEventKind::Acpx { notification }
            }
            LiveEvent::MemorySaved { count, kind } => FrontendEventKind::MemorySaved {
                count: *count,
                kind: *kind,
            },
            LiveEvent::ToolProgress {
                call_id,
                elapsed_secs,
                lines,
            } => FrontendEventKind::Acpx {
                notification: AcpxNotification::new(
                    AcpxMethod::ToolProgress,
                    serde_json::json!({ "call_id": call_id, "elapsed_secs": elapsed_secs, "lines": lines }),
                ),
            },
            LiveEvent::SystemLine { text } => FrontendEventKind::SystemLine { text: text.clone() },
        };
        let seq = next_seq.fetch_add(1, Ordering::Relaxed);
        let frame = ServerFrame::Event(EventEnvelope::new(EventSeq(seq), frame));
        if let Ok(line) = encode(&frame)
            && let Ok(mut tx) = out_tx.lock()
        {
            // try_send is non-blocking: a full channel drops the event. Deltas
            // are ephemeral (an authoritative message replaces them); a
            // MemorySaved notice is not replaceable, so surface the drop so
            // it is not a silent failure.
            if tx.try_send(line).is_err() && matches!(ev, LiveEvent::MemorySaved { .. }) {
                tracing::warn!("memory-saved notice dropped: outbound channel full");
            }
        }
    })
}

/// Install the live-delta sink on a runner. Thin wrapper over build_delta_sink
/// so the composition root can arm the runner in one call.
pub fn install_delta_sink(
    runner: &mut Runner,
    out_tx: mpsc::Sender<String>,
    next_seq: Arc<std::sync::atomic::AtomicU64>,
) {
    runner.set_live_sink(build_delta_sink(out_tx, next_seq));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A text delta fires one acpx/llm/text_delta ServerFrame::Event onto the
    /// channel, with the seq minted from the shared counter. A reasoning delta
    /// fires the reasoning variant. A full channel drops the delta silently
    /// (try_send non-blocking) rather than stalling the engine thread.
    #[test]
    fn test_sink_encodes_as_event() {
        let (tx, mut rx) = mpsc::channel(8);
        let seq = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let sink = build_delta_sink(tx, seq.clone());

        sink(&LiveEvent::AssistantDelta {
            text: "hello".into(),
        });
        sink(&LiveEvent::ReasoningDelta {
            text: "thinking".into(),
        });

        // Two frames land, in order, each a complete NDJSON line encoding a
        // ServerFrame::Event whose acpx notification carries the delta text.
        let line1 = rx.try_recv().unwrap();
        let frame1: ServerFrame = serde_json::from_str(&line1).unwrap();
        let ServerFrame::Event(ev1) = frame1 else {
            panic!("expected an event frame");
        };
        assert_eq!(ev1.seq, EventSeq(0));
        match ev1.payload {
            FrontendEventKind::Acpx { notification } => {
                assert_eq!(notification.method, AcpxMethod::LlmTextDelta);
                assert_eq!(notification.params["text"], "hello");
            }
            _ => panic!("expected an acpx event"),
        }

        let line2 = rx.try_recv().unwrap();
        let frame2: ServerFrame = serde_json::from_str(&line2).unwrap();
        let ServerFrame::Event(ev2) = frame2 else {
            panic!("expected an event frame");
        };
        assert_eq!(ev2.seq, EventSeq(1));
        match ev2.payload {
            FrontendEventKind::Acpx { notification } => {
                assert_eq!(notification.method, AcpxMethod::LlmReasoningDelta);
            }
            _ => panic!("expected an acpx event"),
        }
        assert_eq!(seq.load(Ordering::Relaxed), 2);
    }

    /// A ToolProgress tick (bash elapsed) rides the acpx notification stream
    /// so the host routes it to the chip. Pins the adapter side of the
    /// bash-elapsed channel (call_id + elapsed_secs in params).
    #[test]
    fn test_sink_encodes_tool_progress() {
        let (tx, mut rx) = mpsc::channel(8);
        let seq = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let sink = build_delta_sink(tx, seq);
        sink(&LiveEvent::ToolProgress {
            call_id: "c1".into(),
            elapsed_secs: 12,
            lines: Some(14),
        });
        let line = rx.try_recv().unwrap();
        let frame: ServerFrame = serde_json::from_str(&line).unwrap();
        let ServerFrame::Event(ev) = frame else {
            panic!("expected an event frame");
        };
        match ev.payload {
            FrontendEventKind::Acpx { notification } => {
                assert_eq!(notification.method, AcpxMethod::ToolProgress);
                assert_eq!(notification.params["call_id"], "c1");
                assert_eq!(notification.params["elapsed_secs"], 12);
            }
            _ => panic!("expected an acpx event for tool progress"),
        }
    }

    /// A SystemLine notice rides the event stream as a typed
    /// FrontendEventKind::SystemLine (not an acpx notification) so the host
    /// renders it verbatim as a transcript system line.
    #[test]
    fn test_sink_encodes_system_line() {
        let (tx, mut rx) = mpsc::channel(8);
        let seq = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let sink = build_delta_sink(tx, seq);
        sink(&LiveEvent::SystemLine {
            text: "set catalog context_window".into(),
        });
        let line = rx.try_recv().unwrap();
        let frame: ServerFrame = serde_json::from_str(&line).unwrap();
        let ServerFrame::Event(ev) = frame else {
            panic!("expected an event frame");
        };
        match ev.payload {
            FrontendEventKind::SystemLine { text } => {
                assert_eq!(text, "set catalog context_window");
            }
            _ => panic!("expected a system_line event"),
        }
    }
}
