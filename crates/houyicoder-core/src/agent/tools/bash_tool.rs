//! BashTool: run a shell command in the sandbox. Destructive commands
//! snapshot the writable roots first (so /undo can revert); the output is
//! bounded so a runaway command cannot overflow the model context (the
//! full output spills to a temp file past the cap). Split from tools.rs so
//! that file stays under the file-size gate.

use std::sync::Arc;

use houyicoder_api::sandbox::SandboxSession;
use houyicoder_async::PFut;
use serde_json::{Value, json};

use houyicoder_api::tool::{Tool, ToolCtx};
use houyicoder_protocol::extension::ToolError;

use super::bash_snapshot;

/// Run a shell command in the sandbox. When wired with undo, destructive
/// commands snapshot the workspace before executing so /undo can revert.
pub struct BashTool {
    session: Arc<dyn SandboxSession>,
    undo_stack: Option<Arc<std::sync::Mutex<crate::snapshot::UndoStack>>>,
    snapshot_store: Option<Arc<crate::snapshot::SnapshotStore>>,
}

impl BashTool {
    pub fn new(session: Arc<dyn SandboxSession>) -> Self {
        Self {
            session,
            undo_stack: None,
            snapshot_store: None,
        }
    }

    /// Wire the undo hook: snapshot before destructive exec, push to undo stack.
    pub fn with_undo(
        session: Arc<dyn SandboxSession>,
        undo_stack: Arc<std::sync::Mutex<crate::snapshot::UndoStack>>,
        snapshot_store: Arc<crate::snapshot::SnapshotStore>,
    ) -> Self {
        Self {
            session,
            undo_stack: Some(undo_stack),
            snapshot_store: Some(snapshot_store),
        }
    }
}

impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }
    fn description(&self) -> &str {
        "Run a shell command in the sandbox workspace. \
         Input: {command: string, workdir?: string}. \
         Returns stdout/stderr/exit_code. \
         When issuing multiple commands: if they are independent, make \
         multiple Bash calls in a single message so they run in parallel \
         (example: run git status and git diff as two calls in one message, \
         not one compound call). If the commands depend on each other, use a \
         single call with && to chain them. Use ; only when running \
         sequentially without caring if earlier commands fail. Pipe output \
         with | to pass it between commands (find . -name '*.rs' | xargs \
         wc -l | sort -n). Do not use newlines to separate commands."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {"type": "string"},
                "workdir": {"type": "string"}
            },
            "required": ["command"]
        })
    }
    fn execute(&self, ctx: ToolCtx, input: Value) -> PFut<'_, Result<Value, ToolError>> {
        let session = self.session.clone();
        let undo_stack = self.undo_stack.clone();
        let snapshot_store = self.snapshot_store.clone();
        // Clone the progress sink so a spawned ticker can report elapsed
        // seconds to the host while exec runs. None for non-interactive runs
        // (the ticker then no-ops). The sink ticks every ~1s; the host
        // renders (Ns) on the chip after 2s.
        let progress = ctx.progress.clone();
        Box::pin(async move {
            let cmd = input
                .get("command")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolError::InvalidInput("bash: command (string) required".into()))?;
            // Snapshot a destructive command's writable roots before running
            // it, so /undo can revert the write. The outcome is split by what
            // it means, not just Ok/Err, so a performance skip (no copy-on-
            // write + workspace over the slow-copy threshold) does not masquer-
            // ade as a safety refusal and a real I/O failure does not slip
            // through as a skip. The gate asked, the user said yes on the
            // premise that undo exists; a snapshot failure removes that
            // premise, so a real failure refuses to run. A decline returns a
            // notice (undo skipped for speed) that is folded into stderr so
            // it reaches the transcript and the user-facing render rather than
            // a process stderr the TUI never shows. See bash_snapshot::prepare.
            let mut notice: Option<String> = None;
            if let (Some(store), Some(stack)) = (&snapshot_store, &undo_stack)
                && crate::snapshot::is_destructive_command(cmd)
            {
                notice = bash_snapshot::prepare(store, stack, &*session)?;
            }
            // Run the command, ticking progress every second so a
            // long-running command shows it is not stuck. The line counter
            // (-1 sentinel) is shared with the sandbox exec: a streaming
            // backend (mac) updates it as stdout newlines arrive, so the chip
            // renders (Ns · M lines); a non-streaming backend leaves it at
            // -1, so the tick reports lines=None and the chip shows (Ns)
            // only. The tick loop is dropped (cancelled) the moment exec
            // completes — no leak. Only spawned when a progress sink is
            // wired (interactive runs): tests + non-interactive runs have no
            // sink and skip it (no tokio time driver needed). The tick arm
            // never completes on its own (infinite loop), so the unreachable
            // is a guard, not a path.
            let lines_counter = std::sync::Arc::new(std::sync::atomic::AtomicI64::new(-1));
            let exec = session.exec_streaming(cmd, std::sync::Arc::clone(&lines_counter));
            let result = if progress.is_some() {
                tokio::select! {
                    r = exec => r,
                    _ = async {
                        let start = std::time::Instant::now();
                        loop {
                            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                            if let Some(p) = &progress {
                                let lines = lines_counter.load(std::sync::atomic::Ordering::Relaxed);
                                let lines_opt = if lines >= 0 {
                                    Some(lines as u64)
                                } else {
                                    None
                                };
                                p.progress(start.elapsed().as_secs(), lines_opt);
                            }
                        }
                    } => unreachable!("bash progress tick loop must not return"),
                }
            } else {
                exec.await
            }
            .map_err(|e| ToolError::Failed(format!("bash: {e}")))?;
            // Bound the output before it enters the model context so a huge
            // result cannot overflow the window and brick the session (the
            // reference harness spills to disk + returns a tail stub). The
            // full output is re-readable via the spill path in the marker.
            let exit_code = result.exit_code;
            let success = result.is_success();
            let (stdout, stderr) = bound_bash_output(result.stdout, result.stderr);
            // Fold a snapshot-decline notice into stderr so the user sees it
            // inline with the command's own output (it is a per-command
            // notice about undo, not the command's own error, but stderr is
            // the one channel the render path already surfaces verbatim).
            let stderr = match notice {
                Some(n) if stderr.is_empty() => n,
                Some(n) => format!("{n}\n{stderr}"),
                None => stderr,
            };
            Ok(json!({
                "stdout": stdout,
                "stderr": stderr,
                "exit_code": exit_code,
                "success": success,
            }))
        })
    }
    fn is_destructive(&self) -> bool {
        true
    }
    fn is_read_only(&self) -> bool {
        false
    }
    fn requires_approval(&self) -> bool {
        true
    }
}

/// Max chars a bash result may enter the model context before it spills to a
/// temp file. A runaway command (find on a huge tree, a verbose build) can
/// emit megabytes; without a bound the next turn overflows the context
/// window and the provider rejects with HTTP 400, bricking the session.
///
/// Bounds at 30K chars (~7.5K tokens, safe for any model window) since
/// the compaction runtime is not landed yet; a compaction pass would
/// otherwise keep accumulated results under the window. Relax toward 8MB
/// once the compaction runtime lands.
const BASH_MAX_OUTPUT_CHARS: usize = 30_000;

/// Max chars kept in the stub when the output spills to disk. Char-based
/// (not line) so a no-newline megabyte blob still yields a bounded stub; the
/// tail carries the exit/error context the model most needs, and the full
/// output is re-readable via the spill path in the marker.
const BASH_STUB_CHARS: usize = 5_000;

/// Bound a bash result before it enters the model context. Under the cap the
/// output passes through verbatim. Over the cap the full stdout + stderr
/// spill to a temp file (stderr lines tagged) and the model gets the tail
/// plus a marker naming the spill path so it can re-read the full output.
fn bound_bash_output(stdout: String, stderr: String) -> (String, String) {
    let combined = stdout.len() + stderr.len();
    if combined <= BASH_MAX_OUTPUT_CHARS {
        return (stdout, stderr);
    }
    let kb = combined / 1024;
    match spill_bash_output(&stdout, &stderr) {
        Ok(path) => {
            let tail = tail_chars(&stdout, BASH_STUB_CHARS);
            let stub = format!(
                "{tail}\n\nOutput truncated ({kb}KB total). Full output saved to: {}\n",
                path.display()
            );
            // stderr is folded into the spill file (tagged); the model gets
            // the stdout stub + marker only.
            (stub, String::new())
        }
        Err(_) => {
            // Spill failed (temp dir unwritable): fall back to a head+tail
            // in-memory stub so the result still fits the context budget.
            let half = BASH_MAX_OUTPUT_CHARS / 2;
            let head: String = stdout.chars().take(half).collect();
            let tail_start = stdout.chars().count().saturating_sub(half);
            let tail: String = stdout.chars().skip(tail_start).collect();
            (
                format!("{head}\n... [output truncated, {kb}KB total, spill failed] ...\n{tail}"),
                stderr,
            )
        }
    }
}

/// Write the full stdout + stderr to a temp file, stderr lines prefixed so
/// the spill is self-describing. Returns the spill path for the marker.
fn spill_bash_output(stdout: &str, stderr: &str) -> std::io::Result<std::path::PathBuf> {
    use std::io::Write;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path = std::env::temp_dir().join(format!("houyi-bash-{nanos}.log"));
    let mut f = std::fs::File::create(&path)?;
    f.write_all(stdout.as_bytes())?;
    if !stderr.is_empty() {
        for line in stderr.split('\n') {
            writeln!(f, "[stderr] {line}")?;
        }
    }
    Ok(path)
}

/// The last N chars of a string, in original order. Char-based (not line)
/// so a no-newline megabyte blob still yields a bounded stub.
fn tail_chars(s: &str, n: usize) -> String {
    let count = s.chars().count();
    if count <= n {
        return s.to_string();
    }
    s.chars().skip(count - n).collect()
}

#[cfg(test)]
mod bash_bound_tests {
    use super::*;

    #[test]
    fn test_bound_under_cap_passes() {
        let (o, e) = bound_bash_output("hello".into(), "world".into());
        assert_eq!(o, "hello");
        assert_eq!(e, "world");
    }

    #[test]
    fn test_bound_over_cap_spills() {
        let big = "a".repeat(BASH_MAX_OUTPUT_CHARS + 1);
        let (o, e) = bound_bash_output(big.clone(), String::new());
        assert!(o.contains("Output truncated"), "marker missing: {o}");
        assert!(
            o.contains("Full output saved to"),
            "spill path missing: {o}"
        );
        assert_eq!(e, "", "stderr should fold into spill");
        // stub is far smaller than the original
        assert!(o.len() < big.len() / 2);
    }

    #[test]
    fn test_tail_chars_no_newline() {
        let big = "a".repeat(10_000);
        assert_eq!(tail_chars(&big, 100).len(), 100);
        assert_eq!(tail_chars(&big, 20_000).len(), 10_000);
        assert_eq!(tail_chars(&big, 100).chars().next(), Some('a'));
    }
}
