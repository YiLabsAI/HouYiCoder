//! Diagnostics as the tool results render them: the compact block a write's
//! answer carries, ordered by severity and bounded per file, and the decorator
//! that attaches it to an edit or write result.

use std::sync::Arc;

use houyicoder_api::sandbox::SandboxSession;
use houyicoder_api::tool::Tool;
use houyicoder_api::tool::ToolCtx;
use houyicoder_async::PFut;
use houyicoder_protocol::extension::ToolError;
use houyicoder_protocol::lsp::LspDiagnostic;
use serde_json::Value;

use super::LspConnections;
use crate::child_process::file_uri;

/// How many diagnostic lines one block carries. The cap keeps a small edit
/// from flooding the model with the whole project's problems.
const DIAGNOSTIC_LINES: usize = 10;

/// How many characters one diagnostic message keeps on its line.
const MESSAGE_CHARS: usize = 220;

/// The largest file the post-write read pulls in. A write larger than this
/// analysis window is not worth reading back to look for diagnostics.
const MAX_READ_BYTES: usize = 10 * 1024 * 1024;

/// The severity rank: lower sorts first, and a server that named no severity
/// reads as an error (fail closed, like the safety hints).
fn severity_rank(diagnostic: &LspDiagnostic) -> u32 {
    diagnostic.severity.unwrap_or(1)
}

/// The severity word one diagnostic renders.
fn severity_label(diagnostic: &LspDiagnostic) -> &'static str {
    match diagnostic.severity {
        Some(2) => "warning",
        Some(3) => "info",
        Some(4) => "hint",
        _ => "error",
    }
}

/// The whole block one file's diagnostics render, or None when there are
/// none. Lines are ordered by severity, then by position, and each carries
/// the one-based line and column plus the single-line message. Server text
/// is untrusted: markup characters are escaped so a message cannot close the
/// host's frame.
pub fn format_block(file: &str, diagnostics: &[LspDiagnostic]) -> Option<String> {
    if diagnostics.is_empty() {
        return None;
    }
    let mut sorted: Vec<&LspDiagnostic> = diagnostics.iter().collect();
    sorted.sort_by(|a, b| {
        severity_rank(a)
            .cmp(&severity_rank(b))
            .then(a.range.start.line.cmp(&b.range.start.line))
            .then(a.range.start.character.cmp(&b.range.start.character))
    });
    let mut text = format!("<lsp_diagnostics file=\"{}\">\n", escape(file));
    for diagnostic in sorted.iter().take(DIAGNOSTIC_LINES) {
        let flattened = diagnostic
            .message
            .replace("\r\n", " ")
            .replace(['\n', '\r'], " ");
        let message: String = escape(&flattened.chars().take(MESSAGE_CHARS).collect::<String>());
        text.push_str(&format!(
            "  {}[{}:{}] {message}\n",
            severity_label(diagnostic),
            diagnostic.range.start.line + 1,
            diagnostic.range.start.character + 1
        ));
    }
    if sorted.len() > DIAGNOSTIC_LINES {
        text.push_str(&format!(
            "  ({more} more diagnostics not shown)\n",
            more = sorted.len() - DIAGNOSTIC_LINES
        ));
    }
    text.push_str("</lsp_diagnostics>");
    Some(text)
}

/// The checked-clean block: a routed file the server published no
/// diagnostics for reads as checked, not as unknown.
pub fn clean_block(file: &str) -> String {
    format!(
        "<lsp_diagnostics file=\"{}\">no problems</lsp_diagnostics>",
        escape(file)
    )
}

/// The not-yet-checked block: the server owns the file but published
/// nothing inside the wait, so the answer says the check is pending rather
/// than implying the file is clean.
pub fn unknown_block(file: &str) -> String {
    format!(
        "<lsp_diagnostics file=\"{}\">not published yet</lsp_diagnostics>",
        escape(file)
    )
}

/// Escape one untrusted string for inside the host's markup frame.
fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Wraps a writing tool so its result carries the diagnostics the file has
/// after the write. The inner tool runs unchanged; the sync and the short
/// wait happen only when a server routes the written suffix.
pub(crate) struct SaveDiagnosticsTool {
    inner: Arc<dyn Tool>,
    manager: Arc<LspConnections>,
    session: Arc<dyn SandboxSession>,
    /// The read-back window this instance accepts.
    max_read_bytes: usize,
}

impl SaveDiagnosticsTool {
    /// Wrap one writing tool with the post-write diagnostics pass.
    pub(crate) fn new(
        inner: Arc<dyn Tool>,
        manager: Arc<LspConnections>,
        session: Arc<dyn SandboxSession>,
    ) -> Self {
        Self {
            inner,
            manager,
            session,
            max_read_bytes: MAX_READ_BYTES,
        }
    }

    /// The largest file this decorator reads back. Production keeps the
    /// analysis window; a test lowers it so the over-limit branch runs on a
    /// tiny fixture instead of a ten-megabyte file.
    pub(crate) fn with_max_read_bytes(mut self, max: usize) -> Self {
        self.max_read_bytes = max;
        self
    }
}

impl Tool for SaveDiagnosticsTool {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn description(&self) -> &str {
        self.inner.description()
    }

    fn input_schema(&self) -> Value {
        self.inner.input_schema()
    }

    fn is_read_only(&self) -> bool {
        self.inner.is_read_only()
    }

    fn is_destructive(&self) -> bool {
        self.inner.is_destructive()
    }

    fn is_concurrency_safe(&self) -> bool {
        self.inner.is_concurrency_safe()
    }

    fn requires_approval(&self) -> bool {
        self.inner.requires_approval()
    }

    fn requires_approval_for(&self, input: &Value) -> bool {
        self.inner.requires_approval_for(input)
    }

    fn empty_usable_arguments(&self) -> &'static [&'static str] {
        self.inner.empty_usable_arguments()
    }

    fn resolved_input(&self, input: &Value) -> Option<Value> {
        self.inner.resolved_input(input)
    }

    fn execute_authorized(&self, ctx: ToolCtx, input: Value) -> PFut<'_, Result<Value, ToolError>> {
        self.run(ctx, input, true)
    }

    fn execute(&self, ctx: ToolCtx, input: Value) -> PFut<'_, Result<Value, ToolError>> {
        self.run(ctx, input, false)
    }
}

impl SaveDiagnosticsTool {
    /// The one body both entry points run: the inner write (through the
    /// authorized path when the caller already cleared approval), then the
    /// diagnostics pass. Keeping one body means a future caller of either
    /// entry point cannot silently lose the pass.
    fn run(
        &self,
        ctx: ToolCtx,
        input: Value,
        authorized: bool,
    ) -> PFut<'_, Result<Value, ToolError>> {
        let inner = Arc::clone(&self.inner);
        let manager = Arc::clone(&self.manager);
        let session = Arc::clone(&self.session);
        let path_text = input
            .get("path")
            .and_then(Value::as_str)
            .map(str::to_string);
        Box::pin(async move {
            let mut value = if authorized {
                inner.execute_authorized(ctx, input).await?
            } else {
                inner.execute(ctx, input).await?
            };
            let Some(path_text) = path_text else {
                return Ok(value);
            };
            let path = manager.resolve_path(&path_text);
            let Some(server) = manager
                .server_for_path(&path)
                .await
                .filter(|_| value.is_object())
            else {
                return Ok(value);
            };
            let file = path.display().to_string();
            // A routed file whose check could not run still answers with the
            // unknown marker: silence would read as "no server", and the
            // read or sync failure below is a repeatable state the model
            // should be able to see.
            let stalled = |value: &mut Value| {
                if let Some(object) = value.as_object_mut() {
                    object.insert(
                        "lsp_diagnostics".into(),
                        Value::String(unknown_block(&file)),
                    );
                }
            };
            // Read through the sandbox, like every other file access this
            // tool path makes. The extra byte is how an over-limit file is
            // told apart from one that exactly fills the window: syncing a
            // truncated body would leave the server believing the file is
            // shorter than it is.
            let max = self.max_read_bytes;
            let Ok(bytes) = session.read_file(&path.to_string_lossy(), max + 1).await else {
                tracing::warn!(
                    path = %path.display(),
                    "the written file could not be read back for diagnostics"
                );
                stalled(&mut value);
                return Ok(value);
            };
            if bytes.len() > max {
                tracing::warn!(
                    path = %path.display(),
                    max,
                    "the written file is too large to synchronize for diagnostics"
                );
                stalled(&mut value);
                return Ok(value);
            }
            let content = String::from_utf8_lossy(&bytes).into_owned();
            let uri = file_uri(&path);
            let since = manager.publish_generation(&server, &uri).await;
            if let Err(error) = manager.sync_file_saved(&path, &content).await {
                tracing::warn!(%error, server, "the save could not be synchronized");
                stalled(&mut value);
                return Ok(value);
            }
            let block = match manager.wait_for_diagnostics(&server, &uri, since).await {
                Some(diagnostics) => {
                    format_block(&file, &diagnostics).unwrap_or_else(|| clean_block(&file))
                }
                // A server owns the file but published nothing in the window.
                // Saying so keeps "not checked yet" apart from "no server".
                None => unknown_block(&file),
            };
            if let Some(object) = value.as_object_mut() {
                object.insert("lsp_diagnostics".into(), Value::String(block));
            }
            Ok(value)
        })
    }
}

#[cfg(test)]
mod tests;
