//! Workspace + session-log path resolution. Split from composition.rs to
//! keep that file under the file-size gate.
//!
//! Every canonicalization here goes through dunce, so the workspace leaves
//! this module in the plain form with no Windows verbatim prefix. Consumers
//! compare this path against dunce-form grant sets and hand it to git probes;
//! a std canonicalize form would never prefix-match the grants, and git for
//! Windows is widely reported to reject it as well.

use std::env::current_dir;
use std::env::var;
use std::fs::metadata;
use std::fs::read_to_string;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// The workspace override decision: the first non-empty raw value wins and is
/// normalized to the plain canonical form; None means no override, so the
/// caller falls through to the manifest walk. An override pointing at a
/// directory that does not exist yet passes through unchanged -- it is still
/// an override. Split out of the resolver so precedence and normalization
/// are testable without touching the process environment.
fn override_workspace(project: Option<&str>, env_value: Option<&str>) -> Option<PathBuf> {
    for raw in [project, env_value].into_iter().flatten() {
        if !raw.is_empty() {
            let pb = PathBuf::from(raw);
            return Some(dunce::canonicalize(&pb).unwrap_or(pb));
        }
    }
    None
}

/// Resolve the workspace root the sandbox should pin to, so the agent's bash
/// lands in the repo (and can see + edit the code it is developing), never in
/// the inherited home dir. Order: an explicit project override (set by the
/// CLI --project flag), then HOUYICODER_PROJECT, then walk up from the
/// current dir for a Cargo.toml workspace root. Returns None when no manifest
/// is found and no override is set — the caller degrades to a tempdir session
/// with a notice. The environment read is a thin shell around the pure
/// decision above, which is where the testable logic lives.
pub fn resolve_project_workspace(project: Option<String>) -> Option<PathBuf> {
    const ENV_PROJECT: &str = houyicoder_config::ENV_HOUYICODER_PROJECT;
    if let Some(ws) = override_workspace(project.as_deref(), var(ENV_PROJECT).ok().as_deref()) {
        return Some(ws);
    }
    let start = current_dir().ok()?;
    walk_to_workspace_root(&start)
}

/// Walk up from start; return the topmost ancestor whose manifest exists and
/// marks a workspace root. A workspace manifest is a Cargo.toml containing a
/// [workspace] section; if none qualifies, fall back to the topmost ancestor
/// with any Cargo.toml (a single-crate project root). Returns None if no
/// manifest is found on the path from start to the filesystem root.
pub fn walk_to_workspace_root(start: &Path) -> Option<PathBuf> {
    let mut workspace_root: Option<PathBuf> = None;
    let mut any_root: Option<PathBuf> = None;
    let mut dir: Option<PathBuf> = dunce::canonicalize(start)
        .ok()
        .or_else(|| Some(start.into()));
    while let Some(d) = dir {
        let manifest = d.join("Cargo.toml");
        if manifest.is_file() {
            any_root = Some(d.clone());
            if read_to_string(&manifest)
                .map(|body| body.contains("[workspace]"))
                .unwrap_or(false)
            {
                workspace_root = Some(d.clone());
                break;
            }
        }
        dir = d.parent().map(PathBuf::from);
    }
    workspace_root.or(any_root)
}

/// The fallback when no manifest qualified: the canonicalized current dir, so
/// a non-project dir's sessions match across symlinked paths (macOS /tmp vs
/// /private/tmp), not just the manifest case.
fn cwd_fallback() -> Option<PathBuf> {
    current_dir()
        .ok()
        .and_then(|p| dunce::canonicalize(&p).ok().or(Some(p)))
}

/// The canonical workspace path a session's descriptor cwd should record + the
/// value --continue converges on. resolve_project_workspace when a manifest
/// is found (already canonicalized), else the canonicalized current dir.
pub fn workspace_cwd(project: Option<String>) -> String {
    resolve_project_workspace(project)
        .or_else(cwd_fallback)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Where the workspace's snapshot store lives. It is kept outside the
/// workspace tree so the agent's own find and grep do not walk cloned copies of
/// the repository, which both slows the search down and returns hits from
/// files that are not the working tree.
///
/// clonefile and FICLONE only work within one volume, so the store stays
/// workspace-local when the home directory is on a different volume: a cross
/// volume store would silently turn every snapshot into a full copy, which is
/// slower and larger than the alternative it was meant to improve.
pub fn snapshot_store_root(workspace: &Path) -> PathBuf {
    let home_store = var("HOME")
        .ok()
        .map(PathBuf::from)
        .map(|home| home.join(".houyicoder").join("snapshots"))
        .filter(|dir| same_filesystem(workspace, dir));
    match home_store {
        Some(dir) => dir.join(workspace_slug(workspace)),
        None => workspace.join(".houyicoder").join("snapshots"),
    }
}

/// Whether two paths live on the same filesystem. A missing path is measured
/// through its nearest existing ancestor, so a store directory that has not
/// been created yet is judged by the volume it would land on. An unresolvable
/// path answers no, so the caller falls back to the location that is
/// guaranteed to work.
#[cfg(unix)]
fn same_filesystem(a: &Path, b: &Path) -> bool {
    match (filesystem_of(a), filesystem_of(b)) {
        (Some(dev_a), Some(dev_b)) => dev_a == dev_b,
        _ => false,
    }
}

#[cfg(unix)]
fn filesystem_of(path: &Path) -> Option<u64> {
    let mut cursor = path;
    loop {
        if let Ok(meta) = metadata(cursor) {
            return Some(meta.dev());
        }
        cursor = cursor.parent()?;
    }
}

#[cfg(not(unix))]
fn same_filesystem(_a: &Path, _b: &Path) -> bool {
    false
}

/// A filesystem-safe, stable directory name for one workspace: its own name
/// plus a hash of the full path, so two checkouts named alike do not share a
/// store.
///
/// The hash is taken over the path as given, so callers pass the canonical
/// form: the same workspace spelled through a symlink would otherwise mint a
/// second store, and the second store's migration would find the first one's
/// snapshots already there.
fn workspace_slug(workspace: &Path) -> String {
    let raw_name = workspace
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("workspace");
    let name: String = raw_name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .take(48)
        .collect();
    let mut hash: u64 = 5381;
    for byte in workspace.to_string_lossy().as_bytes() {
        hash = hash.wrapping_mul(33) ^ u64::from(*byte);
    }
    format!("{name}-{hash:016x}")
}

/// The sessions root. Each session lives in <sid>/log.jsonl under this.
/// sid-keyed (NOT cwd-slug) so a session survives its original dir being
/// deleted -- resume the log from anywhere. session.json records the
/// original cwd; resume falls back to the current cwd if it is gone.
/// Default: the sessions directory under the config home. A sessions-dir env
/// override points at a custom root (used by the PTY test harness to land each
/// test's session log in an isolated temp dir, never the developer real home).
/// Public so the CLI resume path builds a file backend at the same root.
pub fn session_log_root() -> PathBuf {
    if let Ok(p) = var(houyicoder_config::ENV_HOUYICODER_SESSIONS_DIR)
        && !p.is_empty()
    {
        return PathBuf::from(p);
    }
    let home = var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::new());
    home.join(".houyicoder").join("sessions")
}

#[cfg(test)]
mod tests;
