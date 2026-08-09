//! Tool-result expand body: turn a tool's output JSON into the multi-line
//! body shown below the one-line summary chip on Ctrl+O expand. Pairs with
//! brief.rs (the one-line chip); this module is the multi-line detail. Split
//! from transcript.rs so that file stays under the size gate.

use serde_json::Value;

use crate::brief::{edit_diff_summary, result_summary, value_brief};

/// Build the Write tool result body: the "Wrote N lines to {path}" chip
/// followed by the full written content. The content is pulled from the
/// call's input (the model sent it to write); the result itself stays
/// path-only for the model. Folding (first-N visible + overflow tail) is
/// the render layer's job via tool_rows(expanded), not baked into the body
/// string - so the expand control actually reveals more, instead of a dead
/// "ctrl+o to expand" hint over an already-capped preview. When call_input
/// is absent (a late-arriving result whose call frame already passed) the
/// chip alone surfaces.
pub(crate) fn write_result_body(output: &Value, call_input: Option<&Value>) -> String {
    let chip = result_summary("write", output).unwrap_or_default();
    let Some(input) = call_input else {
        return chip;
    };
    let content = input.get("content").and_then(|c| c.as_str()).unwrap_or("");
    if content.is_empty() {
        return chip;
    }
    format!("{chip}\n{content}")
}

/// Extract a tool result's multi-line body from its output JSON. Edit/MultiEdit
/// results carry a unified diff → a summary line (Added/removed N lines) followed
/// by the diff body. Bash results carry stdout (stderr appended when non-empty).
/// Search results (grep files_with_matches/count, glob) carry the matched file
/// list; grep content mode carries its matched lines in content. Write results
/// carry a byte count. Errors surface as an error: line. Anything else falls
/// back to a brief JSON glimpse so nothing is silently dropped.
pub(crate) fn extract_body(output: &str) -> String {
    let Ok(v) = serde_json::from_str::<Value>(output) else {
        // Not JSON (a plain-string result from a stub) — show it verbatim.
        return output.to_string();
    };
    if let Some(diff) = v.get("diff").and_then(|d| d.as_str()) {
        let (added, removed) = count_diff_lines(diff);
        let mut s = edit_diff_summary(added, removed);
        if !diff.is_empty() {
            s.push('\n');
            s.push_str(diff);
        }
        return s;
    }
    if let Some(err) = v.get("error") {
        if let Some(m) = err.as_str() {
            return format!("error: {m}");
        }
        return format!("error: {err}");
    }
    if let Some(stdout) = v.get("stdout").and_then(|s| s.as_str()) {
        // stdout and stderr render as separate blocks (stderr
        // in error color); our single-body renderer colors the whole body by
        // outcome, so for a failed command (success=false -> Error) the
        // joined stdout+stderr renders red. Append non-empty stderr so a
        // failed command's error message is not lost (was an empty body
        // before).
        let stderr = v.get("stderr").and_then(|s| s.as_str()).unwrap_or("");
        if !stderr.is_empty() && stdout.is_empty() {
            return stderr.to_string();
        }
        if !stderr.is_empty() {
            return format!("{stdout}\n{stderr}");
        }
        return stdout.to_string();
    }
    // Search tools (grep/glob): the expand body is the matched content or
    // file list. grep content mode carries its matched lines in content;
    // glob and grep files_with_matches/count carry the file list. An empty
    // result (0 files, no content) returns empty so the caller surfaces the
    // "No files found" / "Found 0 ..." chip alone — without this the value_brief
    // fallback below would dump a JSON glimpse as noise on expand.
    if let Some(content) = v.get("content").and_then(|s| s.as_str())
        && !content.is_empty()
    {
        return content.to_string();
    }
    if let Some(files) = v.get("filenames").and_then(|a| a.as_array()) {
        let list: Vec<&str> = files.iter().filter_map(|f| f.as_str()).collect();
        return list.join("\n");
    }
    if v.get("bytes").is_some() {
        let path = v.get("path").and_then(|p| p.as_str()).unwrap_or("?");
        let bytes = v.get("bytes").and_then(|b| b.as_u64()).unwrap_or(0);
        return format!("wrote {path} ({bytes} bytes)");
    }
    value_brief(&v)
}

/// True when the output JSON carries a unified diff (Edit/MultiEdit result),
/// so the renderer knows to color plus/minus lines. Parsed once at transcript-
/// build time, not per frame (hot path).
pub(crate) fn output_has_diff(output: &str) -> bool {
    let Ok(v) = serde_json::from_str::<Value>(output) else {
        return false;
    };
    v.get("diff").and_then(|d| d.as_str()).is_some()
}

/// Count additions (+) and removals (-) in a unified-diff body, skipping the
/// +++/--- file headers (defensive — the engine emits none) and @@ hunk
/// headers. Context lines (leading space) and hunk headers are not counted.
pub(crate) fn count_diff_lines(diff: &str) -> (u32, u32) {
    let mut added = 0u32;
    let mut removed = 0u32;
    for line in diff.lines() {
        if line.starts_with("+++") || line.starts_with("---") {
            continue;
        }
        if line.starts_with('+') {
            added += 1;
        } else if line.starts_with('-') {
            removed += 1;
        }
    }
    (added, removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A glob result (files_with_matches shape) renders the matched file list
    /// as the expand body, not a value_brief JSON glimpse. The
    /// verbose view shows this list below the Found N files summary.
    #[test]
    fn test_extract_body_glob_filenames() {
        let out = serde_json::json!({
            "filenames": ["src/a.rs", "src/b.rs"],
            "num_files": 2,
        })
        .to_string();
        assert_eq!(extract_body(&out), "src/a.rs\nsrc/b.rs");
    }

    /// A grep files_with_matches result renders the file list as the body.
    #[test]
    fn test_extract_body_grep_files() {
        let out = serde_json::json!({
            "mode": "files_with_matches",
            "filenames": ["a.rs", "b.rs", "c.rs"],
            "num_files": 3,
            "content": "",
        })
        .to_string();
        assert_eq!(extract_body(&out), "a.rs\nb.rs\nc.rs");
    }

    /// grep content mode keeps its matched lines in the content field.
    #[test]
    fn test_extract_body_grep_content() {
        let out = serde_json::json!({
            "mode": "content",
            "filenames": [],
            "content": "a.rs:1:match\na.rs:3:match",
            "num_lines": 2,
        })
        .to_string();
        assert_eq!(extract_body(&out), "a.rs:1:match\na.rs:3:match");
    }

    /// Empty filenames falls through (no spurious empty body short-circuit);
    /// the caller surfaces the summary chip alone.
    #[test]
    fn test_extract_body_empty() {
        let out = serde_json::json!({"filenames": [], "num_files": 0}).to_string();
        assert!(extract_body(&out).is_empty());
    }

    /// The Write body is the chip + the FULL content; folding is the render
    /// layer's job (tool_rows expanded), so no preview cap or overflow hint
    /// is baked in. Content is pulled from the call input so the result stays
    /// path-only for the model.
    #[test]
    fn test_write_carries_full_content() {
        let out = serde_json::json!({"path": "src/foo.rs", "bytes": 100, "lines": 12});
        let input = serde_json::json!({
            "path": "src/foo.rs",
            "content": "line0\nline1\nline2\nline3\nline4\nline5\nline6\nline7\nline8\nline9\nline10\nline11"
        });
        let body = write_result_body(&out, Some(&input));
        assert!(
            body.starts_with("Wrote 12 lines to src/foo.rs\nline0"),
            "chip then full content: {body}"
        );
        assert!(body.contains("line11"), "last line carried: {body}");
        assert!(
            !body.contains("ctrl+o"),
            "no dead expand hint baked into body: {body}"
        );
    }

    /// Without the call input (a late result) the chip alone surfaces.
    #[test]
    fn test_write_chip_only_late() {
        let out = serde_json::json!({"path": "src/foo.rs", "bytes": 100, "lines": 12});
        let body = write_result_body(&out, None);
        assert_eq!(body, "Wrote 12 lines to src/foo.rs");
    }

    /// A short write carries all content; no overflow hint.
    #[test]
    fn test_write_body_short_content() {
        let out = serde_json::json!({"path": "f", "bytes": 4, "lines": 2});
        let input = serde_json::json!({"path": "f", "content": "a\nb"});
        let body = write_result_body(&out, Some(&input));
        assert_eq!(body, "Wrote 2 lines to f\na\nb");
    }
}
