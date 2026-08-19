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
            let handle = tokio::task::spawn_blocking(move || {
                glob_files(&root, &base, &pattern, GLOB_MAX_FILES)
            });
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
            let mtime = std::fs::metadata(&p)
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            (p, mtime)
        })
        .collect();
    let mut sorted = entries;
    sorted.sort_by_key(|(_, t)| std::cmp::Reverse(*t));
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
        if std::path::is_separator(c) {
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
        Some(i) => pattern[..i].trim_end_matches(std::path::is_separator),
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
mod tests {
    use super::*;
    use houyicoder_api::sandbox::SandboxSession;
    use houyicoder_api::tool::ToolCtx;
    use houyicoder_context::{ExecConfig, ExecResult, SandboxError};
    use std::fs;
    use std::sync::Arc;
    use std::thread;
    use std::time::{Duration, SystemTime};

    /// A minimal SandboxSession stub backed by a temp dir. Only workspace_root
    /// and working_dirs are read by GlobTool::execute; the shell/file ops
    /// return Unsupported (never called by glob).
    struct StubSession {
        root: PathBuf,
    }
    impl SandboxSession for StubSession {
        fn exec_with_config(
            &self,
            _command: &str,
            _config: ExecConfig,
        ) -> houyicoder_async::PFut<'_, Result<ExecResult, SandboxError>> {
            Box::pin(async { Err(SandboxError::Unsupported("test".into())) })
        }
        fn read_file(
            &self,
            _path: &str,
            _max: usize,
        ) -> houyicoder_async::PFut<'_, Result<Vec<u8>, SandboxError>> {
            Box::pin(async { Err(SandboxError::Unsupported("test".into())) })
        }
        fn write_file(
            &self,
            _path: &str,
            _content: Vec<u8>,
        ) -> houyicoder_async::PFut<'_, Result<(), SandboxError>> {
            Box::pin(async { Err(SandboxError::Unsupported("test".into())) })
        }
        fn list_dir(
            &self,
            _path: &str,
        ) -> houyicoder_async::PFut<'_, Result<Vec<houyicoder_context::DirEntry>, SandboxError>>
        {
            Box::pin(async { Err(SandboxError::Unsupported("test".into())) })
        }
        fn path_exists(
            &self,
            _path: &str,
        ) -> houyicoder_async::PFut<'_, Result<bool, SandboxError>> {
            Box::pin(async { Ok(false) })
        }
        fn workspace_root(&self) -> std::sync::Arc<std::path::Path> {
            std::sync::Arc::from(self.root.clone())
        }
    }

    /// Create a unique temp dir for test isolation. std lacks mkdtemp so
    /// compose pid + counter + nanos.
    fn test_dir() -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path =
            std::env::temp_dir().join(format!("glob-test-{}-{n}-{nanos}", std::process::id()));
        fs::create_dir_all(&path).unwrap();
        path
    }

    /// Write a file and optionally sleep to force distinct mtimes.
    fn touch(dir: &Path, name: &str, content: &str) {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, content).unwrap();
    }

    #[test]
    fn test_pattern_matches_files() {
        let dir = test_dir();
        touch(&dir, "a.rs", "fn a() {}");
        touch(&dir, "b.rs", "fn b() {}");
        touch(&dir, "c.txt", "hello");
        let (files, trunc) = glob_files(&dir, &dir, "**/*.rs", 200).unwrap();
        assert!(!trunc);
        assert_eq!(files.len(), 2);
        assert!(files.iter().any(|f| f.contains("a.rs")));
        assert!(files.iter().any(|f| f.contains("b.rs")));
        assert!(!files.iter().any(|f| f.contains("c.txt")));
        fs::remove_dir_all(&dir).ok();
    }

    /// Build-output / dependency directories are pruned from glob results so
    /// a broad **/*.rs does not surface target/ or node_modules/ artifacts.
    #[test]
    fn test_ignore_dirs_pruned() {
        let dir = test_dir();
        touch(&dir, "a.rs", "");
        touch(&dir, "target/debug/lib.rs", "");
        touch(&dir, "node_modules/pkg/index.rs", "");
        let (files, _) = glob_files(&dir, &dir, "**/*.rs", 200).unwrap();
        assert_eq!(
            files.len(),
            1,
            "target/ and node_modules/ pruned, got: {files:?}"
        );
        assert!(files[0].contains("a.rs"));
        fs::remove_dir_all(&dir).ok();
    }

    /// GlobTool::execute runs the traversal on the blocking pool (not the
    /// async worker) and returns the same result as the direct glob_files call.
    #[tokio::test]
    async fn test_execute_runs_blocking_pool() {
        let dir = test_dir();
        touch(&dir, "a.rs", "");
        touch(&dir, "target/x.rs", "");
        let session = Arc::new(StubSession { root: dir.clone() }) as Arc<dyn SandboxSession>;
        let tool = GlobTool::new(session);
        let out = tool
            .execute(
                ToolCtx::new("c1"),
                serde_json::json!({"pattern": "**/*.rs"}),
            )
            .await
            .expect("execute ok");
        let files = out.get("filenames").and_then(|v| v.as_array()).unwrap();
        assert_eq!(files.len(), 1, "target/ pruned in execute path too");
        assert!(files[0].as_str().unwrap().contains("a.rs"));
        fs::remove_dir_all(&dir).ok();
    }

    /// A cancelled GlobTool::execute short-circuits to an interrupted error
    /// instead of waiting for the traversal. Proves the select!{cancelled}
    /// race covers the cancel path even when spawn_blocking is used.
    #[tokio::test]
    async fn test_execute_cancel_returns_interrupted() {
        let dir = test_dir();
        touch(&dir, "a.rs", "");
        let session = Arc::new(StubSession { root: dir.clone() }) as Arc<dyn SandboxSession>;
        let tool = GlobTool::new(session);
        let token = tokio_util::sync::CancellationToken::new();
        token.cancel();
        let ctx = ToolCtx::new("c1").with_cancel(token);
        let err = tool
            .execute(ctx, serde_json::json!({"pattern": "**/*.rs"}))
            .await
            .expect_err("cancelled execute errors");
        assert!(err.to_string().contains("interrupted"), "{err}");
        fs::remove_dir_all(&dir).ok();
    }

    /// The StubSession's unused trait methods (shell/file ops) return
    /// Unsupported or Ok(false) — exercised here so the stub stays honest and
    /// the trait surface is covered.
    #[tokio::test]
    async fn test_stub_unused_return_unsupported() {
        let dir = test_dir();
        let s = StubSession { root: dir.clone() };
        assert!(s.exec("git status").await.is_err());
        assert!(s.read_file("x", 10).await.is_err());
        assert!(s.write_file("x", vec![]).await.is_err());
        assert!(s.list_dir(".").await.is_err());
        assert!(!s.path_exists("nope").await.unwrap_or(false));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_path_option_filters() {
        let dir = test_dir();
        touch(&dir, "root.rs", "");
        touch(&dir, "src/nested.rs", "");
        touch(&dir, "lib/deep.rs", "");
        let (files, _) = glob_files(&dir, &dir.join("src"), "**/*.rs", 200).unwrap();
        assert_eq!(files.len(), 1);
        assert!(files[0].contains("nested.rs"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_mtime_sort_newest_first() {
        let dir = test_dir();
        touch(&dir, "old.rs", "");
        // Sleep so the next file has a strictly newer mtime.
        thread::sleep(Duration::from_millis(20));
        touch(&dir, "new.rs", "");
        thread::sleep(Duration::from_millis(20));
        touch(&dir, "mid.rs", "");
        thread::sleep(Duration::from_millis(20));
        touch(&dir, "newest.rs", "");
        let (files, _) = glob_files(&dir, &dir, "**/*.rs", 200).unwrap();
        assert_eq!(files.len(), 4);
        // newest first
        assert!(files[0].contains("newest.rs"));
        assert!(files[1].contains("mid.rs"));
        assert!(files[2].contains("new.rs"));
        assert!(files[3].contains("old.rs"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_cap_truncated_flag() {
        let dir = test_dir();
        for i in 0..10 {
            touch(&dir, &format!("f{i}.rs"), "");
        }
        let (files, truncated) = glob_files(&dir, &dir, "**/*.rs", 5).unwrap();
        assert_eq!(files.len(), 5);
        assert!(truncated);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_no_match_returns_empty() {
        let dir = test_dir();
        touch(&dir, "a.rs", "");
        let (files, trunc) = glob_files(&dir, &dir, "**/*.zzz", 200).unwrap();
        assert!(files.is_empty());
        assert!(!trunc);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_glob_returns_relative_paths() {
        let dir = test_dir();
        let cdir = canonical_root(&dir).unwrap();
        touch(&dir, "src/a.rs", "");
        touch(&dir, "src/b.rs", "");
        let (files, _) = glob_files(&dir, &cdir, "**/*.rs", 200).unwrap();
        assert_eq!(files.len(), 2);
        for f in &files {
            assert!(
                !f.starts_with('/'),
                "expected relative path, got absolute: {f}"
            );
        }
        assert!(files.iter().any(|f| f.ends_with("src/a.rs")));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_confine_rejects_traversal() {
        let dir = test_dir();
        touch(&dir, "a.rs", "");
        let result = glob_files(&dir, &dir, "**/*.rs", 200);
        // Sanity: normal glob works.
        assert!(result.is_ok());
        // Path traversal via parent-dir references must be rejected.
        let result = confine_path(&dir, &[], "../../etc");
        assert!(result.is_err());
        fs::remove_dir_all(&dir).ok();
    }

    /// A path inside a user-granted working dir (beyond the workspace root)
    /// must NOT be treated as an escape — the agent may read checkouts the
    /// human added. Pins the additional-dirs half of confine_path.
    #[test]
    fn test_confine_allows_granted_dir() {
        let root = test_dir();
        let extra = test_dir();
        touch(&extra, "secret.txt", "");
        let extra_str = extra.to_string_lossy().to_string();
        let p = extra.join("secret.txt").to_string_lossy().to_string();
        let result = confine_path(&root, &[extra_str], &p);
        assert!(result.is_ok(), "granted dir access must pass: {result:?}");
        // The same path without the grant still escapes the workspace root.
        let denied = confine_path(&root, &[], &p);
        assert!(denied.is_err(), "unganted path must escape");
        fs::remove_dir_all(&root).ok();
        fs::remove_dir_all(&extra).ok();
    }

    /// A real path inside an ungranted dir escapes (canonical resolves but
    /// matches no granted dir).
    #[test]
    fn test_confine_existing_outside_escapes() {
        let root = test_dir();
        let extra = test_dir();
        touch(&extra, "ok.txt", "");
        let p = extra.join("ok.txt").to_string_lossy().to_string();
        let result = confine_path(&root, &[], &p);
        assert!(
            matches!(result, Err(ToolError::PathEscapes(_))),
            "existing path outside all dirs must escape: {result:?}"
        );
        fs::remove_dir_all(&root).ok();
        fs::remove_dir_all(&extra).ok();
    }

    /// A path under the root that does not exist is an access error, not an
    /// escape (lexical check passes; canonicalize fails).
    #[test]
    fn test_confine_missing_is_io() {
        let root = test_dir();
        let result = confine_path(&root, &[], "missing.txt");
        assert!(
            matches!(result, Err(ToolError::Io(_))),
            "missing path under root must be Io, not escape: {result:?}"
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn test_confine_rejects_abs_pattern() {
        let dir = test_dir();
        touch(&dir, "a.rs", "");
        // An absolute pattern pointing outside the workspace must error.
        let result = glob_files(&dir, &dir, "/etc/*", 200);
        assert!(result.is_err());
        // Absolute single file (no wildcard) also rejected.
        let result = glob_files(&dir, &dir, "/etc/passwd", 200);
        assert!(result.is_err());
        // A pattern that stays inside (relative) must succeed.
        let result = glob_files(&dir, &dir, "**/*.rs", 200);
        assert!(result.is_ok());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_require_dir_rejects_file() {
        let dir = test_dir();
        touch(&dir, "a.rs", "");
        // A file path is rejected when a directory is required.
        let result = require_dir(&dir.join("a.rs"));
        assert!(result.is_err());
        // A directory path passes.
        let result = require_dir(&dir);
        assert!(result.is_ok());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_confine_wildcard_in_filename() {
        let dir = test_dir();
        let cdir = canonical_root(&dir).unwrap();
        touch(&dir, "src/file1.rs", "");
        touch(&dir, "src/file2.rs", "");
        // Patterns with ? in the filename must not be false-rejected.
        let (files, _) = glob_files(&dir, &cdir, "src/file?.rs", 200).unwrap();
        assert_eq!(files.len(), 2);
        // Bracket wildcards in filename also work.
        let (files, _) = glob_files(&dir, &cdir, "src/file[12].rs", 200).unwrap();
        assert_eq!(files.len(), 2);
        fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn test_backslash_not_separator() {
        let dir = test_dir();
        let croot = canonical_root(&dir).unwrap();
        // A backslash is a legal Unix filename character. Treating it as a
        // separator would split the directory at "weird", which does not
        // exist, and turn a valid pattern into a spurious Io error.
        assert!(
            check_pattern_confined(&croot, &croot, "weird\\name*.rs").is_ok(),
            "backslash in a Unix filename must not split the directory"
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn test_symlink_escape_filtered() {
        use std::os::unix::fs::symlink;
        let dir = test_dir();
        let outside = test_dir();
        touch(&dir, "real.rs", "");
        symlink(&outside, dir.join("escape")).unwrap();
        let cdir = canonical_root(&dir).unwrap();
        let (files, _) = glob_files(&dir, &cdir, "**/*", 200).unwrap();
        // The symlink canonicalizes outside the workspace, so it is dropped.
        assert!(!files.iter().any(|f| f.contains("escape")));
        assert!(files.iter().any(|f| f.contains("real.rs")));
        fs::remove_dir_all(&dir).ok();
        fs::remove_dir_all(&outside).ok();
    }
}
