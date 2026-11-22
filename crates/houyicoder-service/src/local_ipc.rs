//! Local inter-process carrier for detached sessions. A background agent
//! process binds its platform endpoint and each accepted connection runs
//! serve_session over the stream.
//!
//! A connecting client picks its dialect by the first frame: a JSON-RPC
//! initialize frame routes to the ACP server, a Hello frame routes to the
//! rich frontend server. The first frame is bounded by the protocol's
//! message limit, and the BufReader preserves bytes after the newline.

use std::io::Error as IoError;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use futures::SinkExt;
use futures::StreamExt;
use futures::channel::mpsc;
use houyicoder_api::agent_event::ResponseStreamEvent;
use houyicoder_context::SessionId;
use houyicoder_platform::ipc::LocalListener;
use houyicoder_protocol::acp_wire::AcpRequest;
use houyicoder_protocol::acp_wire::NewSessionRequest;
use houyicoder_protocol::acpx::AcpxCapabilities;
use houyicoder_protocol::error::{ErrorCategory, ProtocolError};
use houyicoder_protocol::framing::FrameDecoder;
use houyicoder_protocol::framing::MAX_MESSAGE_BYTES;
use tokio::io::AsyncBufRead;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::io::split;
use tokio::sync::mpsc as tokio_mpsc;

use crate::acp::adapter::AcpAdapter;
use crate::acp::adapter::SessionFacts;
use crate::acp::adapter::SessionView;
use crate::acp::adapter::model_options;
use crate::acp::adapter::session_config_options;
use crate::acp::adapter::session_modes;
use crate::acp::live::AcpLiveStream;
use crate::acp::pump::from_line_channels;
use crate::acp::server::AcpServer;
use crate::composition::RunnerHandleClone;
use crate::composition::SessionHost;
use crate::server::FrameCarrier;
use crate::server::session::serve_session;

/// The connection's place in the accept order. A newer connection to the same
/// session bumps the shared counter, so an older connection sees itself as
/// stale and stops writing its queued frames instead of replaying them over
/// the new connection.
#[derive(Clone)]
pub(crate) struct ConnectionEpoch {
    current: Arc<AtomicU64>,
    mine: u64,
}

impl ConnectionEpoch {
    fn is_stale(&self) -> bool {
        self.current.load(Ordering::Relaxed) != self.mine
    }
}

pub(crate) async fn serve_local_stream(
    host: Arc<SessionHost>,
    session: SessionId,
    stream: impl AsyncRead + AsyncWrite + Unpin + Send + 'static,
    epoch: ConnectionEpoch,
) -> Result<(), ProtocolError> {
    let Some(handle) = host.clone_handle(session) else {
        let io = bridge(stream).await;
        return serve_session(host, session, io).await;
    };
    let (read_half, write_half) = split(stream);
    let mut reader = BufReader::new(read_half);
    let Some(first_frame) = read_first_frame(&mut reader).await? else {
        return Ok(());
    };
    if is_acp_initialize(&first_frame) {
        serve_acp_attach(
            host,
            handle,
            session,
            epoch,
            first_frame,
            reader,
            write_half,
        )
        .await
    } else {
        let io = bridge_halves(reader, write_half, Some(first_frame)).await;
        serve_session(host, session, io).await
    }
}

/// Read the first frame with the protocol's message bound. Bytes after the
/// terminating newline stay in the reader's buffer, so the chosen carrier
/// sees the same stream it would have without the dialect peek. None means
/// the peer closed before sending anything.
async fn read_first_frame<R>(reader: &mut R) -> Result<Option<String>, ProtocolError>
where
    R: AsyncBufRead + Unpin,
{
    let mut line = Vec::new();
    loop {
        let buf = reader.fill_buf().await.map_err(|e| {
            ProtocolError::new(
                ErrorCategory::Unavailable,
                format!("read first frame: {e}"),
                false,
            )
        })?;
        if buf.is_empty() {
            return if line.is_empty() {
                Ok(None)
            } else {
                Ok(Some(String::from_utf8_lossy(&line).into_owned()))
            };
        }
        let take = buf
            .iter()
            .position(|&b| b == b'\n')
            .map_or(buf.len(), |p| p + 1);
        line.extend_from_slice(&buf[..take]);
        reader.consume(take);
        if line.len() > MAX_MESSAGE_BYTES {
            return Err(ProtocolError::new(
                ErrorCategory::InvalidFrame,
                "first frame exceeds the message bound",
                false,
            ));
        }
        if line.last() == Some(&b'\n') {
            line.pop();
            return Ok(Some(String::from_utf8_lossy(&line).into_owned()));
        }
    }
}

fn is_acp_initialize(frame: &str) -> bool {
    serde_json::from_str::<AcpRequest>(frame).is_ok_and(|r| r.method == "initialize")
}

pub async fn listen_local(
    host: Arc<SessionHost>,
    session: SessionId,
    mut listener: LocalListener,
) -> Result<(), ProtocolError> {
    let current = Arc::new(AtomicU64::new(1));
    loop {
        let stream = listener.accept().await.map_err(endpoint_failure)?;
        let mine = current.fetch_add(1, Ordering::Relaxed) + 1;
        // A newer connection supersedes every older one: bump the counter
        // before spawning so a reconnect immediately marks the prior
        // connection stale, and its queued frames stop instead of replaying.
        let epoch = ConnectionEpoch {
            current: Arc::clone(&current),
            mine,
        };
        let host = host.clone();
        tokio::spawn(async move {
            tracing::debug!(epoch = mine, "connection accepted");
            if let Err(e) = serve_local_stream(host, session, stream, epoch).await {
                tracing::warn!(error = %e, "local session stream closed");
            }
        });
    }
}

fn endpoint_failure(e: IoError) -> ProtocolError {
    ProtocolError::new(
        ErrorCategory::Unavailable,
        format!("local endpoint: {e}"),
        true,
    )
}

async fn bridge<S>(stream: S) -> FrameCarrier
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (read_half, write_half) = split(stream);
    bridge_halves(BufReader::new(read_half), write_half, None).await
}

async fn bridge_halves<R, W>(
    mut reader: BufReader<R>,
    write_half: W,
    first_frame: Option<String>,
) -> FrameCarrier
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (inbound_tx, inbound_rx) = mpsc::channel::<String>(16);
    let (outbound_tx, outbound_rx) = mpsc::channel::<String>(16);
    // The re-injected first frame is sent inside the reader task before its
    // loop, so it cannot be overtaken by a frame read from the stream.
    tokio::spawn(async move {
        let mut tx = inbound_tx;
        if let Some(frame) = first_frame
            && tx.send(frame).await.is_err()
        {
            return;
        }
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line).await {
                Ok(0) => break,
                Ok(_) => {
                    let frame = line.strip_suffix('\n').unwrap_or(&line).to_string();
                    if tx.send(frame).await.is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
    let mut writer = write_half;
    tokio::spawn(async move {
        let mut rx = outbound_rx;
        while let Some(frame) = rx.next().await {
            let mut line = frame;
            if !line.ends_with('\n') {
                line.push('\n');
            }
            if writer.write_all(line.as_bytes()).await.is_err() || writer.flush().await.is_err() {
                break;
            }
        }
        drop(writer.shutdown().await);
    });
    FrameCarrier::new(outbound_tx, inbound_rx)
}

async fn serve_acp_attach<R, W>(
    host: Arc<SessionHost>,
    handle: RunnerHandleClone,
    session: SessionId,
    epoch: ConnectionEpoch,
    first_frame: String,
    reader: BufReader<R>,
    write_half: W,
) -> Result<(), ProtocolError>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let runner = handle.runner;
    let gate = handle.gate;
    let models = model_options(&runner);
    let facts_runner = runner.clone();
    let facts_gate = gate.clone();
    let facts_models = models.clone();
    let facts: SessionFacts = Arc::new(move || SessionView {
        modes: session_modes(facts_gate.as_ref()),
        config_options: session_config_options(facts_runner.as_ref(), &facts_models),
    });
    // Bind session/new to the attached session and share the host's lease
    // store, so a client prompting with the id it received reaches this
    // session rather than a fresh empty one.
    let binding = Arc::new(move |_params: &NewSessionRequest| session);
    let adapter = Arc::new(
        AcpAdapter::new(AcpxCapabilities::default(), 1, host.store().clone())
            .with_session_facts(facts)
            .with_session_binding(binding),
    );
    let mut broadcast_rx = handle.event_sequencer.subscribe_response_stream();
    let (live_tx, live_rx) = tokio_mpsc::unbounded_channel::<ResponseStreamEvent>();
    tokio::spawn(async move {
        while let Ok(delta) = broadcast_rx.recv().await {
            if live_tx.send(delta).is_err() {
                return;
            }
        }
    });
    let live = AcpLiveStream::from_receiver(live_rx);
    let server = AcpServer::new(adapter, runner, gate, session)
        .with_live_stream(live)
        .with_model_options(models)
        .with_bus(host.bus().cloned());
    let (inbound_tx, inbound_rx) = tokio_mpsc::channel::<String>(16);
    let (outbound_tx, outbound_rx) = tokio_mpsc::channel::<String>(16);
    drop(inbound_tx.send(first_frame).await);
    let inbound_tx = inbound_tx;
    let read_epoch = epoch.clone();
    tokio::spawn(async move {
        // A FrameDecoder bounds each inbound frame at the protocol limit and
        // hands complete lines to the pump, so a peer streaming a line that
        // never terminates cannot exhaust memory.
        let mut decoder = FrameDecoder::new(MAX_MESSAGE_BYTES);
        let mut reader = reader;
        let tx = inbound_tx;
        let mut chunk = vec![0u8; 8192];
        loop {
            if read_epoch.is_stale() {
                return;
            }
            match reader.read(&mut chunk).await {
                Ok(0) => {
                    if let Some(line) = decoder.finish()
                        && tx.send(line).await.is_err()
                    {
                        return;
                    }
                    break;
                }
                Ok(n) => {
                    if decoder.push(&chunk[..n]).is_err() {
                        break;
                    }
                    while let Some(line) = decoder.next_line() {
                        if tx.send(line).await.is_err() {
                            return;
                        }
                    }
                }
                Err(_) => break,
            }
        }
    });
    tokio::spawn(async move {
        let mut writer = write_half;
        let mut rx = outbound_rx;
        while let Some(frame) = rx.recv().await {
            if epoch.is_stale() {
                tracing::debug!(
                    epoch = epoch.mine,
                    "connection superseded; dropping stale frame"
                );
                break;
            }
            let mut line = frame;
            if !line.ends_with('\n') {
                line.push('\n');
            }
            if writer.write_all(line.as_bytes()).await.is_err() || writer.flush().await.is_err() {
                break;
            }
        }
        drop(writer.shutdown().await);
    });
    let mut pump = from_line_channels(inbound_rx, outbound_tx);
    server.serve(&mut pump).await
}

#[cfg(test)]
mod tests;
