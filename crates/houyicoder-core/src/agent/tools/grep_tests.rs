// Test-only direct spawns (git init for the gitignore fixture, mkfifo for
// the kill-on-cancel fixture, pgrep to assert no orphan) bypass the launcher
// port. They are test scaffolding, not engine spawns, so the fence and audit
// policy does not apply; allow-flagged here so the spawn-chokepoint gate stays
// honest about its scope.
#![allow(clippy::disallowed_methods)]

use super::*;
use std::fs;
use std::path::PathBuf;
use std::time::SystemTime;

fn test_dir() -> PathBuf {
    static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path = std::env::temp_dir().join(format!("grep-test-{}-{n}-{nanos}", std::process::id()));
    fs::create_dir_all(&path).unwrap();
    path
}

fn touch(dir: &Path, name: &str, content: &str) {
    let path = dir.join(name);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, content).unwrap();
}

fn opts(pattern: &str) -> GrepOptions {
    GrepOptions {
        pattern: pattern.to_string(),
        path: None,
        glob_filter: None,
        output_mode: OutputMode::FilesWithMatches,
        context_before: 0,
        context_after: 0,
        head_limit: DEFAULT_HEAD_LIMIT,
        multiline: false,
        case_insensitive: false,
        show_line_numbers: true,
        offset: 0,
        file_type: None,
    }
}

#[test]
fn test_content_mode_shows_lines() {
    let dir = test_dir();
    touch(&dir, "a.rs", "fn alpha() {}\nfn beta() {}\nfn alpha() {}\n");
    let mut o = opts("alpha");
    o.output_mode = OutputMode::Content;
    let out = run_grep(&dir, &[], &o).unwrap();
    assert_eq!(out.num_lines, 2);
    assert!(out.content.contains("a.rs:1: fn alpha() {}"));
    assert!(out.content.contains("a.rs:3: fn alpha() {}"));
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_files_with_matches_mode() {
    let dir = test_dir();
    touch(&dir, "a.rs", "fn alpha() {}\n");
    touch(&dir, "b.rs", "fn beta() {}\n");
    touch(&dir, "c.rs", "fn gamma() {}\n");
    let out = run_grep(&dir, &[], &opts("alpha")).unwrap();
    assert_eq!(out.filenames.len(), 1);
    assert!(out.filenames[0].contains("a.rs"));
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_count_mode_counts() {
    let dir = test_dir();
    touch(&dir, "a.rs", "fn foo() {}\nfn foo() {}\nfn bar() {}\n");
    touch(&dir, "b.rs", "fn foo() {}\n");
    let mut o = opts("foo");
    o.output_mode = OutputMode::Count;
    let out = run_grep(&dir, &[], &o).unwrap();
    assert_eq!(out.num_matches, 3);
    assert!(out.content.contains("a.rs:2"));
    assert!(out.content.contains("b.rs:1"));
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_context_lines_surround_match() {
    let dir = test_dir();
    let body = "line1\nline2\nMATCH\nline4\nline5\n";
    touch(&dir, "a.rs", body);
    let mut o = opts("MATCH");
    o.output_mode = OutputMode::Content;
    o.context_before = 1;
    o.context_after = 1;
    let out = run_grep(&dir, &[], &o).unwrap();
    assert!(out.content.contains("a.rs:2: line2"));
    assert!(out.content.contains("a.rs:3: MATCH"));
    assert!(out.content.contains("a.rs:4: line4"));
    assert!(!out.content.contains("a.rs:1: line1"));
    assert!(!out.content.contains("a.rs:5: line5"));
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_glob_filter_matches_extension() {
    let dir = test_dir();
    touch(&dir, "a.rs", "fn foo() {}\n");
    touch(&dir, "b.txt", "fn foo() {}\n");
    let mut o = opts("foo");
    o.glob_filter = Some("*.rs".to_string());
    let out = run_grep(&dir, &[], &o).unwrap();
    assert_eq!(out.filenames.len(), 1);
    assert!(out.filenames[0].contains("a.rs"));
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_head_limit_truncates() {
    let dir = test_dir();
    for i in 0..10 {
        touch(&dir, &format!("f{i}.rs"), "match\n");
    }
    let mut o = opts("match");
    o.head_limit = 3;
    let out = run_grep(&dir, &[], &o).unwrap();
    assert_eq!(out.filenames.len(), 3);
    assert!(out.truncated);
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_head_limit_keeps_newest() {
    let dir = test_dir();
    // Five old files created in quick succession, then a newest file after
    // a sleep. With head_limit=3, the newest must still appear because sort
    // by mtime desc happens BEFORE the limit is applied.
    for i in 0..5 {
        touch(&dir, &format!("old{i}.rs"), "match\n");
    }
    std::thread::sleep(std::time::Duration::from_millis(20));
    touch(&dir, "new.rs", "match\n");
    let mut o = opts("match");
    o.head_limit = 3;
    let out = run_grep(&dir, &[], &o).unwrap();
    assert_eq!(out.filenames.len(), 3);
    assert!(out.truncated);
    assert!(
        out.filenames.iter().any(|f| f.contains("new.rs")),
        "newest file missing from head_limit results: {:?}",
        out.filenames
    );
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_no_match_returns_empty() {
    let dir = test_dir();
    touch(&dir, "a.rs", "fn foo() {}\n");
    let out = run_grep(&dir, &[], &opts("zzzzz")).unwrap();
    assert!(out.filenames.is_empty());
    assert!(!out.truncated);
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_case_insensitive_flag() {
    let dir = test_dir();
    touch(&dir, "a.rs", "fn FooBar() {}\n");
    let mut o = opts("foobar");
    o.case_insensitive = true;
    let out = run_grep(&dir, &[], &o).unwrap();
    assert_eq!(out.filenames.len(), 1);
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_vcs_dirs_excluded() {
    let dir = test_dir();
    touch(&dir, ".git/config", "[remote]\nmatch\n");
    touch(&dir, "a.rs", "match\n");
    let out = run_grep(&dir, &[], &opts("match")).unwrap();
    assert_eq!(out.filenames.len(), 1);
    assert!(out.filenames[0].contains("a.rs"));
    fs::remove_dir_all(&dir).ok();
}

/// Build-output / dependency directories (target/, node_modules/, ...) are
/// pruned from the walk so a broad pattern does not pin a worker scanning
/// generated/transient artifacts. Without this a match over a workspace
/// with a populated target/ descends into it and surfaces generated files.
#[test]
fn test_ignore_dirs_excluded() {
    let dir = test_dir();
    touch(&dir, "a.rs", "match\n");
    touch(&dir, "target/debug/lib.rs", "match\n");
    touch(&dir, "node_modules/pkg/index.js", "match\n");
    let out = run_grep(&dir, &[], &opts("match")).unwrap();
    assert_eq!(
        out.filenames.len(),
        1,
        "target/ and node_modules/ pruned, got: {:?}",
        out.filenames
    );
    assert!(out.filenames[0].contains("a.rs"));
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_confine_rejects_traversal() {
    let dir = test_dir();
    touch(&dir, "a.rs", "match\n");
    let mut o = opts("match");
    o.path = Some("../../etc".to_string());
    let result = run_grep(&dir, &[], &o);
    assert!(result.is_err());
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_multiline_cross_line() {
    let dir = test_dir();
    let body = "fn foo() {\n    bar()\n}\n";
    touch(&dir, "a.rs", body);
    let mut o = opts("foo.*bar");
    o.output_mode = OutputMode::Content;
    let out = run_grep(&dir, &[], &o).unwrap();
    assert_eq!(out.num_lines, 0);
    o.multiline = true;
    let out = run_grep(&dir, &[], &o).unwrap();
    assert!(out.num_lines > 0);
    assert!(out.content.contains("foo"));
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_head_limit_zero_unlimited() {
    let dir = test_dir();
    for i in 0..5 {
        touch(&dir, &format!("f{i}.rs"), "match\n");
    }
    let mut o = opts("match");
    o.head_limit = 0;
    let out = run_grep(&dir, &[], &o).unwrap();
    assert_eq!(out.filenames.len(), 5);
    assert!(!out.truncated);
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_coerce_string_number() {
    let input = serde_json::json!({"pattern": "foo", "head_limit": "3", "-n": "true"});
    let parsed = GrepOptions::from_input(&input).unwrap();
    assert_eq!(parsed.head_limit, 3);
    assert!(parsed.show_line_numbers);
}

#[test]
fn test_offset_paginates() {
    let dir = test_dir();
    for i in 0..6 {
        touch(&dir, &format!("f{i}.rs"), "match\n");
    }
    let mut o = opts("match");
    o.head_limit = 2;
    o.offset = 2;
    let out = run_grep(&dir, &[], &o).unwrap();
    assert_eq!(out.filenames.len(), 2);
    assert!(out.truncated);
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_type_filter_rust() {
    let dir = test_dir();
    touch(&dir, "a.rs", "match\n");
    touch(&dir, "b.txt", "match\n");
    let mut o = opts("match");
    o.file_type = Some("rust".to_string());
    let out = run_grep(&dir, &[], &o).unwrap();
    assert_eq!(out.filenames.len(), 1);
    assert!(out.filenames[0].contains("a.rs"));
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_sort_mtime_desc() {
    let dir = test_dir();
    touch(&dir, "old.rs", "match\n");
    std::thread::sleep(std::time::Duration::from_millis(20));
    touch(&dir, "new.rs", "match\n");
    let out = run_grep(&dir, &[], &opts("match")).unwrap();
    assert_eq!(out.filenames.len(), 2);
    // Newest file first.
    assert!(out.filenames[0].contains("new.rs"));
    assert!(out.filenames[1].contains("old.rs"));
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_glob_split_multi() {
    let dir = test_dir();
    touch(&dir, "a.rs", "match\n");
    touch(&dir, "b.ts", "match\n");
    touch(&dir, "c.txt", "match\n");
    let mut o = opts("match");
    o.glob_filter = Some("*.rs *.ts".to_string());
    let out = run_grep(&dir, &[], &o).unwrap();
    assert_eq!(out.filenames.len(), 2);
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_relativize_strips_root() {
    let dir = test_dir();
    touch(&dir, "src/a.rs", "match\n");
    let out = run_grep(&dir, &[], &opts("match")).unwrap();
    assert_eq!(out.filenames.len(), 1);
    assert!(
        !out.filenames[0].starts_with('/'),
        "expected relative path, got absolute: {}",
        out.filenames[0]
    );
    assert!(out.filenames[0].ends_with("src/a.rs"));
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_line_numbers_toggle() {
    let dir = test_dir();
    touch(&dir, "a.rs", "fn foo() {}\n");
    let mut o = opts("foo");
    o.output_mode = OutputMode::Content;
    o.show_line_numbers = false;
    let out = run_grep(&dir, &[], &o).unwrap();
    // Without line numbers, the format is path:content, not path:line:content.
    assert!(out.content.contains("a.rs: fn foo"));
    assert!(!out.content.contains("a.rs:1:"));
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_as_number_accepts_float() {
    assert_eq!(as_number(&json!(5)), Some(5));
    assert_eq!(as_number(&json!(5.0)), Some(5));
    assert_eq!(as_number(&json!("5")), Some(5));
    assert_eq!(as_number(&json!("5.0")), Some(5));
    assert_eq!(as_number(&json!(-3)), None);
    assert_eq!(as_number(&json!(true)), None);
    assert_eq!(as_number(&json!(null)), None);
}

#[test]
fn test_skip_non_utf8_line() {
    let dir = test_dir();
    // Line 1 valid, line 2 non-utf8, line 3 has a match.
    let content: &[u8] = b"good\n\xff\xfe\nmatch\n";
    let path = dir.join("a.rs");
    fs::write(&path, content).unwrap();
    let mut o = opts("match");
    o.output_mode = OutputMode::Content;
    let out = run_grep(&dir, &[], &o).unwrap();
    assert!(out.content.contains("match"));
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_count_matches_limited() {
    let dir = test_dir();
    for i in 0..6 {
        touch(&dir, &format!("f{i}.rs"), "match\nmatch\n");
    }
    let mut o = opts("match");
    o.output_mode = OutputMode::Count;
    o.head_limit = 3;
    let out = run_grep(&dir, &[], &o).unwrap();
    // num_matches must equal the sum of counts in the limited output, not
    // the total across all files.
    let shown_total: usize = out
        .content
        .split('\n')
        .filter_map(|l| l.rsplit(':').next().and_then(|c| c.parse::<usize>().ok()))
        .sum();
    assert_eq!(out.num_matches, shown_total);
    assert_eq!(out.num_matches, 6); // 3 files * 2 matches each
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_large_file_skipped() {
    let dir = test_dir();
    let big = "x".repeat(1024 * 1024 + 1);
    touch(&dir, "big.rs", &big);
    touch(&dir, "small.rs", "match\n");
    let out = run_grep(&dir, &[], &opts("match")).unwrap();
    assert_eq!(out.filenames.len(), 1);
    assert!(out.filenames[0].contains("small.rs"));
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_offset_beyond_len_empty() {
    let dir = test_dir();
    touch(&dir, "a.rs", "match\n");
    let mut o = opts("match");
    o.offset = 100;
    let out = run_grep(&dir, &[], &o).unwrap();
    assert!(out.filenames.is_empty());
    fs::remove_dir_all(&dir).ok();
}

/// A minimal SandboxSession stub backed by a temp dir. Only workspace_root
/// is read by GrepTool::execute; the shell/file ops return Unsupported.
struct StubSession {
    root: PathBuf,
}
impl houyicoder_api::sandbox::SandboxSession for StubSession {
    fn exec_with_config(
        &self,
        _command: &str,
        _config: houyicoder_context::ExecConfig,
    ) -> houyicoder_async::PFut<
        '_,
        Result<houyicoder_context::ExecResult, houyicoder_context::SandboxError>,
    > {
        Box::pin(async { Err(houyicoder_context::SandboxError::Unsupported("test".into())) })
    }
    fn read_file(
        &self,
        _path: &str,
        _max: usize,
    ) -> houyicoder_async::PFut<'_, Result<Vec<u8>, houyicoder_context::SandboxError>> {
        Box::pin(async { Err(houyicoder_context::SandboxError::Unsupported("test".into())) })
    }
    fn write_file(
        &self,
        _path: &str,
        _content: Vec<u8>,
    ) -> houyicoder_async::PFut<'_, Result<(), houyicoder_context::SandboxError>> {
        Box::pin(async { Err(houyicoder_context::SandboxError::Unsupported("test".into())) })
    }
    fn list_dir(
        &self,
        _path: &str,
    ) -> houyicoder_async::PFut<
        '_,
        Result<Vec<houyicoder_context::DirEntry>, houyicoder_context::SandboxError>,
    > {
        Box::pin(async { Err(houyicoder_context::SandboxError::Unsupported("test".into())) })
    }
    fn path_exists(
        &self,
        _path: &str,
    ) -> houyicoder_async::PFut<'_, Result<bool, houyicoder_context::SandboxError>> {
        Box::pin(async { Ok(false) })
    }
    fn workspace_root(&self) -> std::sync::Arc<std::path::Path> {
        std::sync::Arc::from(self.root.clone())
    }
}

/// GrepTool::execute runs the WalkDir traversal on the blocking pool and
/// returns results matching the direct run_grep call.
#[tokio::test]
async fn test_execute_runs_blocking_pool() {
    let dir = test_dir();
    touch(&dir, "a.rs", "match\n");
    touch(&dir, "target/x.rs", "match\n");
    let session = std::sync::Arc::new(StubSession { root: dir.clone() })
        as std::sync::Arc<dyn houyicoder_api::sandbox::SandboxSession>;
    let tool = GrepTool::new(session);
    let out = tool
        .execute(
            houyicoder_api::tool::ToolCtx::new("c1"),
            serde_json::json!({"pattern": "match"}),
        )
        .await
        .expect("execute ok");
    let files = out.get("filenames").and_then(|v| v.as_array()).unwrap();
    assert_eq!(files.len(), 1, "target/ pruned in execute path too");
    assert!(files[0].as_str().unwrap().contains("a.rs"));
    fs::remove_dir_all(&dir).ok();
}

/// A cancelled GrepTool::execute short-circuits to an interrupted error
/// instead of waiting for the traversal.
#[tokio::test]
async fn test_execute_cancel_returns_interrupted() {
    let dir = test_dir();
    touch(&dir, "a.rs", "match\n");
    let session = std::sync::Arc::new(StubSession { root: dir.clone() })
        as std::sync::Arc<dyn houyicoder_api::sandbox::SandboxSession>;
    let tool = GrepTool::new(session);
    let token = tokio_util::sync::CancellationToken::new();
    token.cancel();
    let ctx = houyicoder_api::tool::ToolCtx::new("c1").with_cancel(token);
    let err = tool
        .execute(ctx, serde_json::json!({"pattern": "match"}))
        .await
        .expect_err("cancelled execute errors");
    assert!(err.to_string().contains("interrupted"), "{err}");
    fs::remove_dir_all(&dir).ok();
}

/// The subprocess path respects .gitignore: a file listed in .gitignore but
/// not in the build-output prune list is skipped by rg, where the in-process
/// fallback would scan it. This is the discriminator for the subprocess
/// migration: the fallback path has no gitignore awareness, so a gitignored
/// file with matching content surfaces there but not here. Requires the rg
/// binary on PATH; falls back to the in-process path (which would fail this
/// assertion) when rg is absent, so the test skips instead of false-failing.
#[tokio::test]
async fn test_execute_respects_gitignore() {
    if crate::agent::tools::subprocess_util::find_rg().is_none() {
        eprintln!("execute_respects_gitignore: rg not on PATH; skipping");
        return;
    }
    let dir = test_dir();
    touch(&dir, "keep.rs", "match\n");
    touch(&dir, "ignore.log", "match\n");
    fs::write(dir.join(".gitignore"), "ignore.log\n").unwrap();
    // rg only reads .gitignore inside a git repository, so initialize one in
    // the temp dir. Without this rg treats .gitignore as a plain file and
    // the gitignore-aware behavior under test would not engage.
    let git_init = std::process::Command::new("git")
        .arg("init")
        .arg("-q")
        .current_dir(&dir)
        .status()
        .expect("git init");
    assert!(git_init.success(), "git init failed in test dir");
    let session = std::sync::Arc::new(StubSession { root: dir.clone() })
        as std::sync::Arc<dyn houyicoder_api::sandbox::SandboxSession>;
    let tool = GrepTool::new(session);
    let out = tool
        .execute(
            houyicoder_api::tool::ToolCtx::new("c1"),
            serde_json::json!({"pattern": "match", "output_mode": "files_with_matches"}),
        )
        .await
        .expect("execute ok");
    let files = out.get("filenames").and_then(|v| v.as_array()).unwrap();
    assert!(
        files
            .iter()
            .any(|f| f.as_str().unwrap().contains("keep.rs")),
        "keep.rs must be present: {files:?}"
    );
    assert!(
        !files
            .iter()
            .any(|f| f.as_str().unwrap().contains("ignore.log")),
        "gitignored ignore.log must be skipped: {files:?}"
    );
    fs::remove_dir_all(&dir).ok();
}

/// The subprocess content path (rg --json) formats a uniform rel:line:
/// content row for match AND context lines, with a relativized path and a
/// space after the line number — fixing rg's colon-for-match / dash-for-
/// context inconsistency + absolute-path leakage. Requires rg on PATH;
/// skips otherwise (the in-process fallback is covered by the run_grep
/// tests above, which already assert the same format).
#[tokio::test]
async fn test_subprocess_content_uniform_format() {
    if crate::agent::tools::subprocess_util::find_rg().is_none() {
        eprintln!("subprocess_content_uniform_format: rg not on PATH; skipping");
        return;
    }
    let dir = test_dir();
    touch(&dir, "a.rs", "line1\nMATCH\nline3\n");
    let session = std::sync::Arc::new(StubSession { root: dir.clone() })
        as std::sync::Arc<dyn houyicoder_api::sandbox::SandboxSession>;
    let tool = GrepTool::new(session);
    let out = tool
        .execute(
            houyicoder_api::tool::ToolCtx::new("c1"),
            serde_json::json!({
                "pattern": "MATCH",
                "output_mode": "content",
                "-B": 1,
                "-A": 1
            }),
        )
        .await
        .expect("execute ok");
    let content = out.get("content").and_then(|v| v.as_str()).unwrap_or("");
    assert!(
        content.contains("a.rs:2: MATCH"),
        "match row uniform: {content}"
    );
    assert!(
        content.contains("a.rs:1: line1"),
        "before-context row uniform: {content}"
    );
    assert!(
        content.contains("a.rs:3: line3"),
        "after-context row uniform: {content}"
    );
    assert!(!content.contains("-2-"), "no rg dash separator: {content}");
    assert!(!content.contains("-1-"), "no rg dash on context: {content}");
    assert!(
        !content.contains("/Users/"),
        "no absolute path leakage: {content}"
    );
    fs::remove_dir_all(&dir).ok();
}

/// A mid-run cancel kills the rg child immediately so CPU stops, instead of
/// the spawn_blocking orphan running to completion. The rg child is pointed
/// at a FIFO with no writer, so it blocks on read forever and would never
/// return on its own; after cancel the child must be dead (no orphan pgrep
/// match) and the execute must resolve within 2s with an interrupted error.
/// This discriminates a subprocess impl that wires cancel to child.kill()
/// from one that does not. Requires the rg binary on PATH.
#[cfg(unix)]
#[tokio::test]
async fn test_stop_subprocess_on_cancel() {
    use std::time::{Duration, Instant};
    if crate::agent::tools::subprocess_util::find_rg().is_none() {
        eprintln!("stop_subprocess_on_cancel: rg not on PATH; skipping");
        return;
    }
    let dir = test_dir();
    // A FIFO target with no writer blocks rg on read forever, so a non-killed
    // child would linger — the discriminator for kill-on-cancel.
    let fifo = dir.join("blocker.fifo");
    let mk = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .expect("mkfifo");
    assert!(mk.success(), "mkfifo failed");
    let session = std::sync::Arc::new(StubSession { root: dir.clone() })
        as std::sync::Arc<dyn houyicoder_api::sandbox::SandboxSession>;
    let tool = GrepTool::new(session);
    let token = tokio_util::sync::CancellationToken::new();
    let ctx = houyicoder_api::tool::ToolCtx::new("c1").with_cancel(token.clone());
    let handle = tokio::spawn(async move {
        tool.execute(
            ctx,
            serde_json::json!({"pattern": "match", "path": "blocker.fifo"}),
        )
        .await
    });
    // Give rg time to spawn and block on the FIFO read.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let started = Instant::now();
    token.cancel();
    let res = tokio::time::timeout(Duration::from_secs(2), handle).await;
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(2),
        "execute did not resolve promptly after cancel: {elapsed:?}"
    );
    let inner = res.expect("execute did not resolve within 2s after cancel");
    let err = inner
        .expect("execute task panicked")
        .expect_err("expected interrupted error");
    assert!(err.to_string().contains("interrupted"), "{err}");
    // No orphan rg process should linger for our FIFO target. pgrep -f matches
    // the full command line; the FIFO path is unique per test, so only a
    // non-killed rg child would match.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let out = std::process::Command::new("pgrep")
        .args(["-f", &fifo.to_string_lossy()])
        .output()
        .expect("pgrep");
    assert!(
        out.stdout.is_empty(),
        "orphan rg still running for {}: {}",
        fifo.display(),
        String::from_utf8_lossy(&out.stdout)
    );
    fs::remove_dir_all(&dir).ok();
}

/// The caller's select! can drop the tool future before its own cancel-branch
/// child.kill await is ever polled: an Esc fires the loop's cancelled branch
/// at once, and the tool future is dropped in the same cycle it would have
/// observed the token. Without kill_on_drop the Child handle detaches on drop
/// and rg keeps running — the exact CPU leak this fix closes. This test
/// matches that drop path: race the execute in a select! whose cancel branch
/// wins and drops the future, then assert no orphan rg lingers. Requires rg.
#[cfg(unix)]
#[tokio::test]
async fn test_stop_subprocess_on_drop() {
    use std::time::{Duration, Instant};
    if crate::agent::tools::subprocess_util::find_rg().is_none() {
        eprintln!("stop_subprocess_on_drop: rg not on PATH; skipping");
        return;
    }
    let dir = test_dir();
    let fifo = dir.join("dropper.fifo");
    let mk = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .expect("mkfifo");
    assert!(mk.success(), "mkfifo failed");
    let session = std::sync::Arc::new(StubSession { root: dir.clone() })
        as std::sync::Arc<dyn houyicoder_api::sandbox::SandboxSession>;
    let tool = GrepTool::new(session);
    let token = tokio_util::sync::CancellationToken::new();
    let ctx = houyicoder_api::tool::ToolCtx::new("c1").with_cancel(token.clone());
    let exec_fut = tool.execute(
        ctx,
        serde_json::json!({"pattern": "match", "path": "dropper.fifo"}),
    );
    // Pin the future so we can re-poll it inside the racing select!.
    tokio::pin!(exec_fut);
    // Let rg spawn and settle onto the FIFO read.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let started = Instant::now();
    // Cancel, then race: the cancelled branch is ready on the first poll,
    // while exec_fut still needs several cycles to run its internal
    // child.kill + wait + drain. So cancelled wins and drops exec_fut
    // mid-cleanup — the path that leaks an orphan without kill_on_drop.
    token.cancel();
    tokio::select! {
        _ = token.cancelled() => (),
        _ = &mut exec_fut => (),
    };
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(2),
        "select! did not resolve promptly: {elapsed:?}"
    );
    // Without kill_on_drop, the dropped Child detaches and rg keeps blocking
    // on the FIFO; the unique path means only an orphan would match pgrep.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let out = std::process::Command::new("pgrep")
        .args(["-f", &fifo.to_string_lossy()])
        .output()
        .expect("pgrep");
    assert!(
        out.stdout.is_empty(),
        "orphan rg survived future drop for {}: {}",
        fifo.display(),
        String::from_utf8_lossy(&out.stdout)
    );
    fs::remove_dir_all(&dir).ok();
}

/// The StubSession's unused trait methods return Unsupported / Ok(false) —
/// exercised so the trait surface stays honest and covered.
#[tokio::test]
async fn test_stub_unused_return_unsupported() {
    use houyicoder_api::sandbox::SandboxSession;
    let dir = test_dir();
    let s = StubSession { root: dir.clone() };
    assert!(s.exec("git status").await.is_err());
    assert!(s.read_file("x", 10).await.is_err());
    assert!(s.write_file("x", vec![]).await.is_err());
    assert!(s.list_dir(".").await.is_err());
    assert!(!s.path_exists("nope").await.unwrap_or(false));
    fs::remove_dir_all(&dir).ok();
}
