//! Which directories the walk skips and which files a glob or type filter
//! admits.
//!
//! Version-control metadata and generated-output trees are pruned before the
//! walk descends, because they dwarf the source tree and their contents are
//! never what a content search is for.

use std::collections::HashSet;
use std::path::Path;

use glob::Pattern;
use walkdir::DirEntry;

use houyicoder_protocol::extension::ToolError;

/// VCS metadata directories excluded from search to reduce noise.
pub(super) const VCS_DIRS: &[&str] = &[".git", ".svn", ".hg", ".bzr", ".jj", ".sl"];

/// Build-output / dependency directories pruned from the walk so a search
/// does not descend into target/, node_modules/, etc (generated/transient
/// artifacts that dwarf the source tree).
pub(super) const IGNORE_DIRS: &[&str] = &[
    "target",
    "node_modules",
    "dist",
    "build",
    ".next",
    ".cache",
    ".houyicoder",
    ".claude",
];

/// Compile an optional glob filter into patterns (whitespace/comma split,
/// brace-enclosed patterns like *.{ts,tsx} preserved).
pub(super) fn compile_glob_filter(filter: Option<&str>) -> Result<Vec<Pattern>, ToolError> {
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
pub(super) fn split_glob_patterns(s: &str) -> Vec<String> {
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
pub(super) fn is_vcs_dir(entry: &DirEntry, vcs: &HashSet<&str>) -> bool {
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
pub(super) fn passes_glob(path: &Path, base: &Path, root: &Path, patterns: &[Pattern]) -> bool {
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
pub(super) fn type_extensions(t: &str) -> &'static [&'static str] {
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
pub(super) fn passes_type(path: &Path, exts: Option<&[&str]>) -> bool {
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
