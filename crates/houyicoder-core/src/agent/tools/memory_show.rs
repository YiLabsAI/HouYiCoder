//! The structured memory-read tool an agent calls to read one stored memory
//! body in full. The tool routes a key through the provider's read methods,
//! which own the read path, so there is no path-escape surface for the agent
//! to probe and the result is the parsed entry rather than raw file text. A
//! key stored in more than one scope is an error naming the scopes, never a
//! silent newest.
//!
//! Read-only by construction, so the approval gate stays off.

use std::sync::Arc;

use houyicoder_api::memory::MemoryProvider;
use houyicoder_async::PFut;
use houyicoder_context::{MemoryEntry, MemoryScope};
use serde_json::{Value, json};

use super::{Tool, ToolCtx, ToolError};

/// A structured memory-read tool. An agent calls it to read the full body of
/// one memory before deciding whether to merge, update, delete, or act on it;
/// the provider owns the read path.
pub struct ShowMemoryTool {
    provider: Arc<dyn MemoryProvider>,
}

impl ShowMemoryTool {
    /// Construct with a shared provider handle. The provider is shared with
    /// the runner memory so the forked dream reads the same store.
    pub fn new(provider: Arc<dyn MemoryProvider>) -> Self {
        Self { provider }
    }
}

impl Tool for ShowMemoryTool {
    fn name(&self) -> &str {
        "show_memory"
    }
    fn description(&self) -> &str {
        "Read the full body of one stored memory by its key. Returns the key, \
         description, source, mtime, and content. Use it when a memory listing \
         shows a candidate you need to inspect in full before acting on it. \
         Pass scope when the same key is stored in more than one scope."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "key": {
                    "type": "string",
                    "description": "The kebab-case key (file stem) of the memory to read."
                },
                "scope": {
                    "type": "string",
                    "enum": ["user", "project", "auto"],
                    "description": "The scope to read from, when the same key is \
                                    stored in more than one."
                }
            },
            "required": ["key"],
            "additionalProperties": false
        })
    }
    fn execute(&self, _ctx: ToolCtx, input: Value) -> PFut<'_, Result<Value, ToolError>> {
        let provider = Arc::clone(&self.provider);
        Box::pin(async move {
            let key = input
                .get("key")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|k| !k.is_empty())
                .ok_or_else(|| {
                    ToolError::InvalidInput(
                        "show_memory: 'key' must be a non-empty string".to_string(),
                    )
                })?;
            let scope = match input.get("scope") {
                Some(v) => Some(parse_scope(v)?),
                None => None,
            };
            let entry = read_body(&*provider, key, scope)?;
            Ok(json!({
                "key": entry.key,
                "description": entry.description,
                "source": entry.source.as_label(),
                "mtime": entry.mtime_secs,
                "content": entry.content,
            }))
        })
    }
    fn is_read_only(&self) -> bool {
        true
    }
    fn is_destructive(&self) -> bool {
        false
    }
    /// Auto-approve: the tool is read-only, so there is no hard-to-reverse
    /// outward effect to gate. The forked consolidation agent runs
    /// autonomously; a per-call gate would queue approvals no one answers.
    fn requires_approval(&self) -> bool {
        false
    }
}

/// Parse the optional scope argument. An unrecognized label is a caller error
/// rather than a silent default, so a typo cannot read from a scope the caller
/// did not choose.
fn parse_scope(value: &Value) -> Result<MemoryScope, ToolError> {
    let label = value.as_str().ok_or_else(|| {
        ToolError::InvalidInput("show_memory: 'scope' must be a string".to_string())
    })?;
    MemoryScope::from_label(label).ok_or_else(|| {
        ToolError::InvalidInput(format!(
            "show_memory: unknown scope '{label}'; use user, project, or auto"
        ))
    })
}

/// Read the body for a key, resolving the scope question first. A key stored
/// in one scope reads straight through. A key stored in more than one has two
/// different bodies, so the caller must name the scope it wants — picking the
/// newest would hand back a body the caller did not ask for and cannot tell
/// apart from the one it did.
fn read_body(
    provider: &dyn MemoryProvider,
    key: &str,
    scope: Option<MemoryScope>,
) -> Result<MemoryEntry, ToolError> {
    if let Some(scope) = scope {
        return provider.show_memory_in_scope(key, scope).ok_or_else(|| {
            ToolError::Failed(format!(
                "show_memory: no memory with key '{key}' in scope '{}'",
                scope.as_label()
            ))
        });
    }
    let scopes = provider.scopes_for_key(key);
    if scopes.len() > 1 {
        let names: Vec<&str> = scopes.iter().map(|s| s.as_label()).collect();
        return Err(ToolError::InvalidInput(format!(
            "show_memory: '{key}' is stored in {} scopes ({}); pass scope to choose one",
            scopes.len(),
            names.join(", ")
        )));
    }
    provider
        .show_memory(key)
        .ok_or_else(|| ToolError::Failed(format!("show_memory: no memory with key '{key}'")))
}

#[cfg(test)]
mod tests;
