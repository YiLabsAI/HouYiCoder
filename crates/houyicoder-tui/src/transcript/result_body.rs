//! Tool-result expand body: turn a tool's output JSON into the multi-line
//! body shown below the one-line summary chip on Ctrl+O expand. Pairs with
//! brief.rs, which formats the one-line call chip.

use serde_json::Value;

use crate::brief::{edit_diff_summary, result_summary, value_brief};
use houyicoder_protocol::tool::{simple_command_word, strip_ansi};

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
pub fn extract_body(output: &str) -> String {
    strip_ansi(&raw_body(output))
}

/// The body before terminal control sequences are removed. Kept separate so
/// the sanitiser is one wrapper over every return path rather than a call at
/// each of them.
fn raw_body(output: &str) -> String {
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
        // A failed bash command surfaces its exit code, a signal note when
        // the process was killed by a signal, stderr, then stdout — in that
        // order (exit code first as the header, stdout last as the least
        // important). All four sections are rendered verbatim, no truncation
        // or processing — the user sees the full error context. A successful
        // command renders stdout alone (no exit code 0 line — 0 is not worth
        // a row).
        let stderr = v.get("stderr").and_then(|s| s.as_str()).unwrap_or("");
        let exit_code = v.get("exit_code").and_then(|c| c.as_i64());
        let failed = v.get("success").and_then(|s| s.as_bool()) == Some(false);
        let signal = v.get("signal").and_then(|s| s.as_i64());
        if failed {
            let mut sections: Vec<String> = Vec::new();
            if let Some(code) = exit_code {
                sections.push(format!("Exit code {code}"));
            }
            if let Some(sig) = signal {
                sections.push(format!("Killed by signal {sig}"));
            }
            if !stderr.is_empty() {
                sections.push(stderr.to_string());
            }
            if !stdout.is_empty() {
                sections.push(stdout.to_string());
            }
            if sections.is_empty() {
                return String::new();
            }
            return sections.join("\n");
        }
        // Success: stdout (and stderr if any — a warning on a successful
        // command is still useful context, but no exit code line).
        let mut parts: Vec<&str> = Vec::new();
        if !stdout.is_empty() {
            parts.push(stdout);
        }
        if !stderr.is_empty() {
            parts.push(stderr);
        }
        if parts.is_empty() {
            return String::new();
        }
        return parts.join("\n");
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
    // A field-level marker: isolate_large_output externalized one top-level
    // string field (the largest — content for an agent result, stdout for a
    // large bash result) so the envelope's other keys stay inline. That
    // field is now a {block_ref, preview, hint} object. Surface the field's
    // preview + hint, not the raw marker JSON. The top-level arm below
    // handles whole-output markers (blob tools, old-session logs).
    if let Some(body) = field_level_marker_body(&v) {
        return body;
    }
    // A whole-output CAS marker: the append stage replaced a too-large tool
    // output with a block_ref pointer plus a truncated inline preview.
    // Surface the preview (the actual content) and the retrieval hint,
    // not the raw marker JSON. Resumed old sessions benefit too: their
    // logged results carry top-level block_ref markers.
    if v.get("block_ref").is_some() {
        let preview = v.get("preview").and_then(|p| p.as_str()).unwrap_or("");
        let hint = v
            .get("hint")
            .and_then(|h| h.as_str())
            .unwrap_or("output compacted; re-invoke the tool to retrieve it");
        if preview.is_empty() {
            return hint.to_string();
        }
        return format!("{preview}\n[{hint}]");
    }
    value_brief(&v)
}

/// The preview + hint body for a field-level marker nested under one of the
/// output's top-level fields. Walks the object for the single field whose
/// value is a {block_ref, preview, hint} object (isolate externalizes only
/// the largest string field, so there is at most one). None when no field
/// carries a nested marker.
fn field_level_marker_body(v: &Value) -> Option<String> {
    let obj = v.as_object()?;
    for (_, field) in obj.iter() {
        if field.get("block_ref").is_none() {
            continue;
        }
        let preview = field.get("preview").and_then(|p| p.as_str()).unwrap_or("");
        let hint = field
            .get("hint")
            .and_then(|h| h.as_str())
            .unwrap_or("output compacted; re-invoke the tool to retrieve it");
        return if preview.is_empty() {
            Some(hint.to_string())
        } else {
            Some(format!("{preview}\n[{hint}]"))
        };
    }
    None
}

/// True when the output JSON carries a unified diff (Edit/MultiEdit result),
/// so the renderer knows to color plus/minus lines. Parsed once at transcript-
/// build time, not per frame (hot path).
pub(crate) fn output_has_diff(output: &str) -> bool {
    let Ok(v) = serde_json::from_str::<Value>(output) else {
        return false;
    };
    if v.get("diff").and_then(|d| d.as_str()).is_some() {
        return true;
    }
    // A shell command can print a diff too. The header lines decide it, so a
    // log that merely mentions a path with a dash is not coloured.
    v.get("stdout")
        .and_then(|s| s.as_str())
        .is_some_and(looks_like_unified_diff)
}

/// Whether text carries the three header shapes a unified diff opens with.
fn looks_like_unified_diff(text: &str) -> bool {
    let mut saw_old = false;
    let mut saw_new = false;
    let mut saw_hunk = false;
    for line in text.lines() {
        if line.starts_with("diff --git ") {
            return true;
        }
        saw_old |= line.starts_with("--- ");
        saw_new |= line.starts_with("+++ ");
        saw_hunk |= line.starts_with("@@ ");
    }
    saw_old && saw_new && saw_hunk
}

/// The set of bash commands that produce no output on success by design.
/// Their silence IS the success signal — an empty body for these commands
/// should read as "done", not as the ambiguous "(no output)" placeholder
/// that a failed-or-empty command shows. A command is recognized when its
/// first word (after stripping leading env assignments) is in this set;
/// pipelines and compound commands are excluded (their silence may belong
/// to a stage that is NOT in the set).
const SILENT_SUCCESS_COMMANDS: &[&str] = &[
    "mv", "cp", "rm", "mkdir", "rmdir", "chmod", "chown", "touch", "cd", "ln", "install",
];

/// True when a bash tool call is a silent-success command that completed
/// with no output. The caller uses this to render "done" instead of the
/// "(no output)" placeholder — the user sees an explicit success signal
/// for a command whose output is silence by design.
pub(crate) fn command_is_silent_success(call_input: Option<&Value>, output: &Value) -> bool {
    // Only a successful command qualifies: a failed silent command (e.g.
    // rm on a missing file) has an error that must surface, and labelling
    // it done would state the opposite of what happened. A non-zero exit
    // is checked directly rather than trusting the success field alone:
    // success is a derived convenience, exit_code is the primitive the
    // shell actually reported, and a result carrying only the latter must
    // not read as done.
    if output.get("error").is_some()
        || output.get("success").and_then(|v| v.as_bool()) == Some(false)
        || output
            .get("exit_code")
            .and_then(|c| c.as_i64())
            .is_some_and(|c| c != 0)
    {
        return false;
    }
    let Some(input) = call_input else {
        return false;
    };
    let Some(command) = input.get("command").and_then(|c| c.as_str()) else {
        return false;
    };
    let Some(word) = simple_command_word(command) else {
        return false;
    };
    SILENT_SUCCESS_COMMANDS.contains(&word)
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
mod tests;
