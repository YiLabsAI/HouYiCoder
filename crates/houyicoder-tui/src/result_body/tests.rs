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

/// A field-level marker: the largest top-level string field (content for
/// an agent result, stdout for a large bash result) was externalized, so
/// that field is now a {block_ref, preview, hint} object while the
/// envelope's other keys stay inline. The body surfaces the field's
/// preview + hint, never the raw marker JSON or the envelope keys.
#[test]
fn test_extract_body_nested_marker() {
    let out = serde_json::json!({
        "status": "completed",
        "content": {
            "block_ref": "a3a1dde5b075cee6",
            "preview": "the child explored src/auth and found the module",
            "data_tag": false,
            "hint": "large output compacted; re-invoke the tool to retrieve it",
        },
        "agentId": "child-xyz",
        "color": "red",
    })
    .to_string();
    let body = extract_body(&out);
    assert!(
        body.contains("the child explored src/auth"),
        "field preview surfaces: {body}"
    );
    assert!(
        body.contains("re-invoke the tool"),
        "retrieval hint surfaces: {body}"
    );
    assert!(!body.contains("block_ref"), "no raw marker key: {body}");
    assert!(
        !body.contains("child-xyz"),
        "envelope keys do not leak into the body: {body}"
    );
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
