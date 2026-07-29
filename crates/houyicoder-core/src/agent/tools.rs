//! Concrete tools backed by a SandboxSession. BashTool runs shell; Read/Write
//! use the session's file methods. Destructive tools declare
//! requires_approval so the loop gates them behind a human decision.

use std::sync::Arc;

use houyicoder_api::sandbox::SandboxSession;
use houyicoder_async::PFut;
use serde_json::{Value, json};

use houyicoder_api::tool::{Tool, ToolCtx};
use houyicoder_protocol::extension::ToolError;

pub mod ask_user_question;
pub mod bash_snapshot;
pub mod bash_tool;
pub mod conversation_search;
pub mod glob;
pub mod grep;
pub mod memory_add;
pub mod memory_delete;
pub mod memory_promote_demote;
pub mod memory_show;
pub mod path_util;
pub mod subprocess_util;
pub mod todo;
pub mod webfetch;
pub mod worktree_enter;
pub mod worktree_exit;

pub use bash_tool::BashTool;

/// Read a file from the sandbox workspace. Read-only ⇒ no approval.
pub struct ReadTool {
    session: Arc<dyn SandboxSession>,
}

impl ReadTool {
    pub fn new(session: Arc<dyn SandboxSession>) -> Self {
        Self { session }
    }
}

impl Tool for ReadTool {
    fn name(&self) -> &str {
        "read"
    }
    fn description(&self) -> &str {
        "Read a file from the sandbox workspace. Input: {path: string, max_bytes?: number}. Returns content (utf-8, truncated at max_bytes)."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "max_bytes": {"type": "number"}
            },
            "required": ["path"]
        })
    }
    fn execute(&self, _ctx: ToolCtx, input: Value) -> PFut<'_, Result<Value, ToolError>> {
        let session = self.session.clone();
        Box::pin(async move {
            let path = input
                .get("path")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolError::InvalidInput("read: path (string) required".into()))?;
            let max = input
                .get("max_bytes")
                .and_then(|v| v.as_u64())
                .unwrap_or(65_536) as usize;
            // A zero budget truncates to an empty read, which the transcript
            // would mislabel "Read 0 lines (empty file)" for a non-empty file.
            // Reject it so the cause surfaces as an error, not a silent body.
            validate_read_max_bytes(max)?;
            let bytes = session
                .read_file(path, max)
                .await
                .map_err(|e| ToolError::Failed(format!("read: {e}")))?;
            Ok(json!({
                "path": path,
                "content": String::from_utf8_lossy(&bytes),
                "truncated": bytes.len() == max,
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

/// Reject a zero read budget so the tool errors instead of returning an empty
/// body that the transcript would mislabel "Read 0 lines (empty file)" for a
/// non-empty file. Pure so the gate is unit-testable without a sandbox session.
fn validate_read_max_bytes(max: usize) -> Result<(), ToolError> {
    if max == 0 {
        return Err(ToolError::InvalidInput(
            "read: max_bytes must be greater than 0".into(),
        ));
    }
    Ok(())
}

/// Write a file in the sandbox workspace. Destructive ⇒ requires approval.
pub struct WriteTool {
    session: Arc<dyn SandboxSession>,
}

impl WriteTool {
    pub fn new(session: Arc<dyn SandboxSession>) -> Self {
        Self { session }
    }
}

impl Tool for WriteTool {
    fn name(&self) -> &str {
        "write"
    }
    fn description(&self) -> &str {
        "Write a file in the sandbox workspace. Input: {path: string, content: string, write_if_unchanged?: bool}. Creates parent dirs. When write_if_unchanged is true and the existing bytes already equal content, the write is skipped (no mtime bump)."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "content": {"type": "string"},
                "write_if_unchanged": {"type": "boolean", "default": false}
            },
            "required": ["path", "content"]
        })
    }
    fn execute(&self, _ctx: ToolCtx, input: Value) -> PFut<'_, Result<Value, ToolError>> {
        let session = self.session.clone();
        Box::pin(async move {
            let path = input
                .get("path")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolError::InvalidInput("write: path (string) required".into()))?;
            let content = input
                .get("content")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    ToolError::InvalidInput("write: content (string) required".into())
                })?;
            let skip_if_unchanged = input
                .get("write_if_unchanged")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            // Read the existing bytes (if any) so should_skip_write can decide.
            // A missing or unreadable file yields None — never a skip, so a
            // brand-new file is always created. The cap is content+1 (min the
            // standard edit cap): a larger existing file reads truncated and
            // cannot falsely match, so it falls through to a real write.
            let cap = content.len().saturating_add(1).max(EDIT_MAX_BYTES + 1);
            let existing = session.read_file(path, cap).await.ok();
            let lines = content.lines().count();
            if should_skip_write(skip_if_unchanged, existing.as_deref(), content.as_bytes()) {
                return Ok(json!({
                    "path": path,
                    "bytes": content.len(),
                    "lines": lines,
                    "unchanged": true
                }));
            }
            session
                .write_file(path, content.as_bytes().to_vec())
                .await
                .map_err(|e| ToolError::Failed(format!("write: {e}")))?;
            Ok(json!({"path": path, "bytes": content.len(), "lines": lines}))
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

/// Edit a file by replacing old_string with new_string. Strict exact
/// matching (no fuzzy replacers — fuzzy matching is too error-prone). Fail-closed: 0 matches, multiple
/// matches without replace_all, empty old_string, and no-op edits are all
/// refused before any write. Text files only (non-utf-8 → error). Returns a
/// unified diff of the change so the model and the approval UI see exactly
/// what changed. Destructive ⇒ approval-gated.
pub struct EditTool {
    session: Arc<dyn SandboxSession>,
}

impl EditTool {
    pub fn new(session: Arc<dyn SandboxSession>) -> Self {
        Self { session }
    }
}

/// Max bytes Edit will read into memory. Files larger than this are refused
/// (Edit is for source edits, not giant generated files).
const EDIT_MAX_BYTES: usize = 256 * 1024;

impl Tool for EditTool {
    fn name(&self) -> &str {
        "edit"
    }
    fn description(&self) -> &str {
        "Edit a file by replacing old_string with new_string (exact match). \
         Input: {path, old_string, new_string, replace_all?}. \
         old_string must be unique unless replace_all. \
         Refused: 0 matches, multiple matches without replace_all, empty old_string, no-op. \
         Returns a unified diff. Text files only."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "old_string": {"type": "string"},
                "new_string": {"type": "string"},
                "replace_all": {"type": "boolean"}
            },
            "required": ["path", "old_string", "new_string"]
        })
    }
    fn execute(&self, _ctx: ToolCtx, input: Value) -> PFut<'_, Result<Value, ToolError>> {
        let session = self.session.clone();
        Box::pin(async move {
            let path = input
                .get("path")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolError::InvalidInput("edit: path (string) required".into()))?;
            let old = input
                .get("old_string")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    ToolError::InvalidInput("edit: old_string (string) required".into())
                })?;
            let new = input
                .get("new_string")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    ToolError::InvalidInput("edit: new_string (string) required".into())
                })?;
            let replace_all = input
                .get("replace_all")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let (diff, n, bytes) = apply_edit(&session, path, old, new, replace_all).await?;
            Ok(json!({
                "path": path,
                "diff": diff,
                "occurrences_replaced": n,
                "bytes": bytes,
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

/// Apply multiple edits atomically to one file. Each edit is {old_string,
/// new_string, replace_all?} applied in order to the in-memory content; any
/// failed edit (0/multi/no-op) aborts the whole batch with NO write (all-or-
/// nothing). Single-file atomic batch now (multi-file transactions are a
/// TODO). Returns a unified diff of original→final.
pub struct MultiEditTool {
    session: Arc<dyn SandboxSession>,
}

impl MultiEditTool {
    pub fn new(session: Arc<dyn SandboxSession>) -> Self {
        Self { session }
    }
}

impl Tool for MultiEditTool {
    fn name(&self) -> &str {
        "multiedit"
    }
    fn description(&self) -> &str {
        "Apply multiple edits atomically to one file. \
         Input: {path, edits: [{old_string, new_string, replace_all?}]}. \
         All-or-nothing: any failed edit rolls back (no write). \
         Returns a unified diff of original to final."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "edits": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "old_string": {"type": "string"},
                            "new_string": {"type": "string"},
                            "replace_all": {"type": "boolean"}
                        },
                        "required": ["old_string", "new_string"]
                    }
                }
            },
            "required": ["path", "edits"]
        })
    }
    fn execute(&self, _ctx: ToolCtx, input: Value) -> PFut<'_, Result<Value, ToolError>> {
        let session = self.session.clone();
        Box::pin(async move {
            let path = input.get("path").and_then(|v| v.as_str()).ok_or_else(|| {
                ToolError::InvalidInput("multiedit: path (string) required".into())
            })?;
            let edits = input
                .get("edits")
                .and_then(|v| v.as_array())
                .ok_or_else(|| {
                    ToolError::InvalidInput("multiedit: edits (array) required".into())
                })?;
            if edits.is_empty() {
                return Err(ToolError::InvalidInput(
                    "multiedit: edits must be non-empty".into(),
                ));
            }
            // Read once; apply all edits in memory; write once on full success.
            let original = read_text_for_edit(&session, path).await?;
            let mut content = original.clone();
            let mut applied = 0u32;
            for (i, e) in edits.iter().enumerate() {
                let old = e
                    .get("old_string")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        ToolError::InvalidInput(format!(
                            "multiedit: edits[{i}].old_string required"
                        ))
                    })?;
                let new = e
                    .get("new_string")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        ToolError::InvalidInput(format!(
                            "multiedit: edits[{i}].new_string required"
                        ))
                    })?;
                let replace_all = e
                    .get("replace_all")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                content = apply_one(&content, old, new, replace_all)
                    .map_err(|err| ToolError::Failed(format!("multiedit: edits[{i}]: {err}")))?;
                applied += 1;
            }
            let _ = applied;
            let diff = super::unified_diff(&original, &content, 3);
            session
                .write_file(path, content.into_bytes())
                .await
                .map_err(|e| ToolError::Failed(format!("multiedit: {e}")))?;
            Ok(json!({
                "path": path,
                "diff": diff,
                "edits_applied": applied,
                "bytes": original.len(),
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

/// Apply one old→new replacement to content, fail-closed. Returns the new
/// content or an error message. Pure (no I/O) so MultiEdit can apply a batch
/// in memory and abort cleanly on the first failure.
fn apply_one(content: &str, old: &str, new: &str, replace_all: bool) -> Result<String, String> {
    if old.is_empty() {
        return Err("old_string must be non-empty".into());
    }
    if old == new {
        return Err("old_string == new_string (no-op edit refused)".into());
    }
    let n = content.matches(old).count();
    if n == 0 {
        return Err("old_string not found".into());
    }
    if n > 1 && !replace_all {
        // The multi-match error: state the count, give the two recovery
        // paths, and echo the offending old_string so the model can see
        // exactly what it sent (catch whitespace/escape mismatches) and
        // recover on retry.
        return Err(format!(
            "Found {n} matches of the string to replace, but replace_all is false. \
             To replace all occurrences, set replace_all to true. To replace only one \
             occurrence, please provide more context to uniquely identify the instance.\n\
             String: {old}"
        ));
    }
    Ok(if replace_all {
        content.replace(old, new)
    } else {
        content.replacen(old, new, 1)
    })
}

/// Decide whether a Write can be skipped: only when the caller asked for
/// write-if-unchanged AND the existing bytes exactly equal the new content.
/// A missing file (None) is never a skip — the write creates it. Pure so the
/// decision is unit-testable without a sandbox.
fn should_skip_write(write_if_unchanged: bool, existing: Option<&[u8]>, content: &[u8]) -> bool {
    write_if_unchanged && matches!(existing, Some(b) if b == content)
}

/// Read a file's full utf-8 text for editing. Reads one byte past the cap so a
/// file exactly at the cap is NOT falsely refused (only files exceeding it are).
/// Refuses files larger than EDIT_MAX_BYTES and non-utf-8 (binary) content.
/// Shared by EditTool and MultiEditTool so the truncation guard can't drift
/// between them (a prior MultiEdit bug silently wrote truncated content).
async fn read_text_for_edit(
    session: &Arc<dyn SandboxSession>,
    path: &str,
) -> Result<String, ToolError> {
    let bytes = session
        .read_file(path, EDIT_MAX_BYTES + 1)
        .await
        .map_err(|e| ToolError::Failed(format!("edit: {e}")))?;
    if bytes.len() > EDIT_MAX_BYTES {
        return Err(ToolError::Failed(format!(
            "edit: {path} too large (>={EDIT_MAX_BYTES} bytes)"
        )));
    }
    String::from_utf8(bytes)
        .map_err(|_| ToolError::Decode("edit: file is not valid utf-8 (binary?)".into()))
}

/// Read + validate + replace + diff + write for EditTool. Shared as a free fn
/// so the logic is unit-testable without a sandbox (apply_one covers the pure
/// half; this covers the I/O half).
async fn apply_edit(
    session: &Arc<dyn SandboxSession>,
    path: &str,
    old: &str,
    new: &str,
    replace_all: bool,
) -> Result<(String, u32, usize), ToolError> {
    let original = read_text_for_edit(session, path).await?;
    let n = original.matches(old).count() as u32;
    let modified = apply_one(&original, old, new, replace_all)
        .map_err(|m| ToolError::Failed(format!("edit: {m}")))?;
    let diff = super::unified_diff(&original, &modified, 3);
    session
        .write_file(path, modified.into_bytes())
        .await
        .map_err(|e| ToolError::Failed(format!("edit: {e}")))?;
    Ok((diff, n, original.len()))
}

#[cfg(test)]
mod tests {
    use super::{apply_one, should_skip_write, validate_read_max_bytes};

    #[test]
    fn test_apply_one_single_replace() {
        let c = "fn foo() { 1 }\n";
        let out = apply_one(c, "1", "2", false).unwrap();
        assert_eq!(out, "fn foo() { 2 }\n");
    }

    #[test]
    fn test_apply_one_refuses_empty() {
        let err = apply_one("abc", "", "x", false).unwrap_err();
        assert!(err.contains("non-empty"));
    }

    #[test]
    fn test_apply_one_refuses_noop() {
        let err = apply_one("abc", "b", "b", false).unwrap_err();
        assert!(err.contains("no-op"));
    }

    #[test]
    fn test_read_max_bytes_zero() {
        let err = validate_read_max_bytes(0).unwrap_err();
        assert!(err.to_string().contains("max_bytes"));
    }

    #[test]
    fn test_read_max_bytes_positive() {
        assert!(validate_read_max_bytes(1).is_ok());
        assert!(validate_read_max_bytes(65_536).is_ok());
    }

    #[test]
    fn test_skip_write_when_equal() {
        let content = b"hello";
        // Flag off -> never skip, even when content matches.
        assert!(!should_skip_write(false, Some(content), content));
        // Flag on but no existing file -> create it, no skip.
        assert!(!should_skip_write(true, None, content));
        // Flag on, existing differs -> write.
        assert!(!should_skip_write(true, Some(b"world"), content));
        // Flag on, existing equals content -> skip.
        assert!(should_skip_write(true, Some(content), content));
    }

    #[test]
    fn test_apply_one_refuses_zero() {
        let err = apply_one("abc", "z", "y", false).unwrap_err();
        assert!(err.contains("not found"));
    }

    #[test]
    fn test_apply_one_refuses_ambiguous() {
        let err = apply_one("a a a", "a", "b", false).unwrap_err();
        // The multi-match error: states the count, gives the two recovery
        // paths, and echoes the offending old_string.
        assert!(err.contains("Found 3 matches"), "{err}");
        assert!(err.contains("replace_all"), "{err}");
        assert!(err.contains("more context"), "{err}");
        assert!(err.contains("String: a"), "must echo the old_string: {err}");
    }

    #[test]
    fn test_apply_one_replace_all() {
        let out = apply_one("a a a", "a", "b", true).unwrap();
        assert_eq!(out, "b b b");
    }

    /// The atomic-batch core branch: if the second edit in a sequence
    /// fails, the original content is unchanged (the first edit's
    /// in-memory result is discarded, no write). This is the all-or-
    /// nothing invariant MultiEdit relies on.
    #[test]
    fn test_multiedit_second_fail_original() {
        let original = "fn foo() { 1 }\n";
        let after_first = apply_one(original, "1", "2", false).unwrap();
        assert_eq!(after_first, "fn foo() { 2 }\n");
        let err = apply_one(&after_first, "nonexistent", "x", false);
        assert!(err.is_err(), "second edit must fail");
        assert_eq!(original, "fn foo() { 1 }\n", "original is unchanged");
    }
}
