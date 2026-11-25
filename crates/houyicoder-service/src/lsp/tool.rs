//! The lsp tool: one read-only code-intelligence query against the server
//! that owns the file's suffix. Positions cross the boundary in the one
//! based spelling a person uses and are converted to the protocol's zero
//! based UTF-16 coordinates here, because that conversion is the most common
//! place an integration silently lands a caret in the wrong column. Answers
//! report one-based lines and UTF-16 columns, the spelling editors show.

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use houyicoder_api::sandbox::SandboxSession;
use houyicoder_api::tool::Tool;
use houyicoder_api::tool::ToolCtx;
use houyicoder_async::PFut;
use houyicoder_protocol::extension::ToolError;
use houyicoder_protocol::lsp::LspCallHierarchyItem;
use houyicoder_protocol::lsp::LspDocumentSymbol;
use houyicoder_protocol::lsp::LspIncomingCall;
use houyicoder_protocol::lsp::LspOperation;
use houyicoder_protocol::lsp::LspOutgoingCall;
use houyicoder_protocol::lsp::LspPosition;
use houyicoder_protocol::lsp::LspSymbolInformation;
use serde_json::Value;
use serde_json::json;

use self::position::identifier_at;
use self::position::positive_coordinate;
use self::position::utf16_position;
use self::render::NoiseFilter;
use self::render::hierarchy_items_text;
use self::render::incoming_calls_text;
use self::render::locations_text;
use self::render::outgoing_calls_text;
use self::render::symbol_list_text;
use super::LspConnections;
use crate::child_process::file_uri;

mod position;
mod render;

/// The largest file the tool reads. A language server query on a bigger
/// file is not worth the read it would cost.
const MAX_FILE_BYTES: usize = 10 * 1024 * 1024;

/// The largest rendered answer; a symbol search across a big workspace is
/// cut here with a note rather than flooding the model's context.
const MAX_RESULT_CHARS: usize = 64 * 1024;

/// The read-only code-intelligence tool.
pub(crate) struct LspTool {
    manager: Arc<LspConnections>,
    session: Arc<dyn SandboxSession>,
    noise: NoiseFilter,
}

impl LspTool {
    /// A tool over one manager and the sandbox its files are read through.
    pub(crate) fn new(manager: Arc<LspConnections>, session: Arc<dyn SandboxSession>) -> Self {
        let noise = NoiseFilter::new(session.workspace_root().as_ref());
        Self {
            manager,
            session,
            noise,
        }
    }
}

impl Tool for LspTool {
    fn name(&self) -> &str {
        "lsp"
    }

    fn description(&self) -> &str {
        "Query the language server that owns a file: definitions, references, hover, symbols, and call hierarchies. Positions are one-based."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "operation": {
                    "type": "string",
                    "enum": [
                        "goToDefinition", "findReferences", "hover", "documentSymbol",
                        "workspaceSymbol", "goToImplementation", "prepareCallHierarchy",
                        "incomingCalls", "outgoingCalls"
                    ],
                    "description": "The query to run."
                },
                "filePath": {"type": "string", "description": "The file the query targets."},
                "line": {"type": "integer", "minimum": 1, "description": "One-based line."},
                "character": {"type": "integer", "minimum": 1, "description": "One-based character, counted in code points; answers report columns in UTF-16 code units, as editors do."},
                "query": {
                    "type": "string",
                    "description": "Optional symbol query for workspaceSymbol; omitted, the identifier under the position is used."
                }
            },
            "required": ["operation", "filePath", "line", "character"]
        })
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn is_destructive(&self) -> bool {
        false
    }

    fn is_concurrency_safe(&self) -> bool {
        true
    }

    fn execute(&self, ctx: ToolCtx, input: Value) -> PFut<'_, Result<Value, ToolError>> {
        let manager = Arc::clone(&self.manager);
        let session = Arc::clone(&self.session);
        let noise = self.noise.clone();
        let cancel = ctx.cancel.clone();
        Box::pin(async move {
            let work = async {
                let request = Request::parse(&input)?;
                let path = reject_network_path(
                    manager.resolve_path(&request.file_path),
                    &request.file_path,
                )?;
                let bytes = session
                    .read_file(&path.to_string_lossy(), MAX_FILE_BYTES + 1)
                    .await
                    .map_err(|error| ToolError::Failed(format!("lsp: {error}")))?;
                if bytes.len() > MAX_FILE_BYTES {
                    return Err(ToolError::Failed(format!(
                        "lsp: {} exceeds the {MAX_FILE_BYTES} byte analysis limit",
                        path.display()
                    )));
                }
                let content = String::from_utf8_lossy(&bytes).into_owned();
                let Some(server) = manager
                    .ensure_document(&path, &content)
                    .await
                    .map_err(ToolError::Failed)?
                else {
                    return Err(ToolError::Failed(format!(
                        "lsp: no language server routes {}",
                        path.display()
                    )));
                };
                let position = utf16_position(&content, request.line, request.character)?;
                let text = dispatch(
                    &manager, &request, &path, position, &content, &server, &noise,
                )
                .await?;
                let text = truncated_answer(&text);
                Ok(json!({
                    "operation": request.operation_name,
                    "result": text,
                    "filePath": request.file_path,
                }))
            };
            match cancel {
                Some(token) => tokio::select! {
                    outcome = work => outcome,
                    _ = token.cancelled() => Err(ToolError::Failed(
                        "lsp: the query was cancelled".into(),
                    )),
                },
                None => work.await,
            }
        })
    }
}

/// One parsed tool call.
struct Request {
    operation: LspOperation,
    operation_name: String,
    file_path: String,
    line: u32,
    character: u32,
    query: Option<String>,
}

impl Request {
    fn parse(input: &Value) -> Result<Self, ToolError> {
        let invalid = |what: &str| ToolError::InvalidInput(format!("lsp: {what} required"));
        let operation_name = input
            .get("operation")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("operation (string)"))?
            .to_string();
        let operation = LspOperation::parse(&operation_name).ok_or_else(|| {
            ToolError::InvalidInput(format!("lsp: unknown operation {operation_name}"))
        })?;
        let file_path = input
            .get("filePath")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("filePath (string)"))?
            .to_string();
        let line = positive_coordinate(input, "line")?;
        let character = positive_coordinate(input, "character")?;
        Ok(Self {
            operation,
            operation_name,
            file_path,
            line,
            character,
            query: input
                .get("query")
                .and_then(Value::as_str)
                .map(str::to_string),
        })
    }
}

/// The target path, refusing the network spellings that would leak
/// credentials on some hosts. Whether the path reads as a file is left to
/// the read itself, whose failure names the sandbox's own reason.
fn reject_network_path(path: PathBuf, spelled: &str) -> Result<PathBuf, ToolError> {
    if spelled.starts_with(r"\\") || spelled.starts_with("//") {
        return Err(ToolError::InvalidInput(
            "lsp: network paths are not analyzed".into(),
        ));
    }
    Ok(path)
}

/// Run one operation and render its answer.
async fn dispatch(
    manager: &Arc<LspConnections>,
    request: &Request,
    path: &Path,
    position: LspPosition,
    content: &str,
    server: &str,
    noise: &NoiseFilter,
) -> Result<String, ToolError> {
    let uri = file_uri(path);
    let partial = manager.indexing_after_wait(server).await;
    let mut answer = match request.operation {
        LspOperation::DocumentSymbol => {
            let params = json!({"textDocument": {"uri": uri}});
            let value = query(
                manager,
                path,
                "textDocument/documentSymbol",
                params,
                content,
            )
            .await?;
            // A server may answer with the hierarchical outline or with the flat
            // symbol shape; both name the file's symbols, so either decodes.
            match serde_json::from_value::<Vec<LspDocumentSymbol>>(value.clone()) {
                Ok(symbols) => symbols_text(&symbols, 0),
                Err(_) => {
                    let symbols: Vec<LspSymbolInformation> =
                        serde_json::from_value(value).map_err(decode_error)?;
                    symbol_list_text(&symbols, noise)
                }
            }
        }
        LspOperation::WorkspaceSymbol => {
            let query_text = match request.query.clone() {
                Some(query) => query,
                None => {
                    identifier_at(content, request.line, request.character).ok_or_else(|| {
                        ToolError::InvalidInput(
                            "lsp: no identifier under the position; pass query".into(),
                        )
                    })?
                }
            };
            let value = manager
                .request_for_file(
                    path,
                    "workspace/symbol",
                    json!({"query": query_text}),
                    None,
                    budget(),
                )
                .await
                .map_err(ToolError::Failed)?;
            let symbols: Vec<LspSymbolInformation> =
                serde_json::from_value(value).map_err(decode_error)?;
            symbol_list_text(&symbols, noise)
        }
        LspOperation::PrepareCallHierarchy => {
            let value = query(
                manager,
                path,
                "textDocument/prepareCallHierarchy",
                position_params(&uri, position),
                content,
            )
            .await?;
            let items: Vec<LspCallHierarchyItem> =
                serde_json::from_value(value).map_err(decode_error)?;
            if items.is_empty() {
                "no call hierarchy item found at this position".to_string()
            } else {
                hierarchy_items_text(&items, noise)
            }
        }
        LspOperation::IncomingCalls | LspOperation::OutgoingCalls => {
            call_hierarchy_text(manager, request, path, position, &uri, content, noise).await?
        }
        operation => {
            let Some((method, params)) = position_query(operation, &uri, position) else {
                return Err(ToolError::Failed(format!(
                    "lsp: {operation:?} is not a position query"
                )));
            };
            let value = query(manager, path, method, params, content).await?;
            match operation {
                LspOperation::Hover => hover_text(&value),
                _ => locations_text(&value, noise),
            }
        }
    };
    if partial {
        answer.insert_str(
            0,
            "[note: language server is still indexing; results may be partial]\n",
        );
    }
    if answer.is_empty() {
        answer.push_str("no result");
    }
    Ok(answer)
}

/// One request whose document is already open, so no content travels.
async fn query(
    manager: &Arc<LspConnections>,
    path: &Path,
    method: &str,
    params: Value,
    content: &str,
) -> Result<Value, ToolError> {
    manager
        .request_for_file(path, method, params, Some(content), budget())
        .await
        .map_err(ToolError::Failed)
}

/// The per-request budget the tool allows a server.
fn budget() -> Duration {
    Duration::from_secs(10)
}

/// The params every position-based request shares.
fn position_params(uri: &str, position: LspPosition) -> Value {
    json!({"textDocument": {"uri": uri}, "position": position})
}

/// The two-step call-hierarchy flow: resolve the item under the position,
/// then ask for the calls.
async fn call_hierarchy_text(
    manager: &Arc<LspConnections>,
    request: &Request,
    path: &Path,
    position: LspPosition,
    uri: &str,
    content: &str,
    noise: &NoiseFilter,
) -> Result<String, ToolError> {
    let prepare = query(
        manager,
        path,
        "textDocument/prepareCallHierarchy",
        position_params(uri, position),
        content,
    )
    .await?;
    let items: Vec<LspCallHierarchyItem> = serde_json::from_value(prepare).map_err(decode_error)?;
    let Some(first) = items.into_iter().next() else {
        return Ok("no call hierarchy item found at this position".to_string());
    };
    if request.operation == LspOperation::IncomingCalls {
        let value = manager
            .request_for_file(
                path,
                "callHierarchy/incomingCalls",
                json!({"item": first}),
                None,
                budget(),
            )
            .await
            .map_err(ToolError::Failed)?;
        let calls: Vec<LspIncomingCall> = serde_json::from_value(value).map_err(decode_error)?;
        Ok(incoming_calls_text(&calls, noise))
    } else {
        let value = manager
            .request_for_file(
                path,
                "callHierarchy/outgoingCalls",
                json!({"item": first}),
                None,
                budget(),
            )
            .await
            .map_err(ToolError::Failed)?;
        let calls: Vec<LspOutgoingCall> = serde_json::from_value(value).map_err(decode_error)?;
        Ok(outgoing_calls_text(&calls, noise))
    }
}

/// The method and params for one position-based operation. Exhaustive on
/// purpose: a new operation must name its method here instead of silently
/// falling back to a definition query.
fn position_query(
    operation: LspOperation,
    uri: &str,
    position: LspPosition,
) -> Option<(&'static str, Value)> {
    Some(match operation {
        LspOperation::GoToDefinition => ("textDocument/definition", position_params(uri, position)),
        LspOperation::FindReferences => (
            "textDocument/references",
            json!({"textDocument": {"uri": uri}, "position": position,
                "context": {"includeDeclaration": true}}),
        ),
        LspOperation::Hover => ("textDocument/hover", position_params(uri, position)),
        LspOperation::GoToImplementation => (
            "textDocument/implementation",
            position_params(uri, position),
        ),
        LspOperation::DocumentSymbol
        | LspOperation::WorkspaceSymbol
        | LspOperation::PrepareCallHierarchy
        | LspOperation::IncomingCalls
        | LspOperation::OutgoingCalls => return None,
    })
}

/// A decode failure names the crossing, so the model sees which layer broke.
fn decode_error(error: serde_json::Error) -> ToolError {
    ToolError::Failed(format!(
        "lsp: the server's answer did not fit the protocol shape: {error}"
    ))
}

/// The hover answer's text, joining the content blocks.
fn hover_text(value: &Value) -> String {
    let Some(contents) = value.get("contents") else {
        return String::new();
    };
    let mut parts = Vec::new();
    collect_markup(contents, &mut parts);
    parts.join("\n")
}

/// Flatten the three hover content shapes into strings.
fn collect_markup(value: &Value, parts: &mut Vec<String>) {
    match value {
        Value::String(text) => parts.push(text.clone()),
        Value::Array(items) => {
            for item in items {
                collect_markup(item, parts);
            }
        }
        Value::Object(map) => {
            if let Some(text) = map.get("value").and_then(Value::as_str) {
                parts.push(text.to_string());
            }
        }
        _ => {}
    }
}

/// The outline of one document, nesting children under their owner.
fn symbols_text(symbols: &[LspDocumentSymbol], depth: usize) -> String {
    let indent = "  ".repeat(depth);
    let mut lines = Vec::new();
    for symbol in symbols {
        let mut line = format!(
            "{indent}{} (line {})",
            symbol.name,
            symbol.range.start.line + 1
        );
        if let Some(detail) = &symbol.detail
            && !detail.is_empty()
        {
            line.push_str(&format!(": {detail}"));
        }
        lines.push(line);
        if !symbol.children.is_empty() {
            lines.push(symbols_text(&symbol.children, depth + 1));
        }
    }
    lines.join("\n")
}

/// Bound the rendered answer, so a broad search cannot flood the model's
/// context.
fn truncated_answer(text: &str) -> String {
    if text.chars().count() > MAX_RESULT_CHARS {
        let cut: String = text.chars().take(MAX_RESULT_CHARS).collect();
        return format!("{cut}\n[truncated at {MAX_RESULT_CHARS} characters]");
    }
    text.to_string()
}

#[cfg(test)]
mod tests;
