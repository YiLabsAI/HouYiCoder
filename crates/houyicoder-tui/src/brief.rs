//! Brief / truncate helpers for transcript tool rows: extract a clean
//! call-line argument (a runnable command, a file path, a pattern) from a
//! tool-call's JSON args instead of dumping raw JSON, and truncate long
//! shell one-liners so the row stays one-glance. Kept here so records.rs
//! stays under the file-size gate.

use serde_json::Value;

/// Truncate a JSON value to a short status string for the transcript. Tool
/// calls and results can carry large payloads; the transcript row only needs
/// a glance, not the full body. Kept to 60 chars so the row stays one line.
/// Char-safe (counts + slices by char, not byte) so a multi-byte payload does
/// not panic on the byte boundary.
pub(crate) fn value_brief(v: &Value) -> String {
    let s = match v {
        Value::String(s) => s.clone(),
        _ => v.to_string(),
    };
    if s.chars().count() > 60 {
        let kept: String = s.chars().take(57).collect();
        format!("{kept}…")
    } else {
        s
    }
}

/// Extract a clean, human-readable call-line argument for a tool call, so
/// the transcript shows the runnable form (e.g. Bash with the command
/// verbatim) rather than raw JSON args. Known tools delegate the field
/// selection to the shared, untruncated tool_invocation projection (so the
/// chip, the verbose view, and the search index all read the same source
/// text), then truncate to the chip budget: at most 160 chars and 2 lines so
/// a long shell one-liner stays one-glance. Unknown tools keep the original
/// 60-char value_brief glimpse — MCP tools fall here with large inputs, and a
/// 160-char 2-line dump is not a chip. The path field name matches the tool
/// schemas (which use path, not file_path) so a Write/Edit call chip shows
/// the path, not the entire input JSON (which embeds the file content).
pub(crate) fn tool_call_brief(tool: &str, input: &Value) -> String {
    match tool {
        "bash" | "read" | "write" | "edit" | "multiedit" | "grep" | "glob" => {
            truncate_call_arg(&houyicoder_protocol::tool::tool_invocation(tool, input))
        }
        "agent" => {
            let st = input
                .get("subagent_type")
                .and_then(|v| v.as_str())
                .unwrap_or("general-purpose");
            format!("→ {st}")
        }
        _ => value_brief(input),
    }
}

/// User-facing chip name for a tool call. Edit renders as
/// "Update" (or "Create" when old_string is empty — a new file), MultiEdit as
/// "Update", rather than the raw tool name; other tools use their capitalized
/// name. The name lives in the chip; the call args come from tool_call_brief.
pub(crate) fn tool_user_facing_name<'a>(tool: &'a str, input: &Value) -> &'a str {
    match tool {
        "edit" => {
            if input.get("old_string").and_then(|v| v.as_str()) == Some("") {
                "Create"
            } else {
                "Update"
            }
        }
        "multiedit" => "Update",
        _ => tool,
    }
}

/// Truncate a shell command for the chip display: collapse newlines to
/// spaces (a tool-call chip is a one-line summary, not a full-command
/// render), then cap at 160 chars with an ellipsis. Collapses multi-line
/// inputs and truncates with nowrap so the chip stays one line.
pub(crate) fn truncate_call_arg(s: &str) -> String {
    const MAX_CHARS: usize = 160;
    let collapsed: String = s
        .split('\n')
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if collapsed.chars().count() > MAX_CHARS {
        let kept: String = collapsed.chars().take(MAX_CHARS).collect();
        format!("{kept}…")
    } else {
        collapsed
    }
}

/// Grep result summary, mode-dependent: the count axis differs per mode
/// (files / lines / match-occurrences). The default files_with_matches mode
/// emits neither num_matches nor num_lines, so a naive num_matches read would
/// always be 0 — pick the axis from the mode field.
fn grep_summary(output: &Value) -> Option<String> {
    let mode = output
        .get("mode")
        .and_then(|v| v.as_str())
        .unwrap_or("files_with_matches");
    match mode {
        "content" => {
            let n = output
                .get("num_lines")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            Some(format!(
                "Found {} {}",
                n,
                if n == 1 { "line" } else { "lines" }
            ))
        }
        "count" => {
            // The chip one-liner (a search-result summary): the
            // primary axis is match count, the secondary is file count.
            let m = output
                .get("num_matches")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let f = output
                .get("num_files")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            Some(format!(
                "Found {} {} across {} {}",
                m,
                if m == 1 { "match" } else { "matches" },
                f,
                if f == 1 { "file" } else { "files" }
            ))
        }
        _ => {
            // files_with_matches (default): the count is the file count.
            let f = output
                .get("num_files")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            if f == 0 {
                Some("No files found".to_string())
            } else {
                Some(format!(
                    "Found {} {}",
                    f,
                    if f == 1 { "file" } else { "files" }
                ))
            }
        }
    }
}

/// Edit-result summary line in the canonical shape: "Added N line[s]"
/// joined with ", " to "[R|r]emoved M line[s]" — capital R only when there
/// are no additions, singular for 1. The path lives in the call chip, so it
/// is not repeated here.
pub(crate) fn edit_diff_summary(added: u32, removed: u32) -> String {
    let mut parts = Vec::new();
    if added > 0 {
        parts.push(format!(
            "Added {} {}",
            added,
            if added > 1 { "lines" } else { "line" }
        ));
    }
    if removed > 0 {
        let r = if added == 0 { "Removed" } else { "removed" };
        parts.push(format!(
            "{} {} {}",
            r,
            removed,
            if removed > 1 { "lines" } else { "line" }
        ));
    }
    parts.join(", ")
}

/// One-line collapsed-row summary for a tool result. Per-mode
/// summaries (not raw content): "Read N lines" / "Wrote N lines to
/// {path}" / grep mode-aware / AskUserQuestion TUI label. None when the body
/// IS the display (Edit/MultiEdit diff) -- those keep their raw body; the
/// content still lands below for Ctrl+O expand.
pub(crate) fn result_summary(tool: &str, output: &Value) -> Option<String> {
    match tool {
        "read" => {
            let n = output
                .get("content")
                .and_then(|v| v.as_str())
                .map(|s| s.lines().count())
                .unwrap_or(0);
            // An empty file (or a result with no content field) reads as
            // 0 lines. The bare "Read 0 lines" chip reads as a failure
            // ("nothing was read"); the empty-file qualifier disambiguates
            // a genuine empty read from a no-op.
            Some(if n == 0 {
                "Read 0 lines (empty file)".to_string()
            } else {
                format!("Read {} {}", n, if n == 1 { "line" } else { "lines" })
            })
        }
        // Grep summary is mode-dependent (see grep_summary): the default
        // files_with_matches mode emits neither num_matches nor num_lines, so
        // the count axis must be picked from the mode field.
        "grep" => grep_summary(output),
        // Glob summary follows grep's files_with_matches: the count axis is
        // the file count. A search-result summary renders
        // "Found N files" for a glob result; without this arm houyi fell
        // through to a value_brief JSON glimpse.
        "glob" => {
            let f = output
                .get("num_files")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            if f == 0 {
                Some("No files found".to_string())
            } else {
                Some(format!(
                    "Found {} {}",
                    f,
                    if f == 1 { "file" } else { "files" }
                ))
            }
        }
        "write" => {
            // "Wrote N lines to {path}" (always plural "lines"). The lines
            // field is emitted by the tool alongside bytes; without it the
            // body is the display, so return None rather than a byte count.
            let lines = output.get("lines").and_then(|v| v.as_u64());
            let path = output.get("path").and_then(|v| v.as_str()).unwrap_or("");
            lines.map(|n| format!("Wrote {} lines to {}", n, path))
        }
        "bash" => output
            .get("stdout")
            .and_then(|v| v.as_str())
            .and_then(|s| s.lines().next())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string()),
        // Transparent HITL tool: the chip is hidden, but the result row shows
        // a short label for the human. The model sees the full reject message
        // or answers via the tool_result content, so the TUI label and the
        // model-visible string are intentionally separate — do not fold one
        // into the other.
        "AskUserQuestion" => {
            if output
                .get("declined")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                Some("User declined to answer questions".to_string())
            } else if output
                .get("answered")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                let n = output
                    .get("answers")
                    .and_then(|v| v.as_object())
                    .map(|m| m.len())
                    .unwrap_or(0);
                Some(format!(
                    "User answered {} {}",
                    n,
                    if n == 1 { "question" } else { "questions" }
                ))
            } else {
                None
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_write_brief_is_path() {
        // A Write call's input embeds the file content; the chip must show
        // the path only, not dump the whole JSON (which leaks file content
        // into the transcript). Guards the field-name match against the tool
        // schema (path, not file_path).
        let input = serde_json::json!({
            "path": "src/foo.rs",
            "content": "fn main() {}\n... a lot more ..."
        });
        let brief = tool_call_brief("write", &input);
        assert_eq!(brief, "src/foo.rs");
        assert!(
            !brief.contains("content"),
            "must not dump the content field: {brief}"
        );
        assert!(!brief.contains('{'), "must not dump raw json: {brief}");
    }

    #[test]
    fn test_edit_brief_is_path() {
        let input = serde_json::json!({
            "path": "src/bar.rs",
            "old_string": "a",
            "new_string": "b"
        });
        assert_eq!(tool_call_brief("edit", &input), "src/bar.rs");
    }

    #[test]
    fn test_read_brief_is_path() {
        let input = serde_json::json!({"path": "README.md"});
        assert_eq!(tool_call_brief("read", &input), "README.md");
    }

    #[test]
    fn test_bash_brief_is_command() {
        let input = serde_json::json!({"command": "ls -la"});
        assert_eq!(tool_call_brief("bash", &input), "ls -la");
    }

    #[test]
    fn test_value_brief_multibyte() {
        // A long string whose byte boundary at 57 lands inside a multi-byte
        // char must not panic (char-safe truncation, not byte slice).
        let s = "中".repeat(100);
        let brief = value_brief(&Value::String(s));
        assert!(brief.ends_with("…"));
    }

    // Unknown tools (MCP falls here) keep the 60-char value_brief glimpse,
    // not a 160-char 2-line dump. The chip is one-glance; a long input JSON
    // is not. Pins the budget so a regression to truncate_call_arg here is
    // caught.
    #[test]
    fn test_unknown_tool_brief_glimpse() {
        let big = serde_json::json!({ "prompt": "x".repeat(200), "k": 1 });
        let brief = tool_call_brief("SomeMcpTool", &big);
        assert!(
            brief.chars().count() <= 60,
            "unknown tool chip must stay <=60 chars (value_brief), got {}: {brief}",
            brief.chars().count()
        );
        assert!(
            brief.contains('…'),
            "must be truncated with ellipsis: {brief}"
        );
        assert!(!brief.contains('\n'), "must be one line, not 2: {brief}");
    }

    #[test]
    fn test_value_brief_short_unchanged() {
        let brief = value_brief(&Value::String("hi".to_string()));
        assert_eq!(brief, "hi");
    }

    #[test]
    fn test_summary_read_lines() {
        let out = serde_json::json!({"content": "a\nb\nc"});
        assert_eq!(result_summary("read", &out), Some("Read 3 lines".into()));
        let one = serde_json::json!({"content": "only"});
        assert_eq!(result_summary("read", &one), Some("Read 1 line".into()));
    }

    #[test]
    fn test_summary_read_empty_file() {
        // An empty file yields 0 lines; the bare "Read 0 lines" chip reads
        // as a failure. The empty-file qualifier disambiguates a genuine
        // empty read from a no-op. Both an empty content string and a
        // missing content field land here.
        let empty = serde_json::json!({"content": ""});
        assert_eq!(
            result_summary("read", &empty),
            Some("Read 0 lines (empty file)".into())
        );
        let missing = serde_json::json!({"path": "x"});
        assert_eq!(
            result_summary("read", &missing),
            Some("Read 0 lines (empty file)".into())
        );
    }

    #[test]
    fn test_summary_grep_files_mode() {
        // Default mode: count is the file count, not match count.
        let out = serde_json::json!({"mode": "files_with_matches", "num_files": 3});
        assert_eq!(result_summary("grep", &out), Some("Found 3 files".into()));
        let one = serde_json::json!({"mode": "files_with_matches", "num_files": 1});
        assert_eq!(result_summary("grep", &one), Some("Found 1 file".into()));
        let none = serde_json::json!({"mode": "files_with_matches", "num_files": 0});
        assert_eq!(result_summary("grep", &none), Some("No files found".into()));
        // Absent mode defaults to files_with_matches.
        let default = serde_json::json!({"num_files": 2});
        assert_eq!(
            result_summary("grep", &default),
            Some("Found 2 files".into())
        );
    }

    #[test]
    fn test_summary_glob_files() {
        // Follows grep files_with_matches: the count axis is the file count.
        // Without this arm houyi fell through to a value_brief JSON glimpse.
        let out = serde_json::json!({"num_files": 3});
        assert_eq!(result_summary("glob", &out), Some("Found 3 files".into()));
        let one = serde_json::json!({"num_files": 1});
        assert_eq!(result_summary("glob", &one), Some("Found 1 file".into()));
        let none = serde_json::json!({"num_files": 0});
        assert_eq!(result_summary("glob", &none), Some("No files found".into()));
    }

    #[test]
    fn test_summary_grep_content_mode() {
        let out = serde_json::json!({"mode": "content", "num_lines": 5});
        assert_eq!(result_summary("grep", &out), Some("Found 5 lines".into()));
        let one = serde_json::json!({"mode": "content", "num_lines": 1});
        assert_eq!(result_summary("grep", &one), Some("Found 1 line".into()));
    }

    #[test]
    fn test_summary_grep_count_mode() {
        // The chip one-liner: "Found N matches across M
        // files" — not the model-content "total occurrences" string.
        let out = serde_json::json!({"mode": "count", "num_matches": 2, "num_files": 4});
        assert_eq!(
            result_summary("grep", &out),
            Some("Found 2 matches across 4 files".into())
        );
        let single = serde_json::json!({"mode": "count", "num_matches": 1, "num_files": 1});
        assert_eq!(
            result_summary("grep", &single),
            Some("Found 1 match across 1 file".into())
        );
        // 0 matches → plural "matches" (count===0 pluralizes by convention).
        let zero = serde_json::json!({"mode": "count", "num_matches": 0, "num_files": 0});
        assert_eq!(
            result_summary("grep", &zero),
            Some("Found 0 matches across 0 files".into())
        );
    }

    #[test]
    fn test_summary_write_lines() {
        let out = serde_json::json!({"path": "src/foo.rs", "bytes": 100, "lines": 3});
        assert_eq!(
            result_summary("write", &out),
            Some("Wrote 3 lines to src/foo.rs".into())
        );
        // Without the lines field, no summary (the body is the display).
        let no_lines = serde_json::json!({"path": "x", "bytes": 10});
        assert_eq!(result_summary("write", &no_lines), None);
    }

    #[test]
    fn test_summary_ask_declined() {
        let out = serde_json::json!({"declined": true, "summary": "..."});
        assert_eq!(
            result_summary("AskUserQuestion", &out),
            Some("User declined to answer questions".into())
        );
    }

    #[test]
    fn test_summary_ask_answered() {
        let out = serde_json::json!({"answered": true, "answers": {"q1": "a", "q2": "b"}});
        assert_eq!(
            result_summary("AskUserQuestion", &out),
            Some("User answered 2 questions".into())
        );
        let one = serde_json::json!({"answered": true, "answers": {"q": "a"}});
        assert_eq!(
            result_summary("AskUserQuestion", &one),
            Some("User answered 1 question".into())
        );
    }

    #[test]
    fn test_tool_user_facing_name() {
        // Edit: old_string non-empty -> Update; empty -> Create.
        let edit_mod = serde_json::json!({"path": "a.rs", "old_string": "x", "new_string": "y"});
        assert_eq!(tool_user_facing_name("edit", &edit_mod), "Update");
        let edit_new = serde_json::json!({"path": "a.rs", "old_string": "", "new_string": "y"});
        assert_eq!(tool_user_facing_name("edit", &edit_new), "Create");
        assert_eq!(tool_user_facing_name("multiedit", &edit_mod), "Update");
        // Other tools fall through to their raw name (capitalized by the caller).
        assert_eq!(tool_user_facing_name("read", &edit_mod), "read");
        assert_eq!(tool_user_facing_name("bash", &edit_mod), "bash");
    }

    #[test]
    fn test_edit_diff_summary_format() {
        // Both: comma-joined, lowercase removed (canonical shape).
        assert_eq!(edit_diff_summary(3, 2), "Added 3 lines, removed 2 lines");
        // Singular for 1.
        assert_eq!(edit_diff_summary(1, 1), "Added 1 line, removed 1 line");
        // Additions only.
        assert_eq!(edit_diff_summary(2, 0), "Added 2 lines");
        // Removals only: capital Removed.
        assert_eq!(edit_diff_summary(0, 1), "Removed 1 line");
        assert_eq!(edit_diff_summary(0, 3), "Removed 3 lines");
    }

    #[test]
    fn test_truncate_collapses_newline() {
        // A bash command with a comment + newline + command collapses to
        // one line (newlines become spaces), keeping the chip one-line.
        let cmd = "# check empty\ngit ls-remote url 2>&1 | head -5";
        let brief = truncate_call_arg(cmd);
        assert!(!brief.contains('\n'), "must be one line: {brief}");
        assert!(brief.contains("check empty"), "comment kept: {brief}");
        assert!(brief.contains("git ls-remote"), "command kept: {brief}");
    }

    #[test]
    fn test_truncate_filters_empty_lines() {
        // Double newlines do not produce double spaces or empty segments.
        let brief = truncate_call_arg("a\n\n\nb");
        assert_eq!(brief, "a b");
    }

    #[test]
    fn test_truncate_caps_160_chars() {
        let long = "x".repeat(200);
        let brief = truncate_call_arg(&long);
        assert!(brief.ends_with('…'));
        assert!(brief.chars().count() <= 161); // 160 + ellipsis
    }

    #[test]
    fn test_agent_brief_shows_type() {
        let input = serde_json::json!({"subagent_type": "explore", "prompt": "find auth"});
        assert_eq!(tool_call_brief("agent", &input), "→ explore");
    }

    #[test]
    fn test_agent_brief_defaults() {
        let input = serde_json::json!({"prompt": "do stuff"});
        assert_eq!(tool_call_brief("agent", &input), "→ general-purpose");
    }
}
