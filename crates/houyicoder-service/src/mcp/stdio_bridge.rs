//! The byte bridge between a launched child's blocking pipes and the
//! asynchronous transport the protocol client speaks, plus the bounded tail
//! of the child's diagnostic stream.
//!
//! The launcher hands back operating-system pipes, which block when read.
//! A thread per direction moves bytes between those pipes and an in-memory
//! channel; two small tasks move the same bytes between the channels and the
//! duplex streams the client reads and writes asynchronously. The stderr
//! pipe gets its own thread and a fixed ring, so a child that writes
//! diagnostics it cannot flush never blocks on a full pipe and its output
//! can never grow without bound.

use std::collections::VecDeque;
use std::io::Read;
use std::io::Write;
use std::sync::Arc;
use std::sync::Mutex;
use std::thread::spawn;

use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::DuplexStream;
use tokio::io::duplex;
use tokio::sync::mpsc;

/// The in-memory buffer between the client and the pump tasks. One chunk of
/// a child's output fits without the writer waiting on a scheduler turn.
const BRIDGE_BUFFER: usize = 64 * 1024;

/// How many chunks may queue between a thread and its pump task. The bound
/// is what keeps a chatty child from growing memory without limit.
const BRIDGE_DEPTH: usize = 16;

/// How many diagnostic lines the tail keeps. Enough to see the reason a
/// server failed, bounded so a chatty child cannot grow memory.
pub const STDERR_TAIL_LINES: usize = 32;

/// One line longer than this is cut: a single unbounded line is how a
/// crash dump would otherwise escape the line cap.
pub const STDERR_LINE_CHARS: usize = 512;

/// The blocking pipes of one launched child, moved onto threads once.
pub struct Pipes {
    pub stdin: Box<dyn Write + Send>,
    pub stdout: Box<dyn Read + Send>,
    pub stderr: Option<Box<dyn Read + Send>>,
}

/// The asynchronous halves the protocol client reads and writes.
pub struct StdioBridge {
    /// What the client reads: the child's standard output.
    pub reader: DuplexStream,
    /// What the client writes: the child's standard input.
    pub writer: DuplexStream,
}

/// The most recent lines a child wrote to its standard error.
#[derive(Clone, Default)]
pub struct StderrTail {
    lines: Arc<Mutex<VecDeque<String>>>,
}

impl StderrTail {
    /// The lines held, oldest first.
    pub fn lines(&self) -> Vec<String> {
        self.lines
            .lock()
            .map(|lines| lines.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Append one line, cutting it at the character cap and evicting the
    /// oldest once the hold is full.
    fn push(&self, mut line: String) {
        if line.chars().count() > STDERR_LINE_CHARS {
            line = line.chars().take(STDERR_LINE_CHARS).collect();
        }
        if let Ok(mut lines) = self.lines.lock() {
            if lines.len() == STDERR_TAIL_LINES {
                lines.pop_front();
            }
            lines.push_back(line);
        }
    }
}

/// Start the bridge. Must be called from an async context, because the pump
/// tasks it starts ride the current runtime.
///
/// The returned bridge is what the transport reads and writes. Dropping its
/// write half closes the child's stdin through the writer thread, the same
/// way a closed terminal would.
pub fn start(pipes: Pipes) -> (StdioBridge, StderrTail) {
    let (out_tx, mut out_rx) = mpsc::channel::<Vec<u8>>(BRIDGE_DEPTH);
    let (client_read, mut pump_write) = duplex(BRIDGE_BUFFER);
    tokio::spawn(async move {
        while let Some(chunk) = out_rx.recv().await {
            if pump_write.write_all(&chunk).await.is_err() {
                break;
            }
        }
        // The half drops here, so the client reads end of stream.
    });

    // Child stdout to the pump. The thread ends when the child closes its
    // output, which drops the sender and lets the pump finish.
    let mut stdout = pipes.stdout;
    spawn(move || {
        let mut chunk = [0u8; 8192];
        loop {
            match stdout.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if out_tx.blocking_send(chunk[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    let (client_write, mut pump_read) = duplex(BRIDGE_BUFFER);
    let (in_tx, mut in_rx) = mpsc::channel::<Vec<u8>>(BRIDGE_DEPTH);
    tokio::spawn(async move {
        let mut buf = [0u8; 8192];
        loop {
            match pump_read.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if in_tx.send(buf[..n].to_vec()).await.is_err() {
                        break;
                    }
                }
            }
        }
    });

    // The pump's bytes to the child's stdin. The thread ends when the client
    // drops its half, and the pipe closes with it.
    let mut stdin = pipes.stdin;
    spawn(move || {
        while let Some(chunk) = in_rx.blocking_recv() {
            if stdin.write_all(&chunk).is_err() || stdin.flush().is_err() {
                break;
            }
        }
    });

    let tail = StderrTail::default();
    if let Some(mut stderr) = pipes.stderr {
        let sink = tail.clone();
        spawn(move || {
            let mut chunk = [0u8; 4096];
            let mut pending = String::new();
            loop {
                match stderr.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        pending.push_str(&String::from_utf8_lossy(&chunk[..n]));
                        while let Some(at) = pending.find('\n') {
                            let line: String = pending.drain(..=at).collect();
                            sink.push(line.trim_end().to_string());
                        }
                        // A very long partial line still counts once it
                        // reaches the cap, so a binary dump cannot grow the
                        // pending string without bound.
                        if pending.chars().count() > STDERR_LINE_CHARS * 4 {
                            let kept: String = pending.chars().take(STDERR_LINE_CHARS).collect();
                            sink.push(kept);
                            pending.clear();
                        }
                    }
                }
            }
            if !pending.is_empty() {
                sink.push(pending.trim_end().to_string());
            }
        });
    }

    (
        StdioBridge {
            reader: client_read,
            writer: client_write,
        },
        tail,
    )
}

#[cfg(test)]
mod tests;
