//! File pattern matching tool — find files by glob pattern.
//!
//! Returns matching file paths sorted by modification time (newest first).
//! Read-only and concurrency-safe: the agent can run many globs in parallel
//! without side effects. Paths are relativized to the workspace root to save
//! model context tokens. A result cap prevents context overflow; the isolate
//! seam (context lifecycle) will later make large outputs lossless via a CAS
//! block ref, at which point the cap lifts.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use glob::MatchOptions;
use houyicoder_api::sandbox::SandboxSession;
use houyicoder_async::PFut;
use serde_json::{Value, json};

use super::path_util::{canonical_root, confine_path, relativize, require_dir};
use super::{Tool, ToolCtx, ToolError};
use std::cmp::Reverse;
#[cfg(test)]
use std::env::temp_dir;
use std::fs::metadata;
#[cfg(all(test, unix))]
use std::os::unix::fs::symlink;
use std::path::is_separator;
#[cfg(test)]
use std::process::id;
#[cfg(test)]
use std::sync::atomic::AtomicUsize;
#[cfg(test)]
use std::sync::atomic::Ordering;
use tokio::task::spawn_blocking;
#[cfg(test)]
use tokio_util::sync::CancellationToken;

/// Placeholder cap pending the isolate seam (context lifecycle will make
/// large tool outputs lossless via a CAS block ref). Generous enough for
/// exploratory file discovery while preventing context bloat.
const GLOB_MAX_FILES: usize = 200;

/// Build-output and dependency directories pruned from glob results so a
/// broad pattern does not surface generated/transient artifacts (target/,
/// node_modules/, ...). Matches grep's IGNORE_DIRS.
const IGNORE_DIRS: &[&str] = &[
    "target",
    "node_modules",
    "dist",
    "build",
    ".next",
    ".cache",
    ".houyicoder",
    ".claude",
];

/// VCS metadata directories excluded from glob results so a broad pattern
/// does not surface .git/ and friends. Matches grep's VCS_DIRS and is
/// applied in the subprocess path via --glob so the result set matches.
const VCS_DIRS: &[&str] = &[".git", ".svn", ".hg", ".bzr", ".jj", ".sl"];

/// True when the path descends into a pruned build-output/dependency
/// directory (a final or intermediate segment matches IGNORE_DIRS). The
/// glob crate matches the whole pattern, so this filters the collected
/// entries rather than pruning the walk.
fn under_ignore_dir(rel: &Path) -> bool {
    rel.components().any(|c| {
        c.as_os_str()
            .to_str()
            .map(|s| IGNORE_DIRS.contains(&s))
            .unwrap_or(false)
    })
}

/// A file pattern matching tool backed by the sandbox workspace.
pub struct GlobTool {
    session: Arc<dyn SandboxSession>,
}

impl GlobTool {
    pub fn new(session: Arc<dyn SandboxSession>) -> Self {
        Self { session }
    }
}

impl Tool for GlobTool {
    fn name(&self) -> &str {
        "glob"
    }
    fn description(&self) -> &str {
        "Fast file pattern matching. \
         Input: {pattern: string, path?: string}. \
         Returns matching file paths sorted by modification time (newest first). \
         Supports **, *, ?, [abc] wildcards. Truncates at 200 results."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string"},
                "path": {"type": "string"}
            },
            "required": ["pattern"],
            "additionalProperties": false
        })
    }
    fn execute(&self, ctx: ToolCtx, input: Value) -> PFut<'_, Result<Value, ToolError>> {
        let root = self.session.workspace_root().to_path_buf();
        let dirs = self.session.working_dirs();
        let cancel = ctx.cancel.clone();
        Box::pin(async move {
            let pattern = input
                .get("pattern")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolError::InvalidInput("glob: pattern (string) required".into()))?
                .to_string();
            let base = match input.get("path").and_then(|v| v.as_str()) {
                Some(p) => {
                    let confined = confine_path(&root, &dirs, p)?;
                    require_dir(&confined)?;
                    confined
                }
                None => canonical_root(&root)?,
            };
            // Primary path: shell to rg --files so a cancel kills the child
            // immediately and stops CPU, instead of the spawn_blocking orphan
            // running to completion. Falls back to the in-process traversal
            // when the rg binary is not on PATH or the pattern is not
            // expressible as an rg --glob (absolute, parent-dir traversal).
            if let Some((files, truncated)) =
                subprocess::try_run_glob_subprocess(&root, &base, &pattern, cancel.as_ref()).await?
            {
                return Ok(json!({
                    "filenames": files,
                    "num_files": files.len(),
                    "truncated": truncated,
                }));
            }
            // Fallback: run the synchronous glob traversal on the blocking
            // pool so a broad pattern over a large tree does not pin a tokio
            // async worker. Race against the run's CancellationToken: an Esc
            // mid-walk short-circuits to an interrupted error instead of
            // waiting for the blocking traversal to return.
            let handle = spawn_blocking(move || glob_files(&root, &base, &pattern, GLOB_MAX_FILES));
            let (files, truncated) = match cancel {
                Some(token) => tokio::select! {
                    _ = token.cancelled() => return Err(ToolError::Failed(
                        "glob: interrupted by user".into(),
                    )),
                    r = handle => r.map_err(|e| ToolError::Failed(
                        format!("glob: traversal join: {e}"),
                    ))??,
                },
                None => handle
                    .await
                    .map_err(|e| ToolError::Failed(format!("glob: traversal join: {e}")))??,
            };
            Ok(json!({
                "filenames": files,
                "num_files": files.len(),
                "truncated": truncated,
            }))
        })
    }
    fn is_concurrency_safe(&self) -> bool {
        true
    }
    fn is_read_only(&self) -> bool {
        true
    }
    fn is_destructive(&self) -> bool {
        false
    }
}

/// Walk the glob pattern under base, collect matches, sort by mtime (newest
/// first), relativize to root, and cap at max_files. Returns (filenames,
/// truncated). Pure (no sandbox I/O) so the core logic is unit-testable
/// without a session — the Tool just passes workspace_root as both root and
/// the default base.
fn glob_files(
    root: &Path,
    base: &Path,
    pattern: &str,
    max_files: usize,
) -> Result<(Vec<String>, bool), ToolError> {
    let croot = canonical_root(root)?;
    check_pattern_confined(&croot, base, pattern)?;
    let full_pattern = join_pattern(base, pattern);
    let opts = glob_options();
    let entries: Vec<(PathBuf, SystemTime)> = glob::glob_with(&full_pattern, opts)
        .map_err(|e| ToolError::Failed(format!("glob: invalid pattern: {e}")))?
        .filter_map(|r| r.ok())
    // Defense in depth: filter results so a match that canonicalizes outside
    // the workspace (via a symlink, or a wildcard-expanded path that escapes
    // through parent-directory segments after a wildcard) is dropped.
        .filter(|p| {
            dunce::canonicalize(p)
                .map(|c| c.starts_with(&croot))
                .unwrap_or(false)
        })
    // Prune build-output / dependency directories so a broad pattern does
    // not surface target/ or node_modules/ artifacts.
        .filter(|p| {
            let rel = p.strip_prefix(&croot).unwrap_or(p);
            !under_ignore_dir(rel)
        })
        .map(|p| {
            let mtime = metadata(&p)
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            (p, mtime)
        })
        .collect();
    let mut sorted = entries;
    sorted.sort_by_key(|(_, t)| Reverse(*t));
    let truncated = sorted.len() > max_files;
    let files: Vec<String> = sorted
        .into_iter()
        .take(max_files)
        .map(|(p, _)| relativize(&croot, &p))
        .collect();
    Ok((files, truncated))
}

/// Check that the fixed (non-wildcard) directory prefix of a glob pattern
/// resolves to a path under the canonical root. This catches absolute
/// patterns and relative patterns with parent-directory traversal before
/// any filesystem enumeration starts, giving the model a clear error.
/// Patterns that start with a wildcard (e.g. **/*.rs) are safe because
/// enumeration begins from the already-confined base.
///
/// Takes the base as a Path and the pattern as a string, and scans only the
/// pattern. Scanning a joined base-plus-pattern string is what made this
/// fragile: a Windows canonical base can carry a verbatim prefix whose
/// question mark is a path character, not a glob wildcard, and joining a
/// backslash base to a forward-slash pattern mixes separators, so the scan
/// could split in the wrong place. The base never reaches the scanner now.
///
/// Separator detection uses is_separator, which is platform-aware: both / and
/// \ on Windows, only / on Unix, where a backslash is a legal filename char.
fn check_pattern_confined(croot: &Path, base: &Path, pattern: &str) -> Result<(), ToolError> {
    if pattern.is_empty() {
        return Ok(());
    }
    let mut wildcard_pos = pattern.len();
    let mut last_sep = None;
    for (i, c) in pattern.char_indices() {
        if matches!(c, '*' | '?' | '[') {
            wildcard_pos = i;
            break;
        }
        if is_separator(c) {
            last_sep = Some(i);
        }
    }
    // A leading wildcard enumerates from the already-confined base.
    if wildcard_pos == 0 {
        return Ok(());
    }
    // The fixed directory portion is everything up to the last separator
    // preceding the first wildcard, so a wildcard in the filename (file?.rs,
    // test[12].rs) does not make this canonicalize a path that cannot exist.
    // No separator means the pattern names an entry directly in the base, so
    // the base itself is what needs checking.
    let dir_part = match last_sep {
        Some(i) => pattern[..i].trim_end_matches(is_separator),
        None => "",
    };
    // Mirror join_pattern's notion of absolute so this check and the
    // enumeration that follows reason about the same path.
    let dir = if pattern.starts_with('/') {
        PathBuf::from(if dir_part.is_empty() { "/" } else { dir_part })
    } else if dir_part.is_empty() {
        base.to_path_buf()
    } else {
        base.join(dir_part)
    };
    let canonical = dunce::canonicalize(&dir)
        .map_err(|e| ToolError::Io(format!("path not accessible: {e}")))?;
    if !canonical.starts_with(croot) {
        return Err(ToolError::PathEscapes("pattern escapes workspace".into()));
    }
    Ok(())
}

/// Join a base path and a glob pattern into a single pattern string. If the
/// pattern is absolute (starts with /), return it as-is; otherwise join with
/// the OS separator so the glob engine resolves from base.
fn join_pattern(base: &Path, pattern: &str) -> String {
    if pattern.starts_with('/') {
        return pattern.to_string();
    }
    let base_str = base.to_string_lossy();
    let sep = if base_str.ends_with('/') { "" } else { "/" };
    format!("{base_str}{sep}{pattern}")
}

/// Glob match options: case-insensitive on case-insensitive filesystems,
/// and let wildcards cross directory separators so *.rs matches a/b/c.rs
/// (standard glob filter behavior).
fn glob_options() -> MatchOptions {
    MatchOptions {
        case_sensitive: false,
        require_literal_separator: false,
        require_literal_leading_dot: false,
    }
}

mod subprocess;

#[cfg(test)]
mod tests;
