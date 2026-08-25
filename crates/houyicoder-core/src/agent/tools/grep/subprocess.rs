//! Subprocess-backed grep: shell to the rg binary so an Esc cancel kills
//! the child process immediately, stopping CPU at once. The spawn_blocking
//! traversal, by contrast, keeps running to completion as an orphan task
//! after the select! short-circuits the caller. When the rg binary is not
//! on PATH the function returns None and the caller falls back to the
//! in-process traversal.
//!
//! Output is parsed into the same GrepOutput shape the in-process path
//! produces, so the brief layer and downstream callers see no difference.
//! rg respects .gitignore by default, so a gitignored file (for example a
//! log file outside the build-output prune list) is skipped here but would
//! be scanned by the in-process fallback; that behavioral gap is the
//! discriminator the kill-on-cancel test relies on.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;
use tokio_util::sync::CancellationToken;

use houyicoder_protocol::extension::ToolError;

use crate::agent::tools::path_util::{canonical_root, confine_path};
use crate::agent::tools::subprocess_util::{find_rg, spawn_rg_select_cancel};

use super::{
    GrepOptions, GrepOutput, IGNORE_DIRS, OutputMode, VCS_DIRS, build_output, relativize, run_grep,
    split_glob_patterns,
};

/// Dispatch a grep: try the rg subprocess first (a cancel kills the child and
/// stops CPU at once), then fall back to the in-process WalkDir traversal on
/// the blocking pool when rg is not on PATH. The fallback still races the
/// cancellation token so an Esc mid-walk short-circuits, though the
/// spawn_blocking orphan keeps running to completion after the caller returns.
pub(super) async fn dispatch_grep(
    root: PathBuf,
    dirs: Vec<String>,
    opts: GrepOptions,
    cancel: Option<CancellationToken>,
) -> Result<Value, ToolError> {
    if let Some(out) = try_run_grep_subprocess(&root, &dirs, &opts, cancel.as_ref()).await? {
        return Ok(out.to_json());
    }
    let handle = tokio::task::spawn_blocking(move || run_grep(&root, &dirs, &opts));
    let output = match cancel {
        Some(token) => tokio::select! {
            _ = token.cancelled() => return Err(ToolError::Failed(
                "grep: interrupted by user".into(),
            )),
            r = handle => r.map_err(|e| ToolError::Failed(
                format!("grep: traversal join: {e}"),
            ))??,
        },
        None => handle
            .await
            .map_err(|e| ToolError::Failed(format!("grep: traversal join: {e}")))??,
    };
    Ok(output.to_json())
}

/// Try to run grep via the rg subprocess. Ok(Some) means rg ran and produced
/// output; Ok(None) means rg is not available so the caller falls back to
/// the in-process traversal; Err means rg ran but failed in a way the caller
/// should surface.
pub(super) async fn try_run_grep_subprocess(
    root: &Path,
    dirs: &[String],
    opts: &GrepOptions,
    cancel: Option<&CancellationToken>,
) -> Result<Option<GrepOutput>, ToolError> {
    let rg_path = match find_rg() {
        Some(p) => p,
        None => return Ok(None),
    };
    let target = match &opts.path {
        Some(p) => confine_path(root, dirs, p)?,
        None => canonical_root(root)?,
    };
    let args = build_rg_args(opts);
    let output = match spawn_rg_select_cancel(&rg_path, &args, &target, cancel).await? {
        Some(o) => o,
        None => return Ok(None),
    };
    let parsed = parse_rg_output(&output.stdout, opts, root)?;
    Ok(Some(parsed))
}

/// Build the rg argument list from the parsed options. Matches the in-process
/// traversal's semantics: VCS + build-output dirs pruned via --glob, hidden
/// files included, long lines capped, output flags per mode, context flags
/// in content mode, and the user glob filter applied as include globs.
fn build_rg_args(opts: &GrepOptions) -> Vec<String> {
    let mut args: Vec<String> = vec!["--hidden".into()];
    for d in VCS_DIRS {
        args.push("--glob".into());
        args.push(format!("!{d}"));
    }
    for d in IGNORE_DIRS {
        args.push("--glob".into());
        args.push(format!("!{d}"));
    }
    args.push("--max-columns".into());
    args.push("500".into());
    if opts.multiline {
        args.push("-U".into());
        args.push("--multiline-dotall".into());
    }
    if opts.case_insensitive {
        args.push("-i".into());
    }
    match opts.output_mode {
        OutputMode::FilesWithMatches => {
            args.push("-l".into());
        }
        OutputMode::Count => {
            args.push("-c".into());
        }
        OutputMode::Content => {
            // Structured JSON so the parser never has to disambiguate rg's
            // text format (colon for matches, dash for context, no path in
            // single-file mode, paths with colons). The JSON record carries
            // type + path + line_number + lines verbatim; parse_content
            // formats a consistent rel:line: content row from it.
            args.push("--json".into());
            if opts.context_before > 0 {
                args.push("-B".into());
                args.push(opts.context_before.to_string());
            }
            if opts.context_after > 0 {
                args.push("-A".into());
                args.push(opts.context_after.to_string());
            }
        }
    }
    if opts.pattern.starts_with('-') {
        args.push("-e".into());
    }
    args.push(opts.pattern.clone());
    if let Some(t) = &opts.file_type {
        args.push("--type".into());
        args.push(t.clone());
    }
    if let Some(g) = &opts.glob_filter {
        for pat in split_glob_patterns(g) {
            args.push("--glob".into());
            args.push(pat);
        }
    }
    args
}

/// Parse rg stdout into a GrepOutput that matches the in-process path's
/// shape. Dispatches per mode so the count axis (files / lines / match
/// occurrences) is picked correctly.
fn parse_rg_output(
    stdout: &[u8],
    opts: &GrepOptions,
    root: &Path,
) -> Result<GrepOutput, ToolError> {
    let croot = canonical_root(root)?;
    let text = String::from_utf8_lossy(stdout);
    let lines: Vec<&str> = text.lines().filter(|l| !l.is_empty()).collect();
    match opts.output_mode {
        OutputMode::FilesWithMatches => parse_files_with_matches(&lines, opts, &croot),
        OutputMode::Content => parse_content(&lines, opts, &croot),
        OutputMode::Count => parse_count(&lines, opts, &croot),
    }
}

/// files_with_matches: each stdout line is an absolute path. Relativize to
/// the workspace root, sort by mtime desc (newest first) with the filename
/// as the tiebreaker for determinism, then apply offset + head_limit via
/// build_output so the truncated / applied_limit fields match the
/// in-process path.
fn parse_files_with_matches(
    lines: &[&str],
    opts: &GrepOptions,
    croot: &Path,
) -> Result<GrepOutput, ToolError> {
    let mut entries: Vec<(PathBuf, SystemTime)> = Vec::with_capacity(lines.len());
    for line in lines {
        let p = PathBuf::from(line);
        let mtime = std::fs::metadata(&p)
            .and_then(|m| m.modified())
            .unwrap_or(UNIX_EPOCH);
        entries.push((p, mtime));
    }
    entries.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| a.0.to_string_lossy().cmp(&b.0.to_string_lossy()))
    });
    let match_files: Vec<String> = entries
        .into_iter()
        .map(|(p, _)| relativize(croot, &p))
        .collect();
    Ok(build_output(
        opts,
        Vec::new(),
        match_files,
        Vec::new(),
        false,
    ))
}

/// content mode: each stdout line is a rg --json record. Parse the match +
/// context records, relativize the path, and emit one rel:line: content row
/// per source line (a multiline match or context block splits across rows
/// with incrementing line numbers). begin/end/summary records are skipped.
/// The JSON avoids rg's text-format ambiguity (colon for matches, dash for
/// context, no path in single-file mode, paths that themselves contain
/// colons), so the output is uniform regardless of how rg was invoked or
/// whether context was requested.
fn parse_content(
    lines: &[&str],
    opts: &GrepOptions,
    croot: &Path,
) -> Result<GrepOutput, ToolError> {
    let mut content_lines: Vec<String> = Vec::new();
    for line in lines {
        let Ok(obj) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let typ = obj.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if !matches!(typ, "match" | "context") {
            continue;
        }
        let Some(data) = obj.get("data") else {
            continue;
        };
        let path = data
            .pointer("/path/text")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let line_no = data
            .get("line_number")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize;
        let text = data
            .pointer("/lines/text")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let rel = relativize(croot, Path::new(path));
        // lines.text may hold several source lines (a multiline match or a
        // multi-line context block); emit one row per source line, incrementing
        // the line number. trim_end drops the trailing newline rg appends so it
        // does not become a spurious empty final row.
        for (i, content) in text.trim_end_matches('\n').split('\n').enumerate() {
            let content = super::truncate_line(content);
            let ln = line_no + i;
            let row = if opts.show_line_numbers {
                format!("{rel}:{ln}: {content}")
            } else {
                format!("{rel}: {content}")
            };
            content_lines.push(row);
        }
    }
    Ok(build_output(
        opts,
        content_lines,
        Vec::new(),
        Vec::new(),
        false,
    ))
}

/// count mode: each stdout line is path:count. Relativize the path prefix,
/// then build_output sums the limited lines into num_matches.
fn parse_count(lines: &[&str], opts: &GrepOptions, croot: &Path) -> Result<GrepOutput, ToolError> {
    let count_lines: Vec<String> = lines
        .iter()
        .map(|line| match line.rfind(':') {
            Some(i) => {
                let path = &line[..i];
                let rest = &line[i..];
                let r = relativize(croot, Path::new(path));
                format!("{r}{rest}")
            }
            None => line.to_string(),
        })
        .collect();
    Ok(build_output(
        opts,
        Vec::new(),
        Vec::new(),
        count_lines,
        false,
    ))
}
