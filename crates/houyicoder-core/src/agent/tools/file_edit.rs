//! Shared file-editing primitives for EditTool and MultiEditTool: the pure
//! replacement core, the read-and-validate step, and the composed edit flow.
//! Named file_edit (not edit_util) to avoid a flat-prefix collision with
//! edit.rs; both edit tools import from here so one truncation guard and
//! one match policy cannot drift between them.

use std::sync::Arc;

use houyicoder_api::sandbox::SandboxSession;
use houyicoder_protocol::extension::ToolError;

use crate::agent::diff::unified_diff;

/// Max bytes Edit will read into memory. Files larger than this are
/// refused (Edit is for source edits, not giant generated files).
pub const EDIT_MAX_BYTES: usize = 256 * 1024;

/// Apply one old to new replacement to content, fail-closed. Returns the
/// new content or an error message. Pure (no I/O) so MultiEdit can apply a
/// batch in memory and abort cleanly on the first failure.
pub fn apply_one(content: &str, old: &str, new: &str, replace_all: bool) -> Result<String, String> {
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

/// Read a file's full utf-8 text for editing. Reads one byte past the cap
/// so a file exactly at the cap is NOT falsely refused (only files
/// exceeding it are). Refuses files larger than EDIT_MAX_BYTES and
/// non-utf-8 (binary) content. Shared by EditTool and MultiEditTool so the
/// truncation guard can't drift between them (a prior MultiEdit bug
/// silently wrote truncated content).
pub async fn read_editable_text(
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

/// Read + validate + replace + diff + write for EditTool. Shared as a free
/// fn so the logic is unit-testable without a sandbox (apply_one covers the
/// pure half; this covers the I/O half).
pub async fn apply_edit(
    session: &Arc<dyn SandboxSession>,
    path: &str,
    old: &str,
    new: &str,
    replace_all: bool,
) -> Result<(String, u32, usize), ToolError> {
    let original = read_editable_text(session, path).await?;
    let n = original.matches(old).count() as u32;
    let modified = apply_one(&original, old, new, replace_all)
        .map_err(|m| ToolError::Failed(format!("edit: {m}")))?;
    let diff = unified_diff(&original, &modified, 3);
    session
        .write_file(path, modified.into_bytes())
        .await
        .map_err(|e| ToolError::Failed(format!("edit: {e}")))?;
    Ok((diff, n, original.len()))
}

#[cfg(test)]
#[path = "file_edit_tests.rs"]
mod tests;
