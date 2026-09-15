//! Path normalization and directory-capability analysis shared by permission
//! pre-checks and sandbox enforcement.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde_json::Value;

/// A canonical candidate is in bounds when it is under the workspace root or
/// an additional authorized directory.
pub fn is_within_bounds(candidate: &Path, root: &Path, additional: &[PathBuf]) -> bool {
    candidate.starts_with(root) || additional.iter().any(|dir| candidate.starts_with(dir))
}

/// Canonicalize a tool path, resolving the complete path when it exists or
/// the nearest existing ancestor when its trailing segments are absent.
pub fn normalize_tool_path(base: &Path) -> Option<PathBuf> {
    if let Ok(canonical) = dunce::canonicalize(base) {
        return Some(canonical);
    }
    let mut existing = base.to_path_buf();
    let mut tail: Vec<OsString> = Vec::new();
    while !existing.exists() {
        let name = existing.file_name()?.to_os_string();
        tail.push(name);
        existing = existing.parent()?.to_path_buf();
    }
    let mut canonical = dunce::canonicalize(existing).ok()?;
    for name in tail.iter().rev() {
        canonical.push(name);
    }
    Some(canonical)
}

/// Access granted for a path-bearing tool call outside the current fence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BoundaryAccess {
    /// Read access without write capability.
    ReadOnly,
    /// Read and write access.
    ReadWrite,
}

/// A directory capability required before an approved tool call can execute.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BoundaryGrant {
    /// Directory covered by the capability.
    pub directory: PathBuf,
    /// Whether the directory is readable only or writable.
    pub access: BoundaryAccess,
}

/// Resolve the directory capabilities a tool call needs outside the fence.
/// In-bounds targets produce no grant. An unresolved target remains outside
/// and uses its supplied location so capability installation can fail closed.
pub fn boundary_grants_for(
    tool_name: &str,
    input: Option<&Value>,
    root: &Path,
    read_dirs: &[PathBuf],
    write_dirs: &[PathBuf],
) -> Vec<BoundaryGrant> {
    let access = if boundary_path_uses_parent(tool_name) {
        BoundaryAccess::ReadWrite
    } else {
        BoundaryAccess::ReadOnly
    };
    let additional = match access {
        BoundaryAccess::ReadOnly => read_dirs,
        BoundaryAccess::ReadWrite => write_dirs,
    };
    let mut grants = Vec::new();
    for supplied in path_args_for_boundary(tool_name, input) {
        let candidate = root.join(supplied);
        let resolved = normalize_tool_path(&candidate);
        if resolved
            .as_deref()
            .is_some_and(|path| is_within_bounds(path, root, additional))
        {
            continue;
        }
        let target = resolved.as_deref().unwrap_or(&candidate);
        let directory = match access {
            BoundaryAccess::ReadWrite => target.parent().unwrap_or(target),
            BoundaryAccess::ReadOnly if target.is_file() => target.parent().unwrap_or(target),
            BoundaryAccess::ReadOnly => target,
        }
        .to_path_buf();
        let grant = BoundaryGrant { directory, access };
        if !grants.contains(&grant) {
            grants.push(grant);
        }
    }
    grants
}

/// Whether approving this tool's file target authorizes its parent directory.
pub fn boundary_path_uses_parent(tool_name: &str) -> bool {
    matches!(
        tool_name.to_ascii_lowercase().as_str(),
        "write" | "edit" | "multiedit"
    )
}

/// Extract path-bearing arguments for a workspace boundary check.
pub fn path_args_for_boundary(tool_name: &str, input: Option<&Value>) -> Vec<String> {
    let Some(value) = input else {
        return Vec::new();
    };
    match tool_name.to_ascii_lowercase().as_str() {
        tool if boundary_path_uses_parent(tool) || tool == "grep" => value
            .get("path")
            .and_then(Value::as_str)
            .map(|path| vec![path.to_string()])
            .unwrap_or_default(),
        "glob" => {
            let mut paths = Vec::new();
            if let Some(path) = value.get("path").and_then(Value::as_str) {
                paths.push(path.to_string());
            }
            if let Some(pattern) = value.get("pattern").and_then(Value::as_str) {
                let prefix = match pattern.find(['*', '?', '[']) {
                    Some(position) => &pattern[..position],
                    None => pattern,
                };
                let directory = prefix
                    .rfind('/')
                    .map(|index| &prefix[..index])
                    .unwrap_or(prefix)
                    .trim_end_matches('/');
                if !directory.is_empty() {
                    paths.push(directory.to_string());
                }
            }
            paths
        }
        _ => Vec::new(),
    }
}
