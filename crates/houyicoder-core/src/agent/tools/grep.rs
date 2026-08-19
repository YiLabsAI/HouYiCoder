//! File content search tool — regex search across the workspace.
//! Built on walkdir + the regex crate, in-process. Read-only and
//! concurrency-safe. Output paths relativized to the workspace root; VCS +
//! build-output dirs excluded. Three modes: content, files_with_matches,
//! count. A head_limit caps output; the isolate seam later makes large
//! outputs lossless via a CAS block ref.

use std::collections::HashSet;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use glob::Pattern;
use houyicoder_api::sandbox::SandboxSession;
use houyicoder_async::PFut;
use regex::Regex;
use serde_json::{Value, json};
use walkdir::WalkDir;

use super::path_util::{canonical_root, confine_path, relativize};
use super::{Tool, ToolCtx, ToolError};

/// VCS metadata directories excluded from search to reduce noise.
const VCS_DIRS: &[&str] = &[".git", ".svn", ".hg", ".bzr", ".jj", ".sl"];

/// Build-output / dependency directories pruned from the walk so a search
/// does not descend into target/, node_modules/, etc (generated/transient
/// artifacts that dwarf the source tree).
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

/// Default head_limit when unspecified (broad patterns can flood context).
const DEFAULT_HEAD_LIMIT: usize = 250;

/// Max chars shown per matching line in content mode (minified/base64 truncated).
const MAX_COLUMNS: usize = 500;

/// Max bytes of a file the searcher reads (source search, not log mining).
const MAX_FILE_BYTES: u64 = 1024 * 1024;

/// A content search tool backed by the sandbox workspace.
pub struct GrepTool {
    session: Arc<dyn SandboxSession>,
}

impl GrepTool {
    pub fn new(session: Arc<dyn SandboxSession>) -> Self {
        Self { session }
    }
}

impl Tool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }
    fn description(&self) -> &str {
        "Search file contents with regex. \
         Input: {pattern, path?, glob?, output_mode?, -A?, -B?, -C?, context?, \
         head_limit?, offset?, multiline?, -i?, -n?, type?}. \
         output_mode: content | files_with_matches | count (default). \
         head_limit default 250, 0 = unlimited. offset default 0. \
         -n default true (content mode). VCS dirs excluded."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string"},
                "path": {"type": "string"},
                "glob": {"type": "string"},
                "output_mode": {"type": "string", "enum": ["content", "files_with_matches", "count"]},
                "-A": {"type": "number"},
                "-B": {"type": "number"},
                "-C": {"type": "number"},
                "context": {"type": "number"},
                "head_limit": {"type": "number"},
                "offset": {"type": "number"},
                "multiline": {"type": "boolean"},
                "-i": {"type": "boolean"},
                "-n": {"type": "boolean"},
                "type": {"type": "string"}
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
            let opts = GrepOptions::from_input(&input)?;
            subprocess::dispatch_grep(root, dirs, opts, cancel).await
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

/// Parsed grep input — extracted so the logic is unit-testable without a
/// sandbox session.
struct GrepOptions {
    pattern: String,
    path: Option<String>,
    glob_filter: Option<String>,
    output_mode: OutputMode,
    context_before: usize,
    context_after: usize,
    head_limit: usize,
    offset: usize,
    multiline: bool,
    case_insensitive: bool,
    show_line_numbers: bool,
    file_type: Option<String>,
}

#[derive(Clone, Copy, Default)]
enum OutputMode {
    Content,
    #[default]
    FilesWithMatches,
    Count,
}

impl GrepOptions {
    fn from_input(input: &Value) -> Result<Self, ToolError> {
        let pattern = input
            .get("pattern")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidInput("grep: pattern (string) required".into()))?
            .to_string();
        let path = input.get("path").and_then(|v| v.as_str()).map(String::from);
        let glob_filter = input.get("glob").and_then(|v| v.as_str()).map(String::from);
        let output_mode = match input.get("output_mode").and_then(|v| v.as_str()) {
            Some("content") => OutputMode::Content,
            Some("count") => OutputMode::Count,
            Some("files_with_matches") => OutputMode::FilesWithMatches,
            _ => OutputMode::default(),
        };
        // -C / context takes precedence over -A/-B. Semantic number accepts
        // both numeric and string-numeric values so a model that sends "3"
        // as a string is handled gracefully.
        let context = input
            .get("context")
            .or_else(|| input.get("-C"))
            .and_then(as_number)
            .map(|v| v as usize);
        let (context_before, context_after) = if let Some(c) = context {
            (c, c)
        } else {
            let before = input
                .get("-B")
                .and_then(as_number)
                .map(|v| v as usize)
                .unwrap_or(0);
            let after = input
                .get("-A")
                .and_then(as_number)
                .map(|v| v as usize)
                .unwrap_or(0);
            (before, after)
        };
        let head_limit = input
            .get("head_limit")
            .and_then(as_number)
            .map(|v| v as usize)
            .unwrap_or(DEFAULT_HEAD_LIMIT);
        let offset = input
            .get("offset")
            .and_then(as_number)
            .map(|v| v as usize)
            .unwrap_or(0);
        let multiline = input.get("multiline").and_then(as_bool).unwrap_or(false);
        let case_insensitive = input.get("-i").and_then(as_bool).unwrap_or(false);
        let show_line_numbers = input.get("-n").and_then(as_bool).unwrap_or(true);
        let file_type = input.get("type").and_then(|v| v.as_str()).map(String::from);
        Ok(Self {
            pattern,
            path,
            glob_filter,
            output_mode,
            context_before,
            context_after,
            head_limit,
            offset,
            multiline,
            case_insensitive,
            show_line_numbers,
            file_type,
        })
    }
}

/// Coerce a JSON value to a number (int/float/string; negatives rejected).
fn as_number(v: &Value) -> Option<u64> {
    v.as_u64()
        .or_else(|| v.as_f64().filter(|f| *f >= 0.0).map(|f| f as u64))
        .or_else(|| {
            v.as_str().and_then(|s| {
                s.parse::<u64>().ok().or_else(|| {
                    s.parse::<f64>()
                        .ok()
                        .filter(|f| *f >= 0.0)
                        .map(|f| f as u64)
                })
            })
        })
}

/// Coerce a JSON value to a bool (boolean or string form).
fn as_bool(v: &Value) -> Option<bool> {
    v.as_bool().or_else(|| {
        v.as_str().and_then(|s| match s.to_lowercase().as_str() {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        })
    })
}

/// Collected grep results before JSON serialization.
struct GrepOutput {
    mode: OutputMode,
    filenames: Vec<String>,
    content: String,
    num_files: usize,
    num_lines: usize,
    num_matches: usize,
    truncated: bool,
    applied_limit: Option<usize>,
    applied_offset: Option<usize>,
}

impl GrepOutput {
    fn to_json(&self) -> Value {
        let mode_str = match self.mode {
            OutputMode::Content => "content",
            OutputMode::FilesWithMatches => "files_with_matches",
            OutputMode::Count => "count",
        };
        let mut v = json!({
            "mode": mode_str,
            "filenames": self.filenames,
            "num_files": self.num_files,
            "content": self.content,
        });
        if self.truncated {
            v["truncated"] = json!(true);
        }
        if let Some(limit) = self.applied_limit {
            v["applied_limit"] = json!(limit);
        }
        if let Some(offset) = self.applied_offset {
            v["applied_offset"] = json!(offset);
        }
        match self.mode {
            OutputMode::Content => {
                v["num_lines"] = json!(self.num_lines);
            }
            OutputMode::Count => {
                v["num_matches"] = json!(self.num_matches);
            }
            _ => {}
        }
        v
    }
}

/// Run the grep search (pure, no sandbox I/O — unit-testable).
fn run_grep(root: &Path, dirs: &[String], opts: &GrepOptions) -> Result<GrepOutput, ToolError> {
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
                    let mtime = std::fs::metadata(path)
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
    let mut builder = regex::RegexBuilder::new(pattern);
    builder.case_insensitive(case_insensitive);
    if multiline {
        builder.dot_matches_new_line(true);
    }
    builder
        .build()
        .map_err(|e| ToolError::Failed(format!("grep: invalid regex: {e}")))
}

/// Compile an optional glob filter into patterns (whitespace/comma split,
/// brace-enclosed patterns like *.{ts,tsx} preserved).
fn compile_glob_filter(filter: Option<&str>) -> Result<Vec<Pattern>, ToolError> {
    let Some(f) = filter else {
        return Ok(Vec::new());
    };
    split_glob_patterns(f)
        .iter()
        .map(|p| {
            Pattern::new(p)
                .map_err(|e| ToolError::Failed(format!("grep: invalid glob filter: {e}")))
        })
        .collect()
}

/// Split a glob filter into patterns (whitespace then comma split; brace
/// patterns kept whole).
fn split_glob_patterns(s: &str) -> Vec<String> {
    let mut result = Vec::new();
    for raw in s.split_whitespace() {
        if raw.contains('{') && raw.contains('}') {
            result.push(raw.to_string());
        } else {
            for part in raw.split(',') {
                if !part.is_empty() {
                    result.push(part.to_string());
                }
            }
        }
    }
    result
}

/// Check if a walkdir entry is a VCS or build-output dir to prune.
fn is_vcs_dir(entry: &walkdir::DirEntry, vcs: &HashSet<&str>) -> bool {
    if !entry.file_type().is_dir() {
        return false;
    }
    entry
        .file_name()
        .to_str()
        .map(|name| vcs.contains(name) || IGNORE_DIRS.contains(&name))
        .unwrap_or(false)
}

/// True when the file passes the glob filter (or when no filter is set).
/// The filter matches the filename or relative path (multiple OR-matched).
fn passes_glob(path: &Path, base: &Path, root: &Path, patterns: &[Pattern]) -> bool {
    if patterns.is_empty() {
        return true;
    }
    let fname = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if patterns.iter().any(|p| p.matches(fname)) {
        return true;
    }
    let rel = path
        .strip_prefix(base)
        .or_else(|_| path.strip_prefix(root))
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_string_lossy().into_owned());
    patterns.iter().any(|p| p.matches(&rel))
}

/// Map a file type name to its extensions (in-process fallback, no
/// external binary with built-in type definitions).
fn type_extensions(t: &str) -> &'static [&'static str] {
    match t {
        "rust" => &["rs"],
        "js" | "javascript" => &["js", "mjs", "cjs"],
        "ts" | "typescript" => &["ts", "tsx"],
        "py" | "python" => &["py"],
        "go" => &["go"],
        "java" => &["java"],
        "c" => &["c", "h"],
        "cpp" | "c++" => &["cpp", "cc", "cxx", "hpp", "hxx"],
        "rb" | "ruby" => &["rb"],
        "php" => &["php"],
        "swift" => &["swift"],
        "kt" | "kotlin" => &["kt", "kts"],
        "md" | "markdown" => &["md"],
        "json" => &["json"],
        "yaml" | "yml" => &["yaml", "yml"],
        "toml" => &["toml"],
        "sh" | "shell" | "bash" => &["sh", "bash"],
        _ => &[],
    }
}

/// True when the file passes the type filter (extension matches one of the
/// type's extensions). When no type filter is set, all files pass.
fn passes_type(path: &Path, exts: Option<&[&str]>) -> bool {
    let Some(exts) = exts else {
        return true;
    };
    if exts.is_empty() {
        return true; // unknown type: do not filter
    }
    path.extension()
        .and_then(|e| e.to_str())
        .map(|ext| exts.contains(&ext))
        .unwrap_or(false)
}

/// True when the file exceeds the max readable size.
fn file_too_large(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|m| m.len() > MAX_FILE_BYTES)
        .unwrap_or(true)
}

/// Search a file in content mode: collect matching lines with line numbers
/// + optional context. Long lines truncated to MAX_COLUMNS.
fn search_content(
    path: &Path,
    root: &Path,
    re: &Regex,
    before: usize,
    after: usize,
    show_line_numbers: bool,
    out: &mut Vec<String>,
) -> Result<usize, ToolError> {
    let file = std::fs::File::open(path)
        .map_err(|e| ToolError::Failed(format!("grep: cannot open {}: {e}", path.display())))?;
    let reader = BufReader::new(file);
    let rel = relativize(root, path);
    let mut lines_buf: Vec<(usize, String)> = Vec::new();
    let mut match_count = 0usize;
    let mut after_remaining = 0usize;

    let fmt = |ln: usize, text: &str| -> String {
        let t = truncate_line(text);
        if show_line_numbers {
            format!("{rel}:{ln}:{t}")
        } else {
            format!("{rel}:{t}")
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
    let file = std::fs::File::open(path)
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
    let file = std::fs::File::open(path)
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
    std::fs::read_to_string(path)
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
fn truncate_line(s: &str) -> String {
    let count = s.chars().count();
    if count <= MAX_COLUMNS {
        return s.to_string();
    }
    let truncated: String = s.chars().take(MAX_COLUMNS).collect();
    format!("{truncated}...")
}

/// Assemble the final GrepOutput, applying offset + head_limit. The
/// applied_limit/applied_offset fields are set only when truncation
/// occurred, so the model knows it can paginate.
fn build_output(
    opts: &GrepOptions,
    content_lines: Vec<String>,
    match_files: Vec<String>,
    count_lines: Vec<String>,
    truncated: bool,
) -> GrepOutput {
    let offset = opts.offset;
    let limit = opts.head_limit;
    match opts.output_mode {
        OutputMode::Content => {
            let (lines, was_truncated) = apply_limit(content_lines, limit, offset);
            let applied_limit = if was_truncated || truncated {
                Some(limit)
            } else {
                None
            };
            let applied_offset = if offset > 0 { Some(offset) } else { None };
            GrepOutput {
                mode: OutputMode::Content,
                filenames: Vec::new(),
                content: lines.join("\n"),
                num_files: 0,
                num_lines: lines.len(),
                num_matches: 0,
                truncated: was_truncated || truncated || applied_limit.is_some(),
                applied_limit,
                applied_offset,
            }
        }
        OutputMode::FilesWithMatches => {
            let (files, was_truncated) = apply_limit(match_files, limit, offset);
            let applied_limit = if was_truncated || truncated {
                Some(limit)
            } else {
                None
            };
            let applied_offset = if offset > 0 { Some(offset) } else { None };
            let num_files = files.len();
            GrepOutput {
                mode: OutputMode::FilesWithMatches,
                filenames: files,
                content: String::new(),
                num_files,
                num_lines: 0,
                num_matches: 0,
                truncated: was_truncated || truncated,
                applied_limit,
                applied_offset,
            }
        }
        OutputMode::Count => {
            let (lines, was_truncated) = apply_limit(count_lines, limit, offset);
            let applied_limit = if was_truncated || truncated {
                Some(limit)
            } else {
                None
            };
            let applied_offset = if offset > 0 { Some(offset) } else { None };
            // Sum counts from the limited lines (post-apply_limit) so
            // num_matches matches what the model sees. Lines: "path:count".
            let num_matches: usize = lines
                .iter()
                .filter_map(|l| l.rsplit(':').next().and_then(|c| c.parse::<usize>().ok()))
                .sum();
            GrepOutput {
                mode: OutputMode::Count,
                filenames: Vec::new(),
                content: lines.join("\n"),
                num_files: lines.len(),
                num_lines: 0,
                num_matches,
                truncated: was_truncated || truncated,
                applied_limit,
                applied_offset,
            }
        }
    }
}

/// Apply offset and head_limit (0 = unlimited). Returns sliced items + a
/// truncated flag (true only when items were dropped).
fn apply_limit<T: Clone>(items: Vec<T>, limit: usize, offset: usize) -> (Vec<T>, bool) {
    let total = items.len();
    if limit == 0 {
        let skipped: Vec<T> = items.into_iter().skip(offset).collect();
        let was_truncated = offset > 0 && total > offset + skipped.len();
        return (skipped, was_truncated);
    }
    let was_truncated = total.saturating_sub(offset) > limit;
    let sliced: Vec<T> = items.into_iter().skip(offset).take(limit).collect();
    (sliced, was_truncated)
}

#[cfg(test)]
#[path = "grep_tests.rs"]
mod tests;

mod subprocess;
