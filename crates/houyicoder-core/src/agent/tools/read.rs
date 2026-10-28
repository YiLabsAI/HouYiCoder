//! ReadTool: read a file from the sandbox workspace. Read-only, so no
//! approval is needed. Split from tools.rs so the module root stays under
//! the file-size gate.

use std::sync::Arc;

use houyicoder_api::sandbox::SandboxSession;
use houyicoder_async::PFut;
use serde_json::{Value, json};

use houyicoder_api::tool::{Tool, ToolCtx};
use houyicoder_protocol::extension::ToolError;

use crate::agent::conditional_activation::ConditionalSkillActivator;

/// Read a file from the sandbox workspace. Read-only, no approval.
pub struct ReadTool {
    session: Arc<dyn SandboxSession>,
    activator: Option<Arc<dyn ConditionalSkillActivator>>,
}

impl ReadTool {
    pub fn new(session: Arc<dyn SandboxSession>) -> Self {
        Self {
            session,
            activator: None,
        }
    }
    pub fn with_activator(mut self, activator: Option<Arc<dyn ConditionalSkillActivator>>) -> Self {
        self.activator = activator;
        self
    }
}

impl Tool for ReadTool {
    fn name(&self) -> &str {
        "read"
    }
    fn description(&self) -> &str {
        "Read a file from the sandbox workspace. Input: {path: string, offset?: number, limit?: number, max_bytes?: number}. Returns content (utf-8, sliced by 1-based line offset and line limit, truncated at max_bytes)."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "offset": {"type": "number"},
                "limit": {"type": "number"},
                "max_bytes": {"type": "number"}
            },
            "required": ["path"]
        })
    }
    fn execute(&self, _ctx: ToolCtx, input: Value) -> PFut<'_, Result<Value, ToolError>> {
        let session = self.session.clone();
        let activator = self.activator.clone();
        Box::pin(async move {
            let path = input
                .get("path")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolError::InvalidInput("read: path (string) required".into()))?;
            // Activate paths-gated skills by intent, before the read.
            if let Some(a) = &activator {
                a.activate_for_paths(&[path.to_string()]);
            }
            let max = input
                .get("max_bytes")
                .and_then(|v| v.as_u64())
                .unwrap_or(65_536) as usize;
            // A zero budget truncates to an empty read, which the transcript
            // would mislabel "Read 0 lines (empty file)" for a non-empty file.
            // Reject it so the cause surfaces as an error, not a silent body.
            validate_read_max_bytes(max)?;
            let offset = parse_line_param(&input, "offset")?;
            let limit = match parse_line_param(&input, "limit")? {
                Some(n) => Some(n),
                None => parse_line_param(&input, "context")?,
            };
            validate_read_limit(limit)?;
            let skip = offset.unwrap_or(1).saturating_sub(1);
            let fetch_max = if skip > 0 { max.max(1_048_576) } else { max };
            let bytes = session
                .read_file(path, fetch_max)
                .await
                .map_err(|e| ToolError::Failed(format!("read: {e}")))?;
            let raw = String::from_utf8_lossy(&bytes);
            let (content, truncated) =
                slice_read_window(&raw, bytes.len() == fetch_max, offset, limit, max);
            Ok(json!({
                "path": path,
                "content": content,
                "truncated": truncated,
            }))
        })
    }
    fn is_destructive(&self) -> bool {
        false
    }
    fn is_read_only(&self) -> bool {
        true
    }
    fn requires_approval(&self) -> bool {
        false
    }
}

/// Reject a zero read budget so the tool errors instead of returning an
/// empty body that the transcript would mislabel "Read 0 lines (empty
/// file)" for a non-empty file. Pure so the gate is unit-testable without
/// a sandbox session.
fn validate_read_max_bytes(max: usize) -> Result<(), ToolError> {
    if max == 0 {
        return Err(ToolError::InvalidInput(
            "read: max_bytes must be greater than 0".into(),
        ));
    }
    Ok(())
}

/// Parse an optional line-window parameter from a number or numeric string.
fn parse_line_param(input: &Value, key: &str) -> Result<Option<usize>, ToolError> {
    let Some(val) = input.get(key) else {
        return Ok(None);
    };
    match val {
        Value::Null => Ok(None),
        Value::Number(n) => n
            .as_u64()
            .map(|v| Some(v as usize))
            .ok_or_else(|| ToolError::InvalidInput(format!("read: {key} must be non-negative"))),
        Value::String(s) => s
            .trim()
            .parse::<usize>()
            .map(Some)
            .map_err(|_| ToolError::InvalidInput(format!("read: {key} must be an integer"))),
        _ => Err(ToolError::InvalidInput(format!(
            "read: {key} must be a number"
        ))),
    }
}

/// Reject a zero line limit so the caller never receives a silent empty slice.
fn validate_read_limit(limit: Option<usize>) -> Result<(), ToolError> {
    if limit == Some(0) {
        return Err(ToolError::InvalidInput(
            "read: limit must be greater than 0".into(),
        ));
    }
    Ok(())
}

/// Slice the decoded file text by 1-based line offset and line limit, then
/// enforce the byte ceiling on a character boundary.
fn slice_read_window(
    raw: &str,
    fetch_truncated: bool,
    offset: Option<usize>,
    limit: Option<usize>,
    max_bytes: usize,
) -> (String, bool) {
    if offset.is_none() && limit.is_none() {
        return (raw.to_string(), fetch_truncated);
    }
    let skip = offset.unwrap_or(1).saturating_sub(1);
    let mut iter = raw.split_inclusive('\n').skip(skip);
    let mut out = String::new();
    let mut more_lines = false;
    match limit {
        Some(n) => {
            for line in (&mut iter).take(n) {
                out.push_str(line);
            }
            if iter.next().is_some() {
                more_lines = true;
            }
        }
        None => {
            for line in iter {
                out.push_str(line);
            }
        }
    }
    if out.len() > max_bytes {
        out.truncate(out.floor_char_boundary(max_bytes));
        return (out, true);
    }
    (out, fetch_truncated || more_lines)
}

#[cfg(test)]
mod tests;
