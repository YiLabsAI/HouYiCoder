//! Workspace + session-log path resolution. Split from composition.rs to
//! keep that file under the file-size gate.

/// Resolve the workspace root the sandbox should pin to, so the agent's bash
/// lands in the repo (and can see + edit the code it is developing), never in
/// the inherited home dir. Order: an explicit project env override (set by
/// the CLI --project flag), then HOUYICODER_PROJECT, then walk up from the
/// current dir for a Cargo.toml workspace root. Returns None when no manifest
/// is found and no override is set — the caller degrades to a tempdir session
/// with a notice.
pub fn resolve_project_workspace(project: Option<String>) -> Option<std::path::PathBuf> {
    use std::path::PathBuf;
    if let Some(p) = project
        && !p.is_empty()
    {
        let pb = PathBuf::from(p);
        return Some(pb.canonicalize().unwrap_or(pb));
    }
    const ENV_PROJECT: &str = "HOUYICODER_PROJECT";
    if let Ok(p) = std::env::var(ENV_PROJECT)
        && !p.is_empty()
    {
        let pb = PathBuf::from(p);
        return Some(pb.canonicalize().unwrap_or(pb));
    }
    let start = std::env::current_dir().ok()?;
    walk_to_workspace_root(&start)
}

/// Walk up from start; return the topmost ancestor whose manifest exists and
/// marks a workspace root. A workspace manifest is a Cargo.toml containing a
/// [workspace] section; if none qualifies, fall back to the topmost ancestor
/// with any Cargo.toml (a single-crate project root). Returns None if no
/// manifest is found on the path from start to the filesystem root.
pub fn walk_to_workspace_root(start: &std::path::Path) -> Option<std::path::PathBuf> {
    use std::path::PathBuf;
    let mut workspace_root: Option<PathBuf> = None;
    let mut any_root: Option<PathBuf> = None;
    let mut dir: Option<PathBuf> = start.canonicalize().ok().or_else(|| Some(start.into()));
    while let Some(d) = dir {
        let manifest = d.join("Cargo.toml");
        if manifest.is_file() {
            any_root = Some(d.clone());
            if std::fs::read_to_string(&manifest)
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

/// The canonical workspace path a session's sidecar cwd should record + the
/// value --continue converges on. resolve_project_workspace when a manifest
/// is found (already canonicalized), else the canonicalized current dir -- the
/// fallback must canonicalize too so a non-project dir's sessions match across
/// symlinked paths (macOS /tmp vs /private/tmp), not just the manifest case.
pub fn workspace_cwd(project: Option<String>) -> String {
    resolve_project_workspace(project)
        .or_else(|| {
            std::env::current_dir()
                .ok()
                .and_then(|p| p.canonicalize().ok().or(Some(p)))
        })
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// The sessions root. Each session lives in <sid>/log.jsonl under this.
/// sid-keyed (NOT cwd-slug) so a session survives its original dir being
/// deleted -- resume the log from anywhere. session.json records the
/// original cwd; resume falls back to the current cwd if it is gone.
/// Default: $HOME/.houyi/sessions. A sessions-dir env override points at a
/// custom root (used by the PTY test harness to land each test's session
/// log in an isolated temp dir, never the developer real home). Public so
/// the CLI resume path builds a file backend at the same root.
pub fn session_log_root() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("HOUYICODER_SESSIONS_DIR")
        && !p.is_empty()
    {
        return std::path::PathBuf::from(p);
    }
    let home = std::env::var("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::new());
    home.join(".houyicoder").join("sessions")
}
