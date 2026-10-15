use super::*;

#[test]
fn test_memory_result_labels() {
    // The outcome field drives the save label: a fresh store, a rewrite,
    // and a no-op refresh. The unchanged case must not claim a write.
    assert_eq!(
        result_summary(
            "save_memory",
            &serde_json::json!({"saved": "k", "outcome": "created"})
        )
        .as_deref(),
        Some("created k")
    );
    assert_eq!(
        result_summary(
            "save_memory",
            &serde_json::json!({"saved": "k", "outcome": "updated"})
        )
        .as_deref(),
        Some("updated k")
    );
    assert_eq!(
        result_summary(
            "save_memory",
            &serde_json::json!({"saved": "k", "outcome": "unchanged"})
        )
        .as_deref(),
        Some("unchanged k")
    );
    // Legacy tool records predate the outcome field: a bare saved key
    // defaults to stored, the old unchanged boolean still maps to
    // unchanged.
    assert_eq!(
        result_summary("save_memory", &serde_json::json!({"saved": "k"})).as_deref(),
        Some("stored k")
    );
    assert_eq!(
        result_summary(
            "save_memory",
            &serde_json::json!({"saved": "k", "unchanged": true})
        )
        .as_deref(),
        Some("unchanged k")
    );
    assert_eq!(
        result_summary("delete_memory", &serde_json::json!({"deleted": "k"})).as_deref(),
        Some("deleted k")
    );
}

#[test]
fn test_search_memory_labels() {
    // The call chip shows the query, not a JSON glimpse of it: a search
    // is identified by what it looked for.
    let input = serde_json::json!({"query": "how the deploy gate works"});
    assert_eq!(
        tool_call_brief("search_memory", &input),
        "how the deploy gate works"
    );
    // The result collapses to a match count. The query is already on the
    // call line above, so repeating it would duplicate the row.
    assert_eq!(
        result_summary(
            "search_memory",
            &serde_json::json!({"matches": [{"key": "a"}, {"key": "b"}]})
        )
        .as_deref(),
        Some("found 2")
    );
    // An empty result is a real answer, not a failure: it must render as
    // zero matches rather than falling through to the raw JSON body.
    assert_eq!(
        result_summary("search_memory", &serde_json::json!({"matches": []})).as_deref(),
        Some("found 0")
    );
    // A result with no matches field has no count to report, so the
    // caller keeps the raw body instead of a fabricated zero.
    assert_eq!(
        result_summary("search_memory", &serde_json::json!({})),
        None
    );
}

#[test]
fn test_memory_scope_flow_labels() {
    // The scope-flow and read tools collapse to a human label naming the
    // topic; the raw JSON (promoted/demoted/key) stays out of the
    // transcript body, mirroring save/delete. An empty key must not emit
    // a misleading label — it falls back to None so the caller keeps the
    // raw body.
    assert_eq!(
        result_summary("promote_memory", &serde_json::json!({"promoted": "rule-x"})).as_deref(),
        Some("promoted rule-x")
    );
    assert_eq!(
        result_summary("demote_memory", &serde_json::json!({"demoted": "rule-y"})).as_deref(),
        Some("demoted rule-y")
    );
    assert_eq!(
        result_summary(
            "show_memory",
            &serde_json::json!({"key": "rule-z", "content": "body..."})
        )
        .as_deref(),
        Some("showed rule-z")
    );
    assert_eq!(
        result_summary("promote_memory", &serde_json::json!({"promoted": ""})).as_deref(),
        None,
        "an empty key must not produce a label"
    );
}

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
    // Without this branch houyi fell through to a value_brief JSON glimpse.
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

/// A long subagent_type is capped so the chip stays one line, matching
/// the budget every other branch enforces. Pins the cap against a regression
/// that drops it and overflows the row.
#[test]
fn test_agent_brief_caps_long() {
    let long = "a".repeat(200);
    let input = serde_json::json!({"subagent_type": long});
    let brief = tool_call_brief("agent", &input);
    assert!(
        brief.chars().count() <= 44,
        "long type capped to one chip line, got {} chars",
        brief.chars().count()
    );
    assert!(
        brief.ends_with('\u{2026}'),
        "long type truncated with an ellipsis, got {brief}"
    );
    assert!(brief.starts_with("→ "), "prefix preserved, got {brief}");
}
