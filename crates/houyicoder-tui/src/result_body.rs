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
pub fn extract_body(output: &str) -> String {
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
    // A CAS-isolated result: the append stage replaced a too-large tool
    // output with a block_ref pointer plus a truncated inline preview.
    // Surface the preview (the actual content) and the retrieval hint,
    // not the raw marker JSON. Without this arm the value_brief fallback
    // below dumps {"block_ref":"..."} as noise, which is what the user
    // sees when a grep or a child summary crosses the isolation
    // threshold. Resumed old sessions benefit too: their logged results
    // carry block_ref markers that now render as previews.
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

/// True when the output JSON carries a unified diff (Edit/MultiEdit result),
/// so the renderer knows to color plus/minus lines. Parsed once at transcript-
/// build time, not per frame (hot path).
pub(crate) fn output_has_diff(output: &str) -> bool {
    let Ok(v) = serde_json::from_str::<Value>(output) else {
        return false;
    };
    v.get("diff").and_then(|d| d.as_str()).is_some()
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
    let Some(word) = crate::bash_command::simple_command_word(command) else {
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

    /// A CAS-isolated tool result carries a block_ref pointer + a truncated
    /// preview, not the raw content. The body must surface the preview + the
    /// retrieval hint, never the raw marker JSON. Regression for the
    /// {"block_ref":"..."} noise that leaked into grep + child-summary result
    /// rows when their serialized output crossed the 8 KB isolation threshold.
    #[test]
    fn test_extract_body_block_ref() {
        let out = serde_json::json!({
            "block_ref": "a3a1dde5b075cee6",
            "preview": "src/auth/login.rs:42: fn login()",
            "data_tag": false,
            "hint": "large output compacted; re-invoke the tool to retrieve it",
        })
        .to_string();
        let body = extract_body(&out);
        assert!(
            body.contains("src/auth/login.rs:42: fn login()"),
            "preview content must surface, got: {body}"
        );
        assert!(
            body.contains("re-invoke the tool"),
            "retrieval hint must surface, got: {body}"
        );
        assert!(
            !body.contains("block_ref"),
            "raw marker key must not leak, got: {body}"
        );
        assert!(
            !body.contains("a3a1dde5"),
            "raw hash must not leak, got: {body}"
        );
    }

    /// A block_ref with an empty preview (the reducer stripped it to nothing)
    /// falls back to the hint alone, not the raw JSON.
    #[test]
    fn test_extract_body_empty_block() {
        let out = serde_json::json!({
            "block_ref": "deadbeef",
            "preview": "",
            "data_tag": true,
            "hint": "large output compacted; re-invoke the tool to retrieve it",
        })
        .to_string();
        let body = extract_body(&out);
        assert!(
            body.contains("re-invoke the tool"),
            "hint surfaces when preview is empty, got: {body}"
        );
        assert!(!body.contains("block_ref"), "no raw key: {body}");
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

    /// A failed bash command surfaces its exit code before stderr so the
    /// user sees the numeric verdict, not just the error text.
    #[test]
    fn test_bash_error_exit_stderr() {
        let out = serde_json::json!({
            "stdout": "",
            "stderr": "error: could not write index",
            "exit_code": 1,
            "success": false,
        })
        .to_string();
        let body = extract_body(&out);
        assert!(
            body.contains("Exit code 1"),
            "exit code must surface: {body}"
        );
        assert!(
            body.contains("error: could not write index"),
            "stderr must surface: {body}"
        );
        // Exit code comes before stderr (header, not footer).
        let ec_pos = body.find("Exit code 1").unwrap();
        let err_pos = body.find("error: could not write index").unwrap();
        assert!(ec_pos < err_pos, "exit code before stderr: {body}");
    }

    /// A failed bash command with stdout + stderr + exit code: order is
    /// exit code, stderr, stdout (exit code first as the header, stdout
    /// last as the least important section).
    #[test]
    fn test_bash_error_stdout_exit() {
        let out = serde_json::json!({
            "stdout": "partial output",
            "stderr": "fatal: bad ref",
            "exit_code": 128,
            "success": false,
        })
        .to_string();
        let body = extract_body(&out);
        let ec_pos = body.find("Exit code 128").unwrap();
        let err_pos = body.find("fatal: bad ref").unwrap();
        let stdout_pos = body.find("partial output").unwrap();
        assert!(ec_pos < err_pos, "exit code before stderr: {body}");
        assert!(err_pos < stdout_pos, "stderr before stdout: {body}");
    }

    /// A bash command killed by a signal surfaces the signal note between
    /// the exit code and stderr.
    #[test]
    fn test_bash_signal_surfaces() {
        let out = serde_json::json!({
            "stdout": "",
            "stderr": "",
            "exit_code": 137,
            "signal": 9,
            "success": false,
        })
        .to_string();
        let body = extract_body(&out);
        assert!(body.contains("Exit code 137"), "exit code surfaces: {body}");
        assert!(
            body.contains("Killed by signal 9"),
            "signal note surfaces: {body}"
        );
    }

    /// A successful bash command does NOT surface an exit code line —
    /// exit code 0 is not worth a row.
    #[test]
    fn test_bash_success_no_exit() {
        let out = serde_json::json!({
            "stdout": "hello",
            "stderr": "",
            "exit_code": 0,
            "success": true,
        })
        .to_string();
        let body = extract_body(&out);
        assert_eq!(body, "hello");
        assert!(!body.contains("Exit code"), "no exit code for success");
    }

    /// A failed bash command with only exit code (no stdout, no stderr)
    /// still surfaces the exit code.
    #[test]
    fn test_bash_error_exit_only() {
        let out = serde_json::json!({
            "stdout": "",
            "stderr": "",
            "exit_code": 2,
            "success": false,
        })
        .to_string();
        let body = extract_body(&out);
        assert_eq!(body, "Exit code 2");
    }

    // --- silent-success detection ---

    /// A silent command (mv) with empty stdout and no error is a silent
    /// success — the caller renders "done" instead of "(no output)".
    #[test]
    fn test_silent_success_mv() {
        let input = serde_json::json!({"command": "mv a b"});
        let output = serde_json::json!({"stdout": "", "exit_code": 0});
        assert!(command_is_silent_success(Some(&input), &output));
    }

    /// A non-silent command (echo) with empty stdout is NOT silent success
    /// — echo producing no output is unexpected, not a done signal.
    #[test]
    fn test_silent_success_echo_rejected() {
        let input = serde_json::json!({"command": "echo"});
        let output = serde_json::json!({"stdout": "", "exit_code": 0});
        assert!(!command_is_silent_success(Some(&input), &output));
    }

    /// A silent command that failed (error field present) is NOT silent
    /// success — the error message must surface, not a "done" label.
    #[test]
    fn test_silent_success_failed_rejected() {
        let input = serde_json::json!({"command": "mv missing target"});
        let output = serde_json::json!({"error": "No such file", "success": false});
        assert!(!command_is_silent_success(Some(&input), &output));
    }

    /// A non-zero exit alone rejects the done label, even when the result
    /// carries no success field and no error field. The success field is a
    /// derived convenience; exit_code is what the shell reported. Trusting
    /// only success made a failed mv (exit 1, success absent) render "done"
    /// — the exact opposite of what happened.
    #[test]
    fn test_silent_success_exit_nonzero() {
        let input = serde_json::json!({"command": "mv a b"});
        let output = serde_json::json!({"stdout": "", "exit_code": 1});
        assert!(!command_is_silent_success(Some(&input), &output));
    }

    /// A silent command with success == false is NOT silent success even
    /// without an error field — a failed rm has an error message in stderr.
    #[test]
    fn test_silent_success_false_rejected() {
        let input = serde_json::json!({"command": "rm missing"});
        let output = serde_json::json!({"stdout": "", "success": false, "exit_code": 1});
        assert!(!command_is_silent_success(Some(&input), &output));
    }

    /// A pipeline (mv a b | cat) is NOT silent success — the exit code
    /// belongs to the last stage, not mv, so silence may mean cat failed.
    #[test]
    fn test_silent_success_pipeline_excluded() {
        let input = serde_json::json!({"command": "mv a b | cat"});
        let output = serde_json::json!({"stdout": "", "exit_code": 0});
        assert!(!command_is_silent_success(Some(&input), &output));
    }

    /// A compound command (mv a b && echo done) is NOT silent success —
    /// the exit code belongs to the last stage.
    #[test]
    fn test_silent_success_compound_excluded() {
        let input = serde_json::json!({"command": "mv a b && echo done"});
        let output = serde_json::json!({"stdout": "", "exit_code": 0});
        assert!(!command_is_silent_success(Some(&input), &output));
    }

    /// A leading env-assignment prefix (FOO=bar mv a b) is stripped so
    /// the actual command word is recognized.
    #[test]
    fn test_silent_success_env_prefix() {
        let input = serde_json::json!({"command": "FOO=bar mv a b"});
        let output = serde_json::json!({"stdout": "", "exit_code": 0});
        assert!(command_is_silent_success(Some(&input), &output));
    }

    /// Without call_input (a late-arriving result whose call frame passed)
    /// the command text is unknown, so it is NOT treated as silent success.
    #[test]
    fn test_silent_success_no_input() {
        let output = serde_json::json!({"stdout": "", "exit_code": 0});
        assert!(!command_is_silent_success(None, &output));
    }

    /// Every command in the whitelist is recognized, not just mv.
    #[test]
    fn test_silent_success_all_whitelisted() {
        let output = serde_json::json!({"stdout": "", "exit_code": 0});
        for cmd in [
            "cp a b",
            "rm x",
            "mkdir d",
            "rmdir d",
            "chmod 755 f",
            "touch f",
            "ln -s a b",
        ] {
            let input = serde_json::json!({"command": cmd});
            assert!(
                command_is_silent_success(Some(&input), &output),
                "{cmd} should be silent success"
            );
        }
    }
}
