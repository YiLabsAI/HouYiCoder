//! Workspace + session-log path resolution. Split from composition.rs to
//! keep that file under the file-size gate.
//!
//! Every canonicalization here goes through dunce, so the workspace leaves
//! this module in the plain form with no Windows verbatim prefix. Consumers
//! compare this path against dunce-form grant sets and hand it to git probes;
//! a std canonicalize form would never prefix-match the grants, and git for
//! Windows is widely reported to reject it as well.

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
    if let Some(ws) = override_workspace(
        project.as_deref(),
        std::env::var(ENV_PROJECT).ok().as_deref(),
    ) {
        return Some(ws);
    }
    let start = std::env::current_dir().ok()?;
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

/// The fallback when no manifest qualified: the canonicalized current dir, so
/// a non-project dir's sessions match across symlinked paths (macOS /tmp vs
/// /private/tmp), not just the manifest case.
fn cwd_fallback() -> Option<PathBuf> {
    std::env::current_dir()
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

/// The sessions root. Each session lives in <sid>/log.jsonl under this.
/// sid-keyed (NOT cwd-slug) so a session survives its original dir being
/// deleted -- resume the log from anywhere. session.json records the
/// original cwd; resume falls back to the current cwd if it is gone.
/// Default: $HOME/.houyi/sessions. A sessions-dir env override points at a
/// custom root (used by the PTY test harness to land each test's session
/// log in an isolated temp dir, never the developer real home). Public so
/// the CLI resume path builds a file backend at the same root.
pub fn session_log_root() -> PathBuf {
    if let Ok(p) = std::env::var(houyicoder_config::ENV_HOUYICODER_SESSIONS_DIR)
        && !p.is_empty()
    {
        return PathBuf::from(p);
    }
    let home = std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::new());
    home.join(".houyicoder").join("sessions")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("houyi-paths-{tag}-{}", std::process::id()));
        drop(fs::remove_dir_all(&dir));
        fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    /// The explicit argument beats the environment value, matching the CLI
    /// --project precedence; an empty raw value is skipped rather than
    /// treated as an override.
    #[test]
    fn test_override_precedence() {
        let dir = temp_dir("precedence");
        let canonical = dunce::canonicalize(&dir).expect("canonical");
        let dir_str = dir.to_str().expect("dir str");

        let from_arg = override_workspace(Some(dir_str), Some("/ignored")).expect("arg wins");
        assert_eq!(from_arg, canonical);
        let from_env = override_workspace(Some(""), Some(dir_str)).expect("env wins");
        assert_eq!(from_env, canonical, "an empty argument falls to the env");
        assert!(
            override_workspace(None, Some("")).is_none(),
            "no override means the manifest walk decides"
        );
        fs::remove_dir_all(&dir).ok();
    }

    /// An override pointing at a directory that does not exist yet is still
    /// an override, returned unchanged instead of falling through to the
    /// walk.
    #[test]
    fn test_override_missing_passthrough() {
        let root = temp_dir("missing");
        let raw = root.join("not-created-yet");
        let raw_str = raw.to_str().expect("raw str");
        let got = override_workspace(Some(raw_str), None).expect("passthrough");
        assert_eq!(got, raw);
        fs::remove_dir_all(&root).ok();
    }

    /// The resolver honors an explicit argument end to end, in the plain
    /// canonical form every consumer compares against.
    #[test]
    fn test_resolve_explicit_project() {
        let dir = temp_dir("resolve");
        let canonical = dunce::canonicalize(&dir).expect("canonical");
        let got = resolve_project_workspace(Some(dir.to_string_lossy().into_owned()))
            .expect("explicit project");
        assert_eq!(got, canonical);
        fs::remove_dir_all(&dir).ok();
    }

    /// The fallback canonicalizes, so a non-project dir's sessions match
    /// across symlinked paths.
    #[test]
    fn test_cwd_fallback_canonical() {
        let cwd = std::env::current_dir().expect("cwd");
        let expected = dunce::canonicalize(&cwd).expect("canonical cwd");
        assert_eq!(cwd_fallback().expect("fallback"), expected);
    }
}
