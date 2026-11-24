//! Fault-recall tests: deterministic feature extraction and the local
//! lexical selection over lessons whose bodies carry real error strings.

use std::collections::HashSet;
use std::env::temp_dir;
use std::fs;
use std::path::PathBuf;
use std::process::id;
use std::sync::atomic::{AtomicU64, Ordering};

use houyicoder_api::memory::MemoryProvider;
use houyicoder_context::{MemoryEntry, MemorySource};
use houyicoder_memory::MarkdownMemoryProvider;
use serde_json::json;

use super::*;

fn temp_root() -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let root = temp_dir().join(format!("failure_recall_{seq}_{}", id()));
    fs::create_dir_all(&root).expect("temp root");
    root
}

fn signal(tool: &str, command: &str, output: &str) -> FailureSignal {
    FailureSignal {
        tool: tool.to_string(),
        input: json!({ "command": command }),
        output: json!({ "error": output }),
    }
}

/// A failure yields its tool, its program, the error code, and the long
/// identifiers, in that order, without duplicates.
#[test]
fn test_error_features() {
    let features = error_features(&signal(
        "bash",
        "cargo check /Users/von/project/src/io.rs:42:5",
        "error[E0308]: mismatched types in sandbox_apply (see NOK.out)",
    ));
    assert!(features.contains(&"bash".to_string()));
    assert!(features.contains(&"cargo".to_string()));
    assert!(features.contains(&"E0308".to_string()));
    assert!(features.contains(&"sandbox_apply".to_string()));
    assert!(features.contains(&"NOK.out".to_string()));
    assert!(features.contains(&"io.rs".to_string()));
    assert!(
        !features.contains(&"Users".to_string()),
        "directory segments must not take feature slots: {features:?}"
    );
    assert_eq!(features.iter().filter(|f| *f == "cargo").count(), 1);
}

/// A failure that names only its tool and program has no distinguishing
/// feature, so the threshold rejects it.
#[test]
fn test_tool_only_is_weak() {
    let features = error_features(&signal("bash", "git status", "failed"));
    assert!(features.iter().all(|f| is_tool_name(f)));
    let root = temp_root();
    let provider = MarkdownMemoryProvider::new(root.clone());
    assert!(
        select_failure_entries(&provider, &features, &HashSet::new()).is_empty(),
        "a tool-only signature injects nothing"
    );
    let _ = fs::remove_dir_all(&root).ok();
}

/// Twenty real error strings and the lesson that records each one.
const CASES: &[(&str, &str, &str)] = &[
    (
        "lesson-tool-call-shape-refusals",
        "write: content (string) required",
        "A refused shape is a constraint to change, not to repeat.",
    ),
    (
        "lesson-cross-turn-failed-call-replay",
        "old_string not found",
        "Recall what the earlier call produced before repeating it.",
    ),
    (
        "lesson-missing-required-argument-on-write",
        "missing field old_string",
        "The schema names the field the call omitted.",
    ),
    (
        "lesson-error-debug-shape-identifies-producer",
        "hyper_util TimedOut ConnectError",
        "The debug shape of an error names its producer.",
    ),
    (
        "lesson-in-place-rewrite-destroys-work",
        "cargo check E0308 mismatched types",
        "Rewrite through the editor, never through a stream.",
    ),
    (
        "lesson-pipeline-exit-code-masks-gate-result",
        "gate EXIT=1 while Compiling",
        "Read the gate's own exit status, not the pipeline's.",
    ),
    (
        "lesson-probe-banner-names-the-failing-case",
        "banner cast_to_internal_error",
        "The banner above a traceback names the failing case.",
    ),
    (
        "lesson-read-offset-on-large-file-returns-empty",
        "offset=10095 limit=18 empty",
        "A large offset returns an empty body, not a missing file.",
    ),
    (
        "lesson-rm-denied-outside-workspace",
        "workspace Operation not permitted",
        "A denial outside the workspace is the fence working.",
    ),
    (
        "lesson-second-hand-signal-is-not-measurement",
        "NOK.out closed=0",
        "A second-hand signal is not a measurement.",
    ),
    (
        "lesson-test-at-accumulation-scale",
        "handle_approval panic",
        "A test must reach the accumulation scale it guards.",
    ),
    (
        "lesson-weak-assertion-marks-surviving-defect",
        "rect.x + 11 assertion",
        "A weak assertion marks the defect that survived.",
    ),
    (
        "lesson-coverage-readout-needs-clean-environment",
        "NEXTEST_VERIFY_FILTER missing",
        "Measure the set the gate actually runs.",
    ),
    (
        "lesson-discriminant-test-must-build-real-error",
        "IoError::from ErrorKind",
        "A discriminant test needs the real error path.",
    ),
    (
        "lesson-harness-path-alias-worktree",
        "fix-egress-tier-cwd worktree cd",
        "Build the tree and enter it in one command.",
    ),
    (
        "lesson-prefix-match-is-not-command-identity",
        "skip_word prefix",
        "A prefix match is not the command word.",
    ),
    (
        "lesson-tool-output-two-truncation-surfaces",
        "truncated output surface",
        "Two truncation surfaces hide the same body.",
    ),
    (
        "lesson-worktree-enter-refused-by-live-command",
        "enter_worktree refused",
        "Enter the worktree only after the running command clears.",
    ),
    (
        "lesson-bisect-blocks-commits-silently",
        "silent BISECT_EXPECTED_REV",
        "A bisect in progress blocks a commit silently.",
    ),
    (
        "lesson-ci-red-on-your-commit-may-predate-it",
        "github check-runs",
        "A red check may predate the commit it names.",
    ),
];

/// Twenty lessons whose bodies carry real error strings are each found from
/// that string; the query never repeats the description, so only body
/// scoring can answer it.
#[test]
fn test_twenty_real_error_positives() {
    let root = temp_root();
    let provider = MarkdownMemoryProvider::new(root.clone());
    for &(key, body, description) in CASES {
        provider
            .add(MemoryEntry::new(key, body, MemorySource::Feedback).with_meta(description, 0))
            .expect("seed lesson");
    }
    for &(key, body, _) in CASES {
        let features = error_features(&signal("bash", body, body));
        let selected = select_failure_entries(&provider, &features, &HashSet::new());
        assert!(
            selected.iter().any(|entry| entry.key == key),
            "{key} not selected for its own error string {body:?}; got {:?}",
            selected.iter().map(|e| e.key.as_str()).collect::<Vec<_>>()
        );
    }
    fs::remove_dir_all(&root).ok();
}

/// Ten errors that no stored lesson carries select nothing, so a novel
/// failure does not inject an unrelated lesson.
#[test]
fn test_ten_negatives_select_nothing() {
    let root = temp_root();
    let provider = MarkdownMemoryProvider::new(root.clone());
    for (key, body) in [
        ("lesson-one", "E0308 mismatched types in the parser"),
        (
            "lesson-two",
            "Operation not permitted outside the workspace",
        ),
        ("lesson-three", "old_string not found during the edit"),
    ] {
        provider
            .add(MemoryEntry::new(key, body, MemorySource::Feedback))
            .expect("seed lesson");
    }
    for query in [
        "ENOSPC no space left on device",
        "segmentation fault core dumped",
        "connection reset by peer while reading",
        "SSL certificate verify failed",
        "out of memory killed process",
        "disk quota exceeded",
        "invalid utf-8 sequence in stream",
        "deadlock detected waiting for lock",
        "address already in use on port",
        "unexpected end of json input",
        "docker: image alpine:latest not found",
        "sysctl: Operation not permitted on kernel parameter",
    ] {
        let features = error_features(&signal("bash", query, query));
        let selected = select_failure_entries(&provider, &features, &HashSet::new());
        assert!(
            selected.is_empty(),
            "{query:?} selected {:?}",
            selected.iter().map(|e| e.key.as_str()).collect::<Vec<_>>()
        );
    }
    fs::remove_dir_all(&root).ok();
}

/// The signature ignores order and case, so the same failure reported twice
/// in one turn matches one signature.
#[test]
fn test_signature_is_order_free() {
    let first = vec!["E0308".to_string(), "Cargo".to_string()];
    let second = vec!["cargo".to_string(), "e0308".to_string()];
    assert_eq!(signature(&first), signature(&second));
}
