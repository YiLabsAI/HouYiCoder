//! Permission interception for external ACP children. A reverse
//! session/request_permission from the child is checked against the fence
//! first, then answered from the parent gate, then delegated to the parent
//! user when the gate asks.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use houyicoder_permission::{ModeGate, Outcome, ToolRequest};
use houyicoder_protocol::acp_wire::{
    PermissionOptionKind, RequestPermissionOutcome, RequestPermissionRequest,
    RequestPermissionResponse, SelectedPermissionOutcome,
};
use serde_json::Value;

/// The parent's verdict for one intercepted permission ask, as the option id
/// the child expects back.
pub enum InterceptedVerdict {
    /// Answer with the allow option the child offered.
    Allow,
    /// Answer with the reject option the child offered.
    Reject,
    /// Reap the ask without an answer, as a cancel would.
    Cancel,
}

/// Check one intercepted ask against the workspace fence. A path-bearing
/// call whose target resolves outside the workspace root is an escape and
/// must be refused locally without disturbing the parent user. None means
/// the ask carries no path or stays inside, so it proceeds to the gate.
pub fn check_fence(
    ask: &RequestPermissionRequest,
    workspace_root: &Path,
) -> Option<InterceptedVerdict> {
    let paths: Vec<String> = ask
        .tool_call
        .fields
        .locations
        .iter()
        .map(|loc| loc.path.clone())
        .collect();
    let input_path = ask
        .tool_call
        .fields
        .raw_input
        .as_ref()
        .and_then(|input| input.get("path").or_else(|| input.get("file_path")))
        .and_then(Value::as_str)
        .map(str::to_string);
    let mut candidates = paths;
    if let Some(p) = input_path {
        candidates.push(p);
    }
    for candidate in candidates {
        if candidate.is_empty() {
            continue;
        }
        let target = Path::new(&candidate);
        if escapes_root(target, workspace_root) {
            return Some(InterceptedVerdict::Reject);
        }
    }
    None
}

/// Whether a candidate path lands outside the workspace root once symlinks
/// are resolved. A relative path is joined onto the root before the check
/// because the child runs with the root as its cwd. A candidate that does
/// not exist yet resolves through its deepest existing ancestor, so a file
/// the child is about to create inside the root is judged against the root
/// and not against a dangling symlink spelling of the temp directory.
fn escapes_root(candidate: &Path, root: &Path) -> bool {
    let joined = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        root.join(candidate)
    };
    let resolved = best_effort_resolve(&joined);
    let root_resolved = dunce::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    !resolved.starts_with(&root_resolved)
}

/// Resolve as much of a path as the filesystem knows. The missing tail
/// stays verbatim, so a not-yet-created file keeps its spelling under the
/// resolved parent.
fn best_effort_resolve(path: &Path) -> PathBuf {
    if let Ok(full) = dunce::canonicalize(path) {
        return full;
    }
    let parent = path.parent().map(best_effort_resolve);
    match (parent, path.file_name()) {
        (Some(parent), Some(name)) => parent.join(name),
        _ => path.to_path_buf(),
    }
}

/// Ask the parent gate for one intercepted request. A gate allow answers the
/// child immediately, a deny rejects it, and an ask returns None so the
/// caller delegates to the parent user.
///
/// No gate is a refusal, never an allow: a host that wired none has no policy
/// to answer with, and a synthesized permissive gate would grant a process
/// child what the session never approved.
pub fn check_gate(
    gate: Option<&Arc<dyn ModeGate>>,
    ask: &RequestPermissionRequest,
) -> Option<InterceptedVerdict> {
    let Some(gate) = gate else {
        tracing::warn!("external child asked with no parent gate wired; refusing");
        return Some(InterceptedVerdict::Reject);
    };
    let tool = tool_name_of(ask);
    let input = ask.tool_call.fields.raw_input.clone();
    let req = ToolRequest {
        tool_name: &tool,
        input: input.as_ref(),
        is_destructive: false,
        is_read_only: false,
        native_requires_approval: false,
    };
    match gate.decide(&req).outcome() {
        Outcome::Allow => Some(InterceptedVerdict::Allow),
        Outcome::Deny => Some(InterceptedVerdict::Reject),
        Outcome::Ask => None,
    }
}

/// The tool name the child's ask carries, taken from the raw input or the
/// card title when the input names none.
fn tool_name_of(ask: &RequestPermissionRequest) -> String {
    ask.tool_call
        .fields
        .raw_input
        .as_ref()
        .and_then(|v| v.get("tool_name"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_default()
}

/// Build the response that answers one intercepted ask with the verdict the
/// child offered an option for. A reject falls back to cancellation when the
/// child offered no reject option.
pub fn verdict_response(
    ask: &RequestPermissionRequest,
    verdict: &InterceptedVerdict,
) -> RequestPermissionResponse {
    let wanted = match verdict {
        InterceptedVerdict::Allow => PermissionOptionKind::AllowOnce,
        InterceptedVerdict::Reject => PermissionOptionKind::RejectOnce,
        InterceptedVerdict::Cancel => {
            return RequestPermissionResponse {
                outcome: RequestPermissionOutcome::Cancelled,
                meta: None,
            };
        }
    };
    let option_id = find_option(ask, wanted);
    match option_id {
        Some(id) => RequestPermissionResponse {
            outcome: RequestPermissionOutcome::Selected(SelectedPermissionOutcome {
                option_id: id,
                meta: None,
            }),
            meta: None,
        },
        None => RequestPermissionResponse {
            outcome: RequestPermissionOutcome::Cancelled,
            meta: None,
        },
    }
}

/// The option id of the first option of the wanted kind, or None when the
/// child offered none.
fn find_option(ask: &RequestPermissionRequest, kind: PermissionOptionKind) -> Option<String> {
    ask.options
        .iter()
        .find(|o| o.kind == kind)
        .map(|o| o.option_id.clone())
}

#[cfg(test)]
mod tests;
