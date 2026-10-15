//! Tests for this module.

use super::*;

/// The extraction prompt must name the save-memory tool and the
/// what-not-to-save gate — the two load-bearing guardrails. Byte-stable
/// so the prefix caches across forked runs.
#[test]
fn test_extraction_prompt_names_guardrails() {
    let p = extraction_prompt();
    assert!(
        p.contains("save_memory"),
        "prompt must name the structured write tool"
    );
    assert!(
        p.contains("what-not-to-save"),
        "prompt must reference the gate defined in the system prefix"
    );
    assert!(
        p.to_lowercase().contains("do not"),
        "prompt carries explicit do-not-save guidance"
    );
    assert!(
        p.contains("manifest"),
        "prompt tells the agent to read the preinjected manifest"
    );
    assert!(
        p.contains("parallel"),
        "prompt permits parallel writes within a turn"
    );
    assert!(
        p.contains("User corrections"),
        "prompt tells the agent to watch for user corrections as feedback signal"
    );
    assert!(
        p.contains("feedback_*"),
        "prompt names the feedback_* key namespace for corrections"
    );
    assert!(
        p.contains("dated observations"),
        "prompt teaches dated-observation prose for environment assertions"
    );
    assert!(
        p.contains("self-falsify"),
        "prompt teaches environment assertions to carry a falsification step"
    );
    assert!(
        p.contains("evidence"),
        "prompt must tell the agent to supply window-grounded evidence quotes"
    );
    assert!(
        p.contains("rejected"),
        "prompt must warn that a save without a window quote is rejected"
    );
}

/// Byte-stability: two calls return identical bytes, so a forked run
/// sharing the provider sees a cache-stable prefix.
#[test]
fn test_extraction_prompt_byte_stable() {
    assert_eq!(
        extraction_prompt().as_bytes(),
        extraction_prompt().as_bytes()
    );
}

#[test]
fn test_prompt_states_evidence_window() {
    let prompt = build_extraction_prompt(&[]);
    assert!(
        prompt.contains("complete evidence window"),
        "the prompt states the boundary the host actually enforces: {prompt}"
    );
    assert!(
        prompt.contains("Cite only what they contain"),
        "the eligibility rule must be explicit: {prompt}"
    );
}

/// An empty store appends no manifest.
#[test]
fn test_build_prompt_no_manifest() {
    assert!(
        !build_extraction_prompt(&[]).contains("Existing memory files"),
        "empty store must not append a manifest block"
    );
}

/// A non-empty store appends the manifest heading + each entry's key so
/// the forked agent can dedup. The key and the "Existing memory files"
/// heading are the load-bearing bits the agent reads before writing.
#[test]
fn test_build_prompt_appends_manifest() {
    let mem = MemorySummary {
        key: "build-gate".into(),
        description: "make check must stay green".into(),
        source: MemorySource::Project,
        mtime_secs: 0,
        scope: MemoryScope::Auto,
        origin: MemoryOrigin::Unknown,
    };
    let p = build_extraction_prompt(&[mem]);
    assert!(p.contains("## Existing memory files"));
    assert!(p.contains("build-gate"));
    assert!(p.contains("update an existing file rather than creating a duplicate"));
}
