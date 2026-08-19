//! Path-confinement utilities shared by the filesystem-access tools. Every
//! tool that walks the filesystem or resolves a model-supplied path routes
//! the candidate through confine_path so the workspace boundary is enforced
//! in one place, not per-tool. Extracted from the tools module proper so the
//! tool implementations file stays under the file-size gate.

use std::path::{Path, PathBuf};

use houyicoder_protocol::extension::ToolError;

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
            if houyicoder_api::sandbox::is_within_bounds(&canonical, &croot, &extra) {
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
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
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
mod tests {
    use super::*;

    /// A path inside the root confines to itself. Pins the happy path so a
    /// regression that over-rejects is caught.
    #[test]
    fn test_confine_path_accepts_inside() {
        let dir = std::env::temp_dir().join(format!("path-util-inside-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let file = dir.join("a.txt");
        std::fs::write(&file, b"x").expect("write");
        let confined = confine_path(&dir, &[], "a.txt").expect("inside root");
        assert_eq!(confined, canonicalize(&file).unwrap());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Forward slashes whatever the host separator is. Built with join so
    /// strip_prefix sees a real platform path (backslash on Windows).
    #[test]
    fn test_relativize_normalizes_separators() {
        let root = std::env::temp_dir().join("path-util-sep-root");
        let nested = root.join("src").join("a.rs");
        assert_eq!(relativize(&root, &nested), "src/a.rs");
    }

    /// Under root strips to the remainder; outside root keeps the full path.
    #[test]
    fn test_relativize_strips_root() {
        let root = Path::new("/workspace");
        let file = Path::new("/workspace/src/main.rs");
        assert_eq!(relativize(root, file), "src/main.rs");
        let outside = Path::new("/etc/passwd");
        assert_eq!(relativize(root, outside), "/etc/passwd");
    }

    /// A parent-dir escape is rejected lexically without touching the
    /// escaped target, so the wording is stable whether or not the target
    /// exists outside the workspace.
    #[test]
    fn test_confine_path_rejects_escape() {
        let dir = std::env::temp_dir().join(format!("path-util-esc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let err = confine_path(&dir, &[], "../../etc").expect_err("escape rejected");
        assert!(
            matches!(err, ToolError::PathEscapes(_)),
            "escape surfaces as PathEscapes"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// require_dir rejects a file path with a clear error naming the path.
    #[test]
    fn test_require_dir_rejects_file() {
        let dir = std::env::temp_dir().join(format!("path-util-dir-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let file = dir.join("a.txt");
        std::fs::write(&file, b"x").expect("write");
        assert!(require_dir(&file).is_err(), "a file is not a directory");
        assert!(require_dir(&dir).is_ok(), "the dir itself is a directory");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The shared boundary predicate: a canonical candidate is within bounds
    /// under the root or an additional authorized dir; outside both it is not.
    /// Single source of truth for confine_path + the gate so the two layers
    /// cannot drift on what "inside the workspace" means.
    #[test]
    fn test_within_bounds_covers_extra() {
        let root = std::env::temp_dir().join(format!("path-util-bounds-{}", std::process::id()));
        std::fs::create_dir_all(&root).expect("mkdir");
        let extra = std::env::temp_dir().join(format!("path-util-extra-{}", std::process::id()));
        std::fs::create_dir_all(&extra).expect("mkdir extra");
        let croot = canonicalize(&root).unwrap();
        let cextra = canonicalize(&extra).unwrap();
        let additional = vec![cextra.clone()];
        assert!(
            houyicoder_api::sandbox::is_within_bounds(&croot.join("a.txt"), &croot, &additional),
            "inside root is within bounds"
        );
        assert!(
            houyicoder_api::sandbox::is_within_bounds(&cextra.join("b.txt"), &croot, &additional),
            "inside an additional dir is within bounds"
        );
        let outside = std::env::temp_dir().join(format!("path-util-out-{}", std::process::id()));
        assert!(
            !houyicoder_api::sandbox::is_within_bounds(&outside, &croot, &additional),
            "outside both is not within bounds"
        );
        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&extra).ok();
    }
}
