//! Path-confinement utilities shared by the filesystem-access tools. Every
//! tool that walks the filesystem or resolves a model-supplied path routes
//! the candidate through confine_path so the workspace boundary is enforced
//! in one place, not per-tool. Extracted from the tools module proper so the
//! tool implementations file stays under the file-size gate.

use std::path::{Path, PathBuf};

use houyicoder_api::sandbox::is_within_bounds;
use houyicoder_protocol::extension::ToolError;
#[cfg(test)]
use std::env::temp_dir;
#[cfg(test)]
use std::fs::create_dir_all;
#[cfg(test)]
use std::fs::remove_dir_all;
#[cfg(test)]
use std::fs::write;
use std::path::Component;
#[cfg(test)]
use std::process::id;

/// Canonicalize without the Windows verbatim prefix. The std form returns
/// \\?\ paths, which break glob patterns and git arguments; dunce drops the
/// prefix when it is not needed and is a passthrough on Unix.
fn canonicalize(path: impl AsRef<Path>) -> std::io::Result<PathBuf> {
    dunce::canonicalize(path)
}

/// Canonicalize a candidate path under root and verify it stays within the
/// workspace or a user-authorized additional dir. The candidate is joined to
/// root, canonicalized (resolving parent directory references, dot components,
/// and symlinks), then checked against the canonicalized root plus any granted
/// dirs. A path that escapes both is rejected with a ToolError the model can
/// see and recover from. The additional dirs flow here so glob/grep honor a
/// /permissions working-dir grant at the application layer (the sandbox resolve
/// is the kernel fence; this is the supplementary application guard).
pub(crate) fn confine_path(
    root: &Path,
    additional: &[String],
    p: &str,
) -> Result<PathBuf, ToolError> {
    let candidate = root.join(p);
    let croot = canonical_root(root)?;
    // Granted dirs beyond the root. canonicalize so symlinks + the macOS
    // /var -> /private/var prefix compare cleanly with the canonical
    // candidate. The fence's working_dirs() already returns canonical, so a
    // future gate-side caller can use is_within_bounds directly without
    // re-canonicalizing; confine_path keeps the canonicalize as a defensive
    // measure for callers that have not been through the fence.
    let extra: Vec<PathBuf> = additional
        .iter()
        .filter_map(|d| canonicalize(d).ok())
        .collect();
    match canonicalize(&candidate) {
        Ok(canonical) => {
            if is_within_bounds(&canonical, &croot, &extra) {
                Ok(canonical)
            } else {
                Err(ToolError::PathEscapes("path escapes workspace".into()))
            }
        }
        // No such path: lexical check so an escaping path that does not exist
        // is still reported as an escape (not a generic access failure) — the
        // rejection wording stays stable whether or not the escaped target
        // exists. A non-escaping path that simply does not exist yet (e.g. a
        // file in a granted dir before it is written) surfaces as Io.
        Err(e) => {
            if lexical_escape(root, &candidate) {
                Err(ToolError::PathEscapes("path escapes workspace".into()))
            } else {
                Err(ToolError::Io(format!("path not accessible: {e}")))
            }
        }
    }
}

/// Lexically detect whether a candidate path resolves outside the root by
/// folding cur-dir and parent-dir segments without following symlinks or
/// requiring any component to exist. True when the normalized candidate does
/// not start with the normalized root.
fn lexical_escape(root: &Path, candidate: &Path) -> bool {
    !normalize_lexical(candidate).starts_with(normalize_lexical(root))
}

/// Fold a path lexically: keep normal segments, drop cur-dir, pop on
/// parent-dir. Root and prefix components are preserved so absolute paths
/// stay absolute. Does not touch the filesystem, so it works on paths that
/// do not yet exist.
fn normalize_lexical(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Canonicalize the workspace root, resolving any symlinks in the root path
/// itself. Used by confine_path and by tools that need the canonical root for
/// result filtering or relativization.
pub(crate) fn canonical_root(root: &Path) -> Result<PathBuf, ToolError> {
    canonicalize(root).map_err(|e| ToolError::Io(format!("workspace root not accessible: {e}")))
}

/// Convert an absolute path under root into a workspace-relative string,
/// falling back to the full path when it is not under root. Shared by glob
/// and grep.
///
/// Components join with a forward slash, not the platform separator: the
/// model feeds these paths back as glob patterns, which are forward-slash on
/// every host. Rebuilding from components rather than replacing backslashes
/// keeps a literal backslash in a Unix filename intact.
pub(crate) fn relativize(root: &Path, path: &Path) -> String {
    match path.strip_prefix(root) {
        Ok(rel) => rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/"),
        // Unchanged: rewriting separators would corrupt a root or prefix component.
        Err(_) => path.to_string_lossy().into_owned(),
    }
}

/// Validate that a confined path is a directory. Used by tools that require
/// a directory input (e.g. glob) to give the model a clear error instead of
/// an empty result when the path points to a file.
pub(crate) fn require_dir(p: &Path) -> Result<(), ToolError> {
    if !p.is_dir() {
        return Err(ToolError::Failed(format!(
            "path is not a directory: {}",
            p.display()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
