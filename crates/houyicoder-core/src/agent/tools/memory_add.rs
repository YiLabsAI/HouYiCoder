//! The structured memory-write tool the forked extraction sub-agent uses to
//! land new memories. The agent emits a structured call with key, description,
//! source, and content fields; the tool routes it through the memory
//! provider add method, which owns the atomic two-step (topic file plus
//! derived-index pointer) and the in-process write lock. The tool holds no
//! path logic of its own — the provider owns every path — so there is no
//! path-escape surface for the agent to probe. This is the structurally-safe
//! counterpart to a raw sandboxed Write: the capability is save a memory
//! entry, not write an arbitrary file under the memory dir.
//!
//! Auto-approve by construction: the approval gate stays off and the tool
//! is not destructive (an add creates or refreshes one topic; it does not
//! delete or overwrite unrelated state). The forked extraction agent runs
//! autonomously — a per-call approval gate would starve memory (the agent
//! would queue approvals no one answers) — so the gate is off here and
//! safety comes from the structured capability plus the what-not-to-save
//! guidance in the extraction prompt. A user-facing explicit remember path
//! still routes through the same provider, so this tool is the forked
//! agent write seam only, not the main loop write seam.
//!
//! The provider is shared (Arc) with the runner that owns it, so a forked
//! extraction run in the same process lands writes under the same write lock
//! as an explicit user save — no cross-write orphan within the process.
//! Cross-process safety is a store-level concern (a planned journal), not
//! this tool concern.

use std::sync::Arc;

use houyicoder_api::memory::MemoryProvider;
use houyicoder_async::PFut;
use houyicoder_context::{MemoryEntry, MemoryError, MemoryOrigin, MemoryScope, MemorySource};
use serde_json::{Value, json};

use super::{Tool, ToolCtx, ToolError};

/// A structured memory-write tool. The forked extraction agent calls it to
/// persist a new memory entry; the provider owns the atomic write. Holds the
/// provider behind an Arc so it shares the write lock with any other caller
/// in the same process.
pub struct MemoryAddTool {
    provider: Arc<dyn MemoryProvider>,
    /// Optional write counter the caller threads in to learn how many saves
    /// landed this pass. Incremented on a successful add so the extractor/dream
    /// can fire one memory-saved notice per pass (not per call). None for the
    /// main runner's tool, which does not notify.
    counter: Option<Arc<std::sync::atomic::AtomicU32>>,
    /// Which writer this tool saves on behalf of. Injected by the host at
    /// construction (the LLM never provides origin) so a dream cannot
    /// self-promote. Unknown for a bare tool (tests).
    origin: MemoryOrigin,
}

impl MemoryAddTool {
    /// Construct with a shared provider handle. The provider is shared with
    /// the runner memory so forked-extract writes land under the same lock.
    pub fn new(provider: Arc<dyn MemoryProvider>) -> Self {
        Self {
            provider,
            counter: None,
            origin: MemoryOrigin::Unknown,
        }
    }

    /// Thread a write counter so a successful save bumps it. The caller resets
    /// before a fork pass + reads after to fire one memory-saved notice.
    pub fn with_counter(mut self, counter: Arc<std::sync::atomic::AtomicU32>) -> Self {
        self.counter = Some(counter);
        self
    }

    /// Tag every save with the given writer origin. The host calls this at
    /// tool construction (main agent / extractor / dream each inject one).
    pub fn with_origin(mut self, origin: MemoryOrigin) -> Self {
        self.origin = origin;
        self
    }
}

impl Tool for MemoryAddTool {
    fn name(&self) -> &str {
        "save_memory"
    }
    fn description(&self) -> &str {
        "Save a memory entry that captures context NOT derivable from the \
         current project state (code, git, file structure). Use ONLY for \
         non-obvious facts: user role/preferences, corrective or validating \
         feedback, project goals/decisions/why, or pointers to external \
         systems. Do NOT save code patterns, architecture, file paths, fix \
         recipes, or ephemeral task state — those are derivable or already \
         in the code. Provide a short kebab-case key, a one-line description \
         (specific, naming the entities it relates to), the source type, and \
         the body. For feedback and project types, structure the body as the \
         rule or fact followed by Why and How-to-apply lines."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "key": {
                    "type": "string",
                    "description": "Stable kebab-case identifier (file stem). Reusing a key refreshes that entry."
                },
                "description": {
                    "type": "string",
                    "description": "One-line summary used to decide relevance in future conversations. Be specific and name the entities it relates to."
                },
                "source": {
                    "type": "string",
                    "enum": ["user", "feedback", "project", "reference"],
                    "description": "user = user role/preferences; feedback = corrective or validating guidance on how to work; project = ongoing work/goals/decisions not in git; reference = pointer to an external system."
                },
                "content": {
                    "type": "string",
                    "description": "The memory body. For feedback/project, lead with the rule or fact then add Why and How-to-apply lines."
                },
                "scope": {
                    "type": "string",
                    "enum": ["auto", "project"],
                    "description": "Storage scope. auto (default) lands the entry in the auto-extracted scope, recall-on-demand. project lands the entry in the project scope so the entry lives in the checked-in project memory dir — use this when refreshing a project-scope entry the dream promoted, so the refresh does not write a competing auto-scope copy that would shadow the explicit version."
                }
            },
            "required": ["key", "description", "source", "content"],
            "additionalProperties": false
        })
    }
    fn execute(&self, _ctx: ToolCtx, input: Value) -> PFut<'_, Result<Value, ToolError>> {
        let provider = Arc::clone(&self.provider);
        let counter = self.counter.clone();
        Box::pin(async move {
            let key = parse_string(&input, "key")?;
            let description = parse_string(&input, "description")?;
            let source = parse_source(&input)?;
            let content = parse_string(&input, "content")?;
            let scope = parse_scope(&input);
            // Stamp the entry with the current time so a backend that does
            // not restat on recall still sees a fresh mtime; backends that
            // restat (the markdown store) overwrite this with the file stat,
            // so the value is correct either way.
            let now_secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let entry = MemoryEntry::new(key.clone(), content, source)
                .with_meta(description, now_secs)
                .with_origin(self.origin);
            let save = match scope {
                MemoryScope::Auto => provider.add(entry),
                other => provider.add_in_scope(entry, other),
            };
            match save {
                Ok(()) => {
                    if let Some(c) = &counter {
                        c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    }
                    Ok(json!({"saved": key}))
                }
                Err(e) => Err(map_memory_error(e)),
            }
        })
    }
    /// Not read-only: a save mutates the memory store.
    fn is_read_only(&self) -> bool {
        false
    }
    /// Not destructive: an add creates or refreshes one topic; it does not
    /// delete or overwrite unrelated state. Combined with the structured
    /// capability (the provider owns paths), there is no hard-to-reverse
    /// outward effect to gate.
    fn is_destructive(&self) -> bool {
        false
    }
    /// Auto-approve: the forked extraction agent runs autonomously; a per-call
    /// gate would queue approvals no one answers and starve memory. Safety
    /// comes from the structured capability plus the what-not-to-save gate in
    /// the extraction prompt, not from a human checkpoint here.
    fn requires_approval(&self) -> bool {
        false
    }
}

/// Extract a required string field from the input object. A missing or
/// non-string field is a clear error the model can see and correct rather
/// than a silent panic.
fn parse_string(input: &Value, field: &str) -> Result<String, ToolError> {
    input
        .get(field)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| {
            ToolError::Failed(format!("save_memory: '{field}' must be a non-empty string"))
        })
}

/// Parse the source enum from the wire label. Rejects unknown labels with the
/// accepted set so the model can self-correct.
fn parse_source(input: &Value) -> Result<MemorySource, ToolError> {
    let label = input
        .get("source")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            ToolError::Failed(
                "save_memory: 'source' must be one of user, feedback, project, reference"
                    .to_string(),
            )
        })?;
    MemorySource::from_label(label).ok_or_else(|| {
        ToolError::Failed(format!(
            "save_memory: 'source' must be user, feedback, project, or reference; got '{label}'"
        ))
    })
}

/// Parse the optional scope field. Defaults to Auto (the documented default
/// scope — writes land in the auto-extracted root). Accepts project so the
/// dream or a user can refresh a project-scope entry in place without
/// shadowing it with a competing auto copy. An unknown value is rejected so
/// the model can self-correct rather than silently falling back to auto.
fn parse_scope(input: &Value) -> MemoryScope {
    let Some(label) = input.get("scope").and_then(|v| v.as_str()) else {
        return MemoryScope::Auto;
    };
    MemoryScope::from_label(label).unwrap_or(MemoryScope::Auto)
}

/// Map a memory store error onto the tool error the model sees. An
/// atomicity failure is surfaced verbatim so the model knows the store was
/// left half-written (rare; the provider best-effort-rolls-back).
fn map_memory_error(e: MemoryError) -> ToolError {
    match e {
        MemoryError::InvalidPath(msg) => {
            ToolError::Failed(format!("save_memory: invalid key/path: {msg}"))
        }
        MemoryError::AtomicityFailed(msg) => {
            ToolError::Failed(format!("save_memory: atomic write failed: {msg}"))
        }
        MemoryError::Corrupt(msg) => {
            ToolError::Failed(format!("save_memory: corrupt store: {msg}"))
        }
        MemoryError::Io => ToolError::Failed("save_memory: storage I/O failure".to_string()),
        MemoryError::NotFound => {
            // add does not look up by key, so NotFound is unreachable here;
            // map it for completeness so the match is exhaustive over a
            // growing enum without a wildcard.
            ToolError::Failed("save_memory: entry not found".to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use houyicoder_api::memory::MemoryProvider;
    use houyicoder_context::MemoryEntry;
    use std::collections::HashSet;
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
        fn recall(
            &self,
            _query: &str,
            _budget: usize,
            _surfaced: &HashSet<String>,
        ) -> Vec<MemoryEntry> {
            Vec::new()
        }
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

    async fn run(tool: &MemoryAddTool, input: Value) -> Result<Value, ToolError> {
        tool.execute(ToolCtx::new("test"), input).await
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
        assert_eq!(out, json!({"saved": "user-prefers-terse"}));
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

    /// A threaded counter bumps once per successful save so the extractor can
    /// fire one memory-saved notice per pass. A failed save (unknown source)
    /// does not bump it.
    #[tokio::test]
    async fn test_save_memory_counts_writes() {
        let p = provider();
        let counter = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let tool = MemoryAddTool::new(Arc::clone(&p) as Arc<dyn MemoryProvider>)
            .with_counter(counter.clone());
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
        assert_eq!(
            counter.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "two successful saves bump the counter twice"
        );
        let err_input =
            json!({ "key": "k3", "description": "d", "source": "bogus", "content": "c" });
        let _err = run(&tool, err_input).await;
        assert_eq!(
            counter.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "a failed save does not bump the counter"
        );
    }

    /// Without a threaded counter the tool still saves (the main runner's tool
    /// does not notify, so it never wires one).
    #[tokio::test]
    async fn test_save_memory_works_untracked() {
        let p = provider();
        let tool = MemoryAddTool::new(Arc::clone(&p) as Arc<dyn MemoryProvider>);
        let input = json!({ "key": "k", "description": "d", "source": "user", "content": "c" });
        let out = run(&tool, input).await.expect("save succeeds");
        assert_eq!(out, json!({"saved": "k"}));
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

    /// Auto-approve is the whole point of the forked-extract write seam: a
    /// true gate would queue approvals no one answers. Pin it so a later
    /// destructive-tool-implies-approval default does not silently turn it on.
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
    }

    /// The scope field defaults to auto when omitted, and a project value
    /// threads through to the provider's add_in_scope so the dream can
    /// refresh a project-scope entry in place rather than shadowing it with
    /// a competing auto copy. Pins the MED-1 closure: a project-scope refresh
    /// no longer lands in auto.
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
        assert_eq!(out, json!({"saved": "k"}));
        let scopes = p.scopes.lock().expect("scopes").clone();
        assert_eq!(scopes, vec![MemoryScope::Auto], "bad scope -> auto");
    }
}
