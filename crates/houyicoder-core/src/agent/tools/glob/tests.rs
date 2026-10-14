//! Tests for this module.

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
    ) -> houyicoder_async::PFut<'_, Result<Vec<houyicoder_context::DirEntry>, SandboxError>> {
        Box::pin(async { Err(SandboxError::Unsupported("test".into())) })
    }
    fn path_exists(&self, _path: &str) -> houyicoder_async::PFut<'_, Result<bool, SandboxError>> {
        Box::pin(async { Ok(false) })
    }
    fn workspace_root(&self) -> Arc<Path> {
        Arc::from(self.root.clone())
    }
}

/// Create a unique temp dir for test isolation. std lacks mkdtemp so
/// compose pid + counter + nanos.
fn test_dir() -> PathBuf {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path = temp_dir().join(format!("glob-test-{}-{n}-{nanos}", id()));
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
    let token = CancellationToken::new();
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
