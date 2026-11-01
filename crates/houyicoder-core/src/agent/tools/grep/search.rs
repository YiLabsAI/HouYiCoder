//! The in-process search: walk the workspace, test each file's content, and
//! collect the lines or file names the requested mode reports.
//!
//! Content mode adds surrounding lines and line numbers; whole-file mode
//! reads the file once so a pattern can span lines; count mode sums the
//! matches per file. The walk stops early once the requested page is full.

use std::collections::HashSet;
use std::fs::File;
use std::fs::metadata;
use std::fs::read_to_string;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use regex::{Regex, RegexBuilder};
use walkdir::WalkDir;

use crate::agent::tools::ToolError;
use crate::agent::tools::path_util::{canonical_root, confine_path, relativize};

use super::filters::{
    VCS_DIRS, compile_glob_filter, is_vcs_dir, passes_glob, passes_type, type_extensions,
};
use super::input::{GrepOptions, OutputMode};
use super::output::{GrepOutput, build_output};

/// Max chars shown per matching line in content mode (minified/base64 truncated).
const MAX_COLUMNS: usize = 500;

/// Max bytes of a file the searcher reads (source search, not log mining).
const MAX_FILE_BYTES: u64 = 1024 * 1024;

/// Run the grep search (pure, no sandbox I/O -- unit-testable).
pub(super) fn run_grep(
    root: &Path,
    dirs: &[String],
    opts: &GrepOptions,
) -> Result<GrepOutput, ToolError> {
    let re = build_regex(&opts.pattern, opts.case_insensitive, opts.multiline)?;
    let base = match &opts.path {
        Some(p) => confine_path(root, dirs, p)?,
        None => canonical_root(root)?,
    };
    // Canonicalize root for relativization. Walkdir paths are under the
    // canonicalized base; if root has a symlink (e.g. /var -> /private/var
    // on macOS), strip_prefix(root, path) fails and returns the full
    // absolute path, wasting tokens.
    let croot = canonical_root(root)?;
    let glob_pats = compile_glob_filter(opts.glob_filter.as_deref())?;
    let vcs_set: HashSet<&str> = VCS_DIRS.iter().copied().collect();
    let type_exts = opts.file_type.as_deref().map(type_extensions);

    let mut content_lines: Vec<String> = Vec::new();
    let mut match_files_raw: Vec<(PathBuf, SystemTime)> = Vec::new();
    let mut count_lines: Vec<String> = Vec::new();
    let mut truncated = false;

    for entry in WalkDir::new(&base)
        .into_iter()
        .filter_entry(|e| !is_vcs_dir(e, &vcs_set))
        .filter_map(|r| r.ok())
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if !passes_glob(path, &base, &croot, &glob_pats) {
            continue;
        }
        if !passes_type(path, type_exts) {
            continue;
        }
        if file_too_large(path) {
            continue;
        }
        let matches = match (opts.output_mode, opts.multiline) {
            (OutputMode::Content, true) => {
                search_content_whole(path, &croot, &re, &mut content_lines)
            }
            (OutputMode::Content, false) => search_content(
                path,
                &croot,
                &re,
                opts.context_before,
                opts.context_after,
                opts.show_line_numbers,
                &mut content_lines,
            ),
            (OutputMode::FilesWithMatches, true) => search_matches_whole(path, &re),
            (OutputMode::FilesWithMatches, false) => search_file_matches(path, &re),
            (OutputMode::Count, true) => search_count_whole(path, &re, &mut count_lines, &croot),
            (OutputMode::Count, false) => search_count(path, &re, &mut count_lines, &croot),
        };
        if let Ok(n) = matches
            && n > 0
        {
            match opts.output_mode {
                OutputMode::Content => {
                    // Apply head_limit early so we stop collecting and
                    // relativizing lines that will be discarded (broad
                    // patterns can return thousands of lines).
                    if opts.head_limit != 0 && content_lines.len() >= opts.offset + opts.head_limit
                    {
                        truncated = true;
                        break;
                    }
                }
                OutputMode::FilesWithMatches => {
                    // Collect ALL matching files; sort by mtime desc happens
                    // after the walk, then apply_limit in build_output caps
                    // the count. Early-exit here would keep only the first N
                    // in walkdir order, dropping newest files visited last.
                    let mtime = metadata(path)
                        .and_then(|m| m.modified())
                        .unwrap_or(SystemTime::UNIX_EPOCH);
                    match_files_raw.push((path.to_path_buf(), mtime));
                }
                OutputMode::Count => {
                    if opts.head_limit != 0 && count_lines.len() >= opts.offset + opts.head_limit {
                        truncated = true;
                        break;
                    }
                }
            }
        }
    }

    // Sort files_with_matches by mtime desc (newest first). Filename is the
    // tiebreaker for determinism when mtimes are equal (common in tests).
    match_files_raw.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| a.0.to_string_lossy().cmp(&b.0.to_string_lossy()))
    });
    let match_files: Vec<String> = match_files_raw
        .into_iter()
        .map(|(p, _)| relativize(&croot, &p))
        .collect();

    Ok(build_output(
        opts,
        content_lines,
        match_files,
        count_lines,
        truncated,
    ))
}

/// Build the regex from the pattern, honoring case-insensitive and multiline.
fn build_regex(pattern: &str, case_insensitive: bool, multiline: bool) -> Result<Regex, ToolError> {
    let mut builder = RegexBuilder::new(pattern);
    builder.case_insensitive(case_insensitive);
    if multiline {
        builder.dot_matches_new_line(true);
    }
    builder
        .build()
        .map_err(|e| ToolError::Failed(format!("grep: invalid regex: {e}")))
}

/// True when the file exceeds the max readable size.
fn file_too_large(path: &Path) -> bool {
    metadata(path)
        .map(|m| m.len() > MAX_FILE_BYTES)
        .unwrap_or(true)
}

/// Search a file in content mode: collect matching lines with line numbers
/// plus optional context. Long lines truncated to MAX_COLUMNS.
fn search_content(
    path: &Path,
    root: &Path,
    re: &Regex,
    before: usize,
    after: usize,
    show_line_numbers: bool,
    out: &mut Vec<String>,
) -> Result<usize, ToolError> {
    let file = File::open(path)
        .map_err(|e| ToolError::Failed(format!("grep: cannot open {}: {e}", path.display())))?;
    let reader = BufReader::new(file);
    let rel = relativize(root, path);
    let mut lines_buf: Vec<(usize, String)> = Vec::new();
    let mut match_count = 0usize;
    let mut after_remaining = 0usize;

    let fmt = |ln: usize, text: &str| -> String {
        let t = truncate_line(text);
        if show_line_numbers {
            format!("{rel}:{ln}: {t}")
        } else {
            format!("{rel}: {t}")
        }
    };

    for (idx, line) in reader.lines().enumerate() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue, // non-utf8 line: skip, keep reading
        };
        let line_no = idx + 1;
        let is_match = re.is_match(&line);
        if is_match {
            match_count += 1;
            let start = lines_buf.len().saturating_sub(before);
            for (ln, content) in &lines_buf[start..] {
                out.push(fmt(*ln, content));
            }
            lines_buf.clear();
            out.push(fmt(line_no, &line));
            after_remaining = after;
        } else if after_remaining > 0 {
            out.push(fmt(line_no, &line));
            after_remaining -= 1;
        } else {
            lines_buf.push((line_no, line));
            if lines_buf.len() > before + 1 {
                lines_buf.remove(0);
            }
        }
    }
    Ok(match_count)
}

/// Search a file for at least one match (files_with_matches mode). Returns 1
/// if there is a match, 0 otherwise. Stops at the first match.
fn search_file_matches(path: &Path, re: &Regex) -> Result<usize, ToolError> {
    let file = File::open(path)
        .map_err(|e| ToolError::Failed(format!("grep: cannot open {}: {e}", path.display())))?;
    let reader = BufReader::new(file);
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue, // non-utf8: skip line, keep reading
        };
        if re.is_match(&line) {
            return Ok(1);
        }
    }
    Ok(0)
}

/// Search a file and count matches (count mode). Pushes a line
/// "relative_path:count" to out. Returns the match count.
fn search_count(
    path: &Path,
    re: &Regex,
    out: &mut Vec<String>,
    root: &Path,
) -> Result<usize, ToolError> {
    let file = File::open(path)
        .map_err(|e| ToolError::Failed(format!("grep: cannot open {}: {e}", path.display())))?;
    let reader = BufReader::new(file);
    let rel = relativize(root, path);
    let mut count = 0usize;
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue, // non-utf8: skip line, keep reading
        };
        count += re.find_iter(&line).count();
    }
    if count > 0 {
        out.push(format!("{rel}:{count}"));
    }
    Ok(count)
}

/// Read the entire file into a single string (for multiline search).
fn read_whole_file(path: &Path) -> Result<String, ToolError> {
    read_to_string(path)
        .map_err(|e| ToolError::Failed(format!("grep: cannot read {}: {e}", path.display())))
}

/// Compute the 1-based line number of a byte offset within content.
fn line_at(content: &str, offset: usize) -> usize {
    content[..offset].matches('\n').count() + 1
}

/// Multiline content search: read the whole file, find all matches with
/// find_iter, push each match with its starting line number. Context lines
/// are not expanded in multiline mode (matches may span many lines).
fn search_content_whole(
    path: &Path,
    root: &Path,
    re: &Regex,
    out: &mut Vec<String>,
) -> Result<usize, ToolError> {
    let content = read_whole_file(path)?;
    let rel = relativize(root, path);
    let mut count = 0;
    for m in re.find_iter(&content) {
        count += 1;
        let line_no = line_at(&content, m.start());
        let text = truncate_line(m.as_str());
        out.push(format!("{rel}:{line_no}:{text}"));
    }
    Ok(count)
}

/// Multiline files_with_matches: check the whole file for at least one match.
fn search_matches_whole(path: &Path, re: &Regex) -> Result<usize, ToolError> {
    let content = read_whole_file(path)?;
    Ok(if re.is_match(&content) { 1 } else { 0 })
}

/// Multiline count search: count all matches in the whole file.
fn search_count_whole(
    path: &Path,
    re: &Regex,
    out: &mut Vec<String>,
    root: &Path,
) -> Result<usize, ToolError> {
    let content = read_whole_file(path)?;
    let rel = relativize(root, path);
    let count = re.find_iter(&content).count();
    if count > 0 {
        out.push(format!("{rel}:{count}"));
    }
    Ok(count)
}

/// Truncate a line to MAX_COLUMNS characters for display.
pub(super) fn truncate_line(s: &str) -> String {
    let count = s.chars().count();
    if count <= MAX_COLUMNS {
        return s.to_string();
    }
    let truncated: String = s.chars().take(MAX_COLUMNS).collect();
    format!("{truncated}...")
}
