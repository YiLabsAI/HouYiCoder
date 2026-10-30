//! Shared file-mutation primitives for WriteTool, EditTool and MultiEditTool:
//! the pure replacement core, the read-and-validate step, the composed edit
//! flow, and the recorded write. Named file_edit (not edit_util) to avoid a
//! flat-prefix collision with edit.rs; every file tool imports from here so
//! one truncation guard, one match policy and one write step cannot drift
//! between them.

use std::sync::Arc;

use houyicoder_api::sandbox::SandboxSession;
use houyicoder_context::SessionId;
use houyicoder_protocol::extension::ToolError;

use crate::agent::diff::unified_diff;
use crate::snapshot::file_transaction::FileTransaction;

/// Max bytes Edit will read into memory. Files larger than this are refused.
/// Set to 64 MiB so the long-form review docs the model edits in place clear
/// the guard; the snapshot backup ceiling above it still bounds memory use.
pub const EDIT_MAX_BYTES: usize = 64 * 1024 * 1024;

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

/// Write bytes to a workspace path, recording both sides first when a write
/// journal is wired. Without one this is the plain sandbox write. A failure
/// says why in a form the caller prefixes with its own tool name, and means
/// the target was not touched.
pub(crate) async fn write_recorded(
    transaction: Option<&FileTransaction>,
    session_id: Option<SessionId>,
    session: &Arc<dyn SandboxSession>,
    path: &str,
    bytes: Vec<u8>,
) -> Result<(), String> {
    match transaction {
        Some(transaction) => transaction
            .write(session_id, session.as_ref(), path, bytes)
            .await
            .map_err(|refusal| refusal.to_string()),
        None => session
            .write_file(path, bytes)
            .await
            .map_err(|error| error.to_string()),
    }
}

/// Read + validate + replace + diff + write for EditTool. Shared as a free
/// fn so the logic is unit-testable without a sandbox (apply_one covers the
/// pure half; this covers the I/O half).
pub async fn apply_edit(
    transaction: Option<&FileTransaction>,
    session_id: Option<SessionId>,
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
    write_recorded(
        transaction,
        session_id,
        session,
        path,
        modified.into_bytes(),
    )
    .await
    .map_err(|reason| ToolError::Failed(format!("edit: {reason}")))?;
    Ok((diff, n, original.len()))
}

#[cfg(test)]
mod tests;
