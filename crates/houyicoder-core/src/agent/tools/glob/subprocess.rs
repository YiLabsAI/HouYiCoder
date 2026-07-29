//! Subprocess-backed glob: shell to rg --files so an Esc cancel kills the
//! child immediately, instead of the spawn_blocking orphan running to
//! completion. Falls back to the in-process glob traversal when rg is not
//! on PATH or the pattern is not expressible as an rg --glob (absolute
//! patterns, parent-dir traversal) — the in-process path already confines
//! and rejects those.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use tokio_util::sync::CancellationToken;

use houyicoder_protocol::extension::ToolError;

use crate::agent::tools::path_util::canonical_root;
use crate::agent::tools::subprocess_util::{find_rg, spawn_rg_select_cancel};

use super::{GLOB_MAX_FILES, IGNORE_DIRS, VCS_DIRS, relativize};

/// Try to run glob via the rg --files subprocess. Ok(Some) means rg ran and
/// produced the file list; Ok(None) means rg is not available or the pattern
/// is not expressible as an rg --glob, so the caller falls back to the
/// in-process traversal; Err surfaces a real rg failure. The returned pair
/// matches glob_files: (relativized filenames, truncated flag).
pub(super) async fn try_run_glob_subprocess(
    root: &Path,
    base: &Path,
    pattern: &str,
    cancel: Option<&CancellationToken>,
) -> Result<Option<(Vec<String>, bool)>, ToolError> {
    // rg --glob only accepts relative patterns; an absolute pattern or one
    // with parent-dir segments must go through the in-process confine check.
    if pattern.starts_with('/') || pattern.contains("..") {
        return Ok(None);
    }
    let rg_path = match find_rg() {
        Some(p) => p,
        None => return Ok(None),
    };
    let args = build_rg_args(pattern);
    let output = match spawn_rg_select_cancel(&rg_path, &args, base, cancel).await? {
        Some(o) => o,
        None => return Ok(None),
    };
    let croot = canonical_root(root)?;
    Ok(Some(parse_files(&output.stdout, &croot)))
}

/// Build the rg --files argument list: list files under the target, filtered
/// by the user pattern, with VCS + build-output dirs pruned and hidden files
/// included so the result set matches the in-process traversal's scope.
fn build_rg_args(pattern: &str) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "--files".into(),
        "--glob".into(),
        pattern.into(),
        "--hidden".into(),
    ];
    for d in VCS_DIRS {
        args.push("--glob".into());
        args.push(format!("!{d}"));
    }
    for d in IGNORE_DIRS {
        args.push("--glob".into());
        args.push(format!("!{d}"));
    }
    args
}

/// Parse rg --files stdout into (relativized filenames, truncated). Each
/// line is an absolute path; relativize to the workspace root, sort by mtime
/// desc (newest first), and cap at GLOB_MAX_FILES so the truncated flag and
/// the result shape match the in-process glob_files.
fn parse_files(stdout: &[u8], croot: &Path) -> (Vec<String>, bool) {
    let text = String::from_utf8_lossy(stdout);
    let mut entries: Vec<(PathBuf, SystemTime)> = Vec::new();
    for line in text.lines() {
        if line.is_empty() {
            continue;
        }
        let p = PathBuf::from(line);
        let mtime = std::fs::metadata(&p)
            .and_then(|m| m.modified())
            .unwrap_or(UNIX_EPOCH);
        entries.push((p, mtime));
    }
    entries.sort_by_key(|(_, t)| std::cmp::Reverse(*t));
    let truncated = entries.len() > GLOB_MAX_FILES;
    let files: Vec<String> = entries
        .into_iter()
        .take(GLOB_MAX_FILES)
        .map(|(p, _)| relativize(croot, &p))
        .collect();
    (files, truncated)
}
