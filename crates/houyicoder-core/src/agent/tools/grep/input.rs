//! The tool's input contract: the JSON arguments a model sends, parsed into
//! the options the search runs with.
//!
//! Numbers and booleans are accepted in their string spellings because a
//! model occasionally quotes them. Context flags follow the search-tool
//! convention: -C / context overrides -A and -B.

use serde_json::Value;

use houyicoder_protocol::extension::ToolError;

/// Default head_limit when unspecified (broad patterns can flood context).
pub(super) const DEFAULT_HEAD_LIMIT: usize = 250;

/// Parsed grep input -- extracted so the logic is unit-testable without a
/// sandbox session.
pub(super) struct GrepOptions {
    pub(super) pattern: String,
    pub(super) path: Option<String>,
    pub(super) glob_filter: Option<String>,
    pub(super) output_mode: OutputMode,
    pub(super) context_before: usize,
    pub(super) context_after: usize,
    pub(super) head_limit: usize,
    pub(super) offset: usize,
    pub(super) multiline: bool,
    pub(super) case_insensitive: bool,
    pub(super) show_line_numbers: bool,
    pub(super) file_type: Option<String>,
}

#[derive(Clone, Copy, Default)]
pub(super) enum OutputMode {
    Content,
    #[default]
    FilesWithMatches,
    Count,
}

impl GrepOptions {
    pub(super) fn from_input(input: &Value) -> Result<Self, ToolError> {
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
pub(super) fn as_number(v: &Value) -> Option<u64> {
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
pub(super) fn as_bool(v: &Value) -> Option<bool> {
    v.as_bool().or_else(|| {
        v.as_str().and_then(|s| match s.to_lowercase().as_str() {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        })
    })
}
