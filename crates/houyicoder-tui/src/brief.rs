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
/// text), then truncate to the chip budget: at most 160 chars on one line so
/// a long shell one-liner stays one-glance. Unknown tools keep the original
/// 60-char value_brief glimpse — MCP tools fall here with large inputs, and a
/// 160-char 2-line dump is not a chip. The path field name matches the tool
/// schemas (which use path, not file_path) so a Write/Edit call chip shows
/// the path, not the entire input JSON (which embeds the file content).
pub(crate) fn tool_call_brief(tool: &str, input: &Value) -> String {
    match tool {
        "bash" | "read" | "write" | "edit" | "multiedit" | "grep" | "glob" | "save_memory"
        | "delete_memory" | "promote_memory" | "demote_memory" | "show_memory"
        | "search_memory" => {
            truncate_call_arg(&houyicoder_protocol::tool::tool_invocation(tool, input))
        }
        "agent" => {
            let st = input
                .get("subagent_type")
                .and_then(|v| v.as_str())
                .unwrap_or("general-purpose");
            // Cap the type so a malformed or very long name keeps the chip
            // one line, matching the budget every other arm enforces.
            const MAX_TYPE: usize = 40;
            let capped = if st.chars().count() > MAX_TYPE {
                let cut: String = st.chars().take(MAX_TYPE - 1).collect();
                format!("{cut}\u{2026}")
            } else {
                st.to_string()
            };
            format!("→ {capped}")
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
/// Flatten a child-summary preview into a single-line fold-group label:
/// newlines become spaces, and the text truncates to fit a terminal row
/// (~80 chars) with an ellipsis. The full multiline content shows on
/// Ctrl+O expand; this is just the collapsed one-liner.
pub(crate) fn fold_summary(s: &str) -> String {
    let flat: String = s.replace(['\n', '\r'], " ");
    let chars: Vec<char> = flat.chars().collect();
    if chars.len() <= 80 {
        flat.trim().to_string()
    } else {
        let t: String = chars.iter().take(79).collect();
        format!("{}…", t.trim_end())
    }
}

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

/// The memory tools whose transcript result is one human label rather than
/// the raw JSON body. Shared by the result-summary projection and the
/// transcript body builder so a new memory tool joins both at once.
pub(crate) const MEMORY_LABEL_TOOLS: &[&str] = &[
    "save_memory",
    "delete_memory",
    "promote_memory",
    "demote_memory",
    "show_memory",
    "search_memory",
];

/// Human label for a memory-tool result: names the topic the call touched so
/// the transcript shows one readable line instead of the raw JSON. An empty
/// or missing key returns None so the caller falls back to the raw body
/// rather than emitting a misleading label. show_memory carries the full
/// entry body in its JSON; the label names the key only, mirroring the Read
/// tool (content reaches the model via the tool result, not the transcript).
/// search_memory returns a list rather than one entry, so its label counts
/// the matches; the query is already on the call line.
fn memory_result_label(tool: &str, output: &Value) -> Option<String> {
    if tool == "search_memory" {
        let count = output
            .get("matches")
            .and_then(|v| v.as_array())
            .map(Vec::len)?;
        return Some(format!("found {count}"));
    }
    let (key_field, label) = match tool {
        "save_memory" => ("saved", save_memory_label(output)),
        "delete_memory" => ("deleted", "deleted"),
        "promote_memory" => ("promoted", "promoted"),
        "demote_memory" => ("demoted", "demoted"),
        "show_memory" => ("key", "showed"),
        _ => return None,
    };
    let key = output
        .get(key_field)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())?;
    Some(format!("{label} {key}"))
}

/// The save_memory outcome drives the verb: a fresh key is created, a
/// rewrite is updated, and a no-op refresh is unchanged. Legacy tool
/// records predate the outcome field; the old unchanged boolean still
/// marks a no-op, and a bare saved key defaults to stored.
fn save_memory_label(output: &Value) -> &'static str {
    match output.get("outcome").and_then(|v| v.as_str()) {
        Some("created") => "created",
        Some("updated") => "updated",
        Some("unchanged") => "unchanged",
        _ => {
            if output
                .get("unchanged")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                "unchanged"
            } else {
                "stored"
            }
        }
    }
}

/// summaries (not raw content): "Read N lines" / "Wrote N lines to
/// {path}" / grep mode-aware / AskUserQuestion TUI label. None when the body
/// IS the display (Edit/MultiEdit diff) -- those keep their raw body; the
/// content still lands below for Ctrl+O expand. Memory tools report a
/// machine-readable key (and for show_memory, the full entry body); the
/// transcript collapses to a human label via MEMORY_LABEL_TOOLS.
pub(crate) fn result_summary(tool: &str, output: &Value) -> Option<String> {
    if MEMORY_LABEL_TOOLS.contains(&tool) {
        return memory_result_label(tool, output);
    }
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
#[path = "brief_tests.rs"]
mod tests;
