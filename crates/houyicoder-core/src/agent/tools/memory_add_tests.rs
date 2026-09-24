use super::*;
use houyicoder_api::memory::{MemoryProvider, MemoryWriteOutcome};
use houyicoder_context::{EventId, MemoryEntry, SessionEvent, SessionId, SessionLogEntry};
use std::sync::Mutex;

/// An in-memory capturing provider so the tool test stays deterministic
/// and asserts the structured call reached add with the right entry.
/// Records the scope the caller passed so a scope-field test can assert
/// the project scope threaded through to the provider.
struct RecordingMemory {
    writes: Mutex<Vec<MemoryEntry>>,
    scopes: Mutex<Vec<MemoryScope>>,
}

impl MemoryProvider for RecordingMemory {
    fn add(&self, entry: MemoryEntry) -> Result<(), MemoryError> {
        self.writes.lock().expect("writes").push(entry);
        self.scopes.lock().expect("scopes").push(MemoryScope::Auto);
        Ok(())
    }
    fn add_in_scope(&self, entry: MemoryEntry, scope: MemoryScope) -> Result<(), MemoryError> {
        self.writes.lock().expect("writes").push(entry);
        self.scopes.lock().expect("scopes").push(scope);
        Ok(())
    }
}

fn provider() -> Arc<RecordingMemory> {
    Arc::new(RecordingMemory {
        writes: Mutex::new(Vec::new()),
        scopes: Mutex::new(Vec::new()),
    })
}

struct UnchangedMemory;

impl MemoryProvider for UnchangedMemory {
    fn add(&self, _entry: MemoryEntry) -> Result<(), MemoryError> {
        Ok(())
    }

    fn add_if_changed(&self, _entry: MemoryEntry) -> Result<MemoryWriteOutcome, MemoryError> {
        Ok(MemoryWriteOutcome::Unchanged)
    }
}

async fn run(tool: &MemoryAddTool, input: Value) -> Result<Value, ToolError> {
    tool.execute(ToolCtx::new("test"), input).await
}

/// Build a one-entry evidence window carrying a single user message, so a
/// test can pass the tool a window whose body a quote can match against.
fn window(text: &str) -> Arc<[SessionLogEntry]> {
    let entry = SessionLogEntry {
        id: EventId::new(),
        session: SessionId::new(),
        ts: 0,
        prev_hash: None,
        event: SessionEvent::UserInput {
            text: text.to_string(),
        },
    };
    Arc::from(vec![entry])
}

#[tokio::test]
async fn test_save_lands_structured_entry() {
    let p = provider();
    let tool = MemoryAddTool::new(Arc::clone(&p) as Arc<dyn MemoryProvider>);
    let input = json!({
        "key": "user-prefers-terse",
        "description": "User prefers terse responses without preamble",
        "source": "feedback",
        "content": "Keep responses terse.\n**Why:** the user said the long intros waste their time.\n**How to apply:** drop preamble, lead with the answer."
    });
    let out = run(&tool, input).await.expect("save succeeds");
    assert_eq!(
        out,
        json!({"saved": "user-prefers-terse", "outcome": "created"})
    );
    let writes = p.writes.lock().expect("writes").clone();
    assert_eq!(writes.len(), 1, "exactly one entry landed");
    let e = &writes[0];
    assert_eq!(e.key, "user-prefers-terse");
    assert_eq!(e.source, MemorySource::Feedback);
    assert_eq!(
        e.description,
        "User prefers terse responses without preamble"
    );
    assert!(e.content.contains("**Why:**"));
    assert!(e.mtime_secs > 0, "mtime stamped with now");
}

/// A threaded recorder bumps once per successful save so the extractor can
/// fire one memory-saved notice per pass. A failed save (unknown source)
/// does not bump it.
#[tokio::test]
async fn test_save_memory_counts_writes() {
    let p = provider();
    let recorder = Arc::new(MutationLog::new());
    let tool = MemoryAddTool::new(Arc::clone(&p) as Arc<dyn MemoryProvider>)
        .with_recorder(recorder.clone());
    let input = json!({
        "key": "k1",
        "description": "d",
        "source": "user",
        "content": "c"
    });
    run(&tool, input.clone()).await.expect("first save");
    run(
        &tool,
        json!({ "key": "k2", "description": "d", "source": "user", "content": "c" }),
    )
    .await
    .expect("second save");
    let changes = recorder.take();
    assert_eq!(changes.len(), 2);
    assert_eq!(changes[0].key, "k1");
    assert_eq!(changes[1].key, "k2");
    let err_input = json!({ "key": "k3", "description": "d", "source": "bogus", "content": "c" });
    let _err = run(&tool, err_input).await;
    assert!(recorder.take().is_empty());
}

/// The recorded change carries the scope the save was addressed to, so the
/// notice can name it. The scope the provider saw and the scope the recorder
/// logged are asserted to be the same fact.
#[tokio::test]
async fn test_save_records_scope() {
    let p = provider();
    let recorder = Arc::new(MutationLog::new());
    let tool = MemoryAddTool::new(Arc::clone(&p) as Arc<dyn MemoryProvider>)
        .with_recorder(recorder.clone());
    run(
        &tool,
        json!({ "key": "k-auto", "description": "d", "source": "user", "content": "c" }),
    )
    .await
    .expect("default save lands in auto");
    run(
        &tool,
        json!({
            "key": "k-proj",
            "description": "d",
            "source": "project",
            "content": "c",
            "scope": "project"
        }),
    )
    .await
    .expect("project-scope save");
    let changes = recorder.take();
    assert_eq!(changes.len(), 2, "both saves notify");
    assert_eq!(changes[0].scope, MemoryScope::Auto);
    assert_eq!(
        changes[1].scope,
        MemoryScope::Project,
        "the recorded scope follows the write"
    );
    let seen = p.scopes.lock().expect("scopes").clone();
    assert_eq!(
        changes.iter().map(|c| c.scope).collect::<Vec<_>>(),
        seen,
        "the recorded scope is the scope the provider was addressed with"
    );
}

#[tokio::test]
async fn test_repeat_save_emits_once() {
    let root = std::env::temp_dir().join(format!("memory-add-unchanged-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("create memory root");
    let provider = Arc::new(houyicoder_memory::MarkdownMemoryProvider::new(root.clone()));
    let recorder = Arc::new(MutationLog::new());
    let tool = MemoryAddTool::new(provider).with_recorder(recorder.clone());
    let input = json!({ "key": "stable", "description": "d", "source": "user", "content": "c" });
    run(&tool, input.clone()).await.expect("first save");
    assert_eq!(recorder.take().len(), 1, "the first save is observable");
    let second = run(&tool, input).await.expect("repeated save");
    assert_eq!(second, json!({"saved": "stable", "outcome": "unchanged"}));
    assert!(recorder.take().is_empty(), "the repeated save is silent");
    std::fs::remove_dir_all(root).ok();
}

/// A fresh key records Created; rewriting the same key with new content
/// records Updated. The recorder must carry the provider outcome, not a
/// flat Stored label, so the notification distinguishes a new memory from
/// an update.
#[tokio::test]
async fn test_save_maps_outcome() {
    let root = std::env::temp_dir().join(format!("memory-outcome-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("create memory root");
    let store = Arc::new(houyicoder_memory::MarkdownMemoryProvider::new(root.clone()));
    let recorder = Arc::new(MutationLog::new());
    let tool = MemoryAddTool::new(store).with_recorder(recorder.clone());
    let base =
        json!({ "key": "outcome-key", "description": "d", "source": "user", "content": "v1" });
    run(&tool, base).await.expect("first save");
    let updated =
        json!({ "key": "outcome-key", "description": "d", "source": "user", "content": "v2" });
    run(&tool, updated).await.expect("rewrite with new content");
    let changes = recorder.take();
    assert_eq!(changes.len(), 2, "both saves notify");
    assert_eq!(
        changes[0].operation,
        MemoryOperation::Created,
        "fresh key records Created"
    );
    assert_eq!(
        changes[1].operation,
        MemoryOperation::Updated,
        "changed existing key records Updated"
    );
    std::fs::remove_dir_all(root).ok();
}

#[tokio::test]
async fn test_unchanged_save_is_silent() {
    let recorder = Arc::new(MutationLog::new());
    let tool = MemoryAddTool::new(Arc::new(UnchangedMemory)).with_recorder(recorder.clone());
    let input = json!({ "key": "k", "description": "d", "source": "user", "content": "c" });
    let output = run(&tool, input).await.expect("unchanged save succeeds");
    assert_eq!(output, json!({"saved": "k", "outcome": "unchanged"}));
    assert!(recorder.take().is_empty(), "unchanged writes do not notify");
}

/// Without a threaded recorder the tool still saves (the main runner's tool
/// does not notify, so it never wires one).
#[tokio::test]
async fn test_save_memory_works_untracked() {
    let p = provider();
    let tool = MemoryAddTool::new(Arc::clone(&p) as Arc<dyn MemoryProvider>);
    let input = json!({ "key": "k", "description": "d", "source": "user", "content": "c" });
    let out = run(&tool, input).await.expect("save succeeds");
    assert_eq!(out, json!({"saved": "k", "outcome": "created"}));
}

#[tokio::test]
async fn test_save_rejects_unknown_source() {
    let p = provider();
    let tool = MemoryAddTool::new(Arc::clone(&p) as Arc<dyn MemoryProvider>);
    let input = json!({
        "key": "k",
        "description": "d",
        "source": "personal",
        "content": "c"
    });
    let err = run(&tool, input)
        .await
        .expect_err("unknown source rejected");
    let msg = err.to_string();
    assert!(
        msg.contains("user, feedback, project, or reference"),
        "error names the accepted set: {msg}"
    );
    assert!(
        p.writes.lock().expect("writes").is_empty(),
        "no write landed on a rejected source"
    );
}

#[tokio::test]
async fn test_save_rejects_missing_field() {
    let p = provider();
    let tool = MemoryAddTool::new(Arc::clone(&p) as Arc<dyn MemoryProvider>);
    let input = json!({
        "key": "k",
        "source": "user",
        "content": "c"
    });
    let err = run(&tool, input)
        .await
        .expect_err("missing description rejected");
    assert!(
        err.to_string().contains("'description'"),
        "error names the missing field: {}",
        err
    );
}

/// Auto-approve is required by the forked-extract write seam because a
/// true gate would queue approvals with no responder.
#[test]
fn test_save_memory_auto_approves() {
    let p = provider();
    let tool = MemoryAddTool::new(Arc::clone(&p) as Arc<dyn MemoryProvider>);
    assert!(!tool.requires_approval(), "auto-approve must hold");
    assert!(!tool.is_destructive(), "an add is not destructive");
    assert!(!tool.is_read_only(), "a save mutates the store");
}

/// The structured capability surface: the tool exposes no path field, so
/// there is no path argument for the model to probe. The schema pins
/// exactly five fields (the four structured fields plus the optional
/// scope) with no additional properties.
#[test]
fn test_save_schema_pins_fields() {
    let p = provider();
    let tool = MemoryAddTool::new(Arc::clone(&p) as Arc<dyn MemoryProvider>);
    let schema = tool.input_schema();
    let props = schema
        .get("properties")
        .and_then(|v| v.as_object())
        .expect("properties object");
    assert!(
        !props.contains_key("path"),
        "no path field — the provider owns paths"
    );
    let mut keys: Vec<&String> = props.keys().collect();
    keys.sort();
    assert_eq!(
        keys,
        vec![
            &"content".to_string(),
            &"description".to_string(),
            &"key".to_string(),
            &"scope".to_string(),
            &"source".to_string(),
        ],
        "exactly the five structured fields"
    );
    // The enum lists every root the parser accepts, so a label the
    // schema hides cannot be a label the parser would have taken.
    let scope_enum = props["scope"]["enum"].as_array().expect("scope enum");
    assert_eq!(
        scope_enum,
        &vec![json!("user"), json!("auto"), json!("project")],
        "schema enum and parser agree on the roots"
    );
}

/// The pinned construction hides the scope field: four properties, none
/// named scope, so the extraction model is never offered a root choice.
#[test]
fn test_extraction_schema_hides_scope() {
    let p = provider();
    let tool = MemoryAddTool::new_extraction(
        Arc::clone(&p) as Arc<dyn MemoryProvider>,
        Arc::new(MutationLog::new()),
        Arc::from(Vec::new()),
    );
    let schema = tool.input_schema();
    let props = schema
        .get("properties")
        .and_then(|v| v.as_object())
        .expect("properties object");
    assert!(
        !props.contains_key("scope"),
        "a pinned tool exposes no scope field"
    );
    let mut keys: Vec<&String> = props.keys().collect();
    keys.sort();
    assert_eq!(
        keys,
        vec![
            &"content".to_string(),
            &"description".to_string(),
            &"evidence".to_string(),
            &"key".to_string(),
            &"source".to_string(),
        ],
        "the pinned tool exposes evidence, not scope"
    );
    let req = schema["required"].as_array().expect("required array");
    assert!(
        req.contains(&json!("evidence")),
        "evidence is required on the extraction seam"
    );
}

/// The pin answers whatever the input claims: a scope label in the call
/// is ignored and the write still lands in the auto root, stamped with
/// the extractor origin and recorded for the pass notice — when the
/// evidence quote is grounded in the window.
#[tokio::test]
async fn test_extraction_pin_ignores_input() {
    let p = provider();
    let recorder = Arc::new(MutationLog::new());
    let tool = MemoryAddTool::new_extraction(
        Arc::clone(&p) as Arc<dyn MemoryProvider>,
        Arc::clone(&recorder),
        window("the ledger retention rule ships this week"),
    );
    let out = run(
        &tool,
        json!({
            "key": "k",
            "description": "d",
            "source": "feedback",
            "content": "c",
            "scope": "project",
            "evidence": [{"quote": "the ledger retention rule ships this week"}]
        }),
    )
    .await
    .expect("a grounded save succeeds");
    assert_eq!(out, json!({"saved": "k", "outcome": "created"}));
    let scopes = p.scopes.lock().expect("scopes").clone();
    assert_eq!(
        scopes,
        vec![MemoryScope::Auto],
        "the pin overrides the input label"
    );
    let writes = p.writes.lock().expect("writes").clone();
    assert_eq!(
        writes[0].origin,
        MemoryOrigin::Extractor,
        "the seam stamps the writer"
    );
    assert_eq!(recorder.take().len(), 1, "the save notifies once");
}

/// A save without evidence is rejected before any write: the extraction
/// seam grounds every save in a window quote. Pre-fix this was accepted.
#[tokio::test]
async fn test_extraction_rejects_no_evidence() {
    let p = provider();
    let recorder = Arc::new(MutationLog::new());
    let tool = MemoryAddTool::new_extraction(
        Arc::clone(&p) as Arc<dyn MemoryProvider>,
        Arc::clone(&recorder),
        window("the user prefers terse replies"),
    );
    let err = run(
        &tool,
        json!({
            "key": "k",
            "description": "d",
            "source": "feedback",
            "content": "c"
        }),
    )
    .await
    .expect_err("a save without evidence is rejected");
    assert!(
        err.to_string().contains("'evidence'"),
        "the error names the missing evidence: {err}"
    );
    assert!(
        p.writes.lock().expect("writes").is_empty(),
        "no write landed on rejection"
    );
    assert!(
        recorder.take().is_empty(),
        "the recorder stays flat on rejection"
    );
}

/// A quote that is not a substring of any window event is rejected: a
/// save keyed on a manifest-only fact (no window quote) cannot land.
/// Pre-fix this was accepted.
#[tokio::test]
async fn test_extraction_rejects_fabricated_quote() {
    let p = provider();
    let recorder = Arc::new(MutationLog::new());
    let tool = MemoryAddTool::new_extraction(
        Arc::clone(&p) as Arc<dyn MemoryProvider>,
        Arc::clone(&recorder),
        window("the user prefers terse replies"),
    );
    let err = run(
        &tool,
        json!({
            "key": "manifest-only-fact",
            "description": "a fact from the manifest, not the window",
            "source": "project",
            "content": "c",
            "evidence": [{"quote": "this snippet never appeared in the window"}]
        }),
    )
    .await
    .expect_err("an ungrounded quote is rejected");
    assert!(
        err.to_string()
            .contains("not found in the conversation window"),
        "the error explains the quote was not in the window: {err}"
    );
    assert!(
        p.writes.lock().expect("writes").is_empty(),
        "no write landed on an ungrounded quote"
    );
    assert!(
        recorder.take().is_empty(),
        "the recorder stays flat on an ungrounded quote"
    );
}

/// A quote that drifted in whitespace and line breaks still matches its
/// source body, so the model may copy a snippet across a line break.
#[tokio::test]
async fn test_extraction_accepts_normalized_quote() {
    let p = provider();
    let recorder = Arc::new(MutationLog::new());
    let tool = MemoryAddTool::new_extraction(
        Arc::clone(&p) as Arc<dyn MemoryProvider>,
        Arc::clone(&recorder),
        window("the user said:\n  drop the preamble, lead with the answer"),
    );
    let out = run(
        &tool,
        json!({
            "key": "terse",
            "description": "d",
            "source": "feedback",
            "content": "c",
            "evidence": [{"quote": "drop the preamble, lead with the answer"}]
        }),
    )
    .await
    .expect("a whitespace-drifted quote still matches");
    assert_eq!(out, json!({"saved": "terse", "outcome": "created"}));
    assert_eq!(recorder.take().len(), 1, "a grounded save notifies once");
}

/// Evidence with more than three quotes is rejected: the seam caps the
/// citation count so a save cannot pad with decoy quotes.
#[tokio::test]
async fn test_extraction_rejects_too_many() {
    let p = provider();
    let tool = MemoryAddTool::new_extraction(
        Arc::clone(&p) as Arc<dyn MemoryProvider>,
        Arc::new(MutationLog::new()),
        window("a b c d"),
    );
    let err = run(
        &tool,
        json!({
            "key": "k",
            "description": "d",
            "source": "user",
            "content": "c",
            "evidence": [
                {"quote": "a"}, {"quote": "b"}, {"quote": "c"}, {"quote": "d"}
            ]
        }),
    )
    .await
    .expect_err("more than three quotes is rejected");
    assert!(
        err.to_string().contains("at most three"),
        "the error names the cap: {err}"
    );
}

/// Evidence may quote a tool call's input or its result's output — the
/// model sees both JSON bodies in the projection, so a save can ground in
/// either rather than only user-typed text.
#[tokio::test]
async fn test_extraction_quotes_tool_bodies() {
    let p = provider();
    let recorder = Arc::new(MutationLog::new());
    let session = SessionId::new();
    let call = SessionLogEntry {
        id: EventId::new(),
        session,
        ts: 0,
        prev_hash: None,
        event: SessionEvent::ToolCall {
            call_id: "c1".into(),
            tool: "run_build".into(),
            input: serde_json::json!({"target": "build-gate passes"}),
        },
    };
    let result = SessionLogEntry {
        id: EventId::new(),
        session,
        ts: 0,
        prev_hash: None,
        event: SessionEvent::ToolResult {
            call_id: "c1".into(),
            output: serde_json::json!({"line": "build-gate passes in 4s"}),
            duration_ms: 0,
        },
    };
    let window: Arc<[SessionLogEntry]> = Arc::from(vec![call, result]);
    let tool = MemoryAddTool::new_extraction(
        Arc::clone(&p) as Arc<dyn MemoryProvider>,
        Arc::clone(&recorder),
        window,
    );
    let out = run(
        &tool,
        json!({
            "key": "build-gate",
            "description": "d",
            "source": "project",
            "content": "c",
            "evidence": [
                {"quote": "build-gate passes"},
                {"quote": "build-gate passes in 4s"}
            ]
        }),
    )
    .await
    .expect("quotes from a tool call and its result both ground the save");
    assert_eq!(out, json!({"saved": "build-gate", "outcome": "created"}));
    assert_eq!(
        recorder.take().len(),
        1,
        "a tool-body-grounded save notifies once"
    );
}

/// Host-injected system reminders (memory recall, skill listing) are not
/// conversation evidence: a quote copied from their text is rejected, so a
/// save cannot ground in the manifest it was handed and form a
/// self-referential loop (manifest -> recall -> evidence -> write).
#[tokio::test]
async fn test_extraction_rejects_injected_text() {
    let p = provider();
    let session = SessionId::new();
    let recall = SessionLogEntry {
        id: EventId::new(),
        session,
        ts: 0,
        prev_hash: None,
        event: SessionEvent::MemoryRecall {
            text: "Existing memory files: build-gate".to_string(),
            keys: Vec::new(),
            bytes: 0,
        },
    };
    let listing = SessionLogEntry {
        id: EventId::new(),
        session,
        ts: 0,
        prev_hash: None,
        event: SessionEvent::SkillListing {
            text: "Available skill: deploy-gate".to_string(),
            bytes: 0,
            content_hash: 0,
        },
    };
    let window: Arc<[SessionLogEntry]> = Arc::from(vec![recall, listing]);
    let tool = MemoryAddTool::new_extraction(
        Arc::clone(&p) as Arc<dyn MemoryProvider>,
        Arc::new(MutationLog::new()),
        window,
    );
    let err = run(
        &tool,
        json!({
            "key": "build-gate",
            "description": "d",
            "source": "project",
            "content": "c",
            "evidence": [
                {"quote": "build-gate"},
                {"quote": "deploy-gate"}
            ]
        }),
    )
    .await
    .expect_err("host-injected reminder text is not evidence");
    assert!(
        err.to_string()
            .contains("not found in the conversation window"),
        "the error names the grounding failure: {err}"
    );
    assert!(
        p.writes.lock().expect("writes").is_empty(),
        "no write lands when the only quotes come from injected reminders"
    );
}

/// The scope field defaults to auto when omitted, and a project value
/// threads through to the provider's add_in_scope so the dream refreshes
/// a project-scope entry in place rather than shadowing it with a
/// competing auto copy.
#[tokio::test]
async fn test_save_scope_threads_through() {
    let p = provider();
    let tool = MemoryAddTool::new(Arc::clone(&p) as Arc<dyn MemoryProvider>);
    // Default: no scope field -> Auto.
    run(
        &tool,
        json!({
            "key": "k-auto",
            "description": "d",
            "source": "user",
            "content": "c"
        }),
    )
    .await
    .expect("default save");
    // Explicit project scope -> add_in_scope(Project).
    run(
        &tool,
        json!({
            "key": "k-proj",
            "description": "d",
            "source": "project",
            "content": "c",
            "scope": "project"
        }),
    )
    .await
    .expect("project-scope save");
    let scopes = p.scopes.lock().expect("scopes").clone();
    assert_eq!(
        scopes,
        vec![MemoryScope::Auto, MemoryScope::Project],
        "scope field threads through to the provider"
    );
    let writes = p.writes.lock().expect("writes").clone();
    assert_eq!(writes.len(), 2, "both saves landed");
    assert_eq!(writes[0].key, "k-auto");
    assert_eq!(writes[1].key, "k-proj");
}

/// An unknown scope value falls back to auto rather than rejecting the
/// call: scope is an advisory field and the model's intent was to save.
/// A bad value still saves, so a typo does not starve memory.
#[tokio::test]
async fn test_save_bad_scope_fallback() {
    let p = provider();
    let tool = MemoryAddTool::new(Arc::clone(&p) as Arc<dyn MemoryProvider>);
    let out = run(
        &tool,
        json!({
            "key": "k",
            "description": "d",
            "source": "user",
            "content": "c",
            "scope": "bogus"
        }),
    )
    .await
    .expect("bad scope falls back to auto");
    assert_eq!(out, json!({"saved": "k", "outcome": "created"}));
    let scopes = p.scopes.lock().expect("scopes").clone();
    assert_eq!(scopes, vec![MemoryScope::Auto], "bad scope -> auto");
}
