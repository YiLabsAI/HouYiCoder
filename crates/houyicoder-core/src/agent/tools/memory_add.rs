//! The structured memory-write tool the memory-writing seams share to
//! land new memories. The agent emits a structured call with key, description,
//! source, and content fields; the tool routes it through the memory
//! provider add method, which owns the atomic two-step (topic file plus
//! derived-index pointer) and the in-process write lock. The tool holds no
//! path logic of its own — the provider owns every path — so there is no
//! path-escape surface for the agent to probe. This is the structurally-safe
//! alternative to a raw sandboxed Write: the capability is save a memory
//! entry, not write an arbitrary file under the memory dir.
//!
//! Auto-approve by construction: the approval gate stays off and the tool
//! is not destructive (an add creates or refreshes one topic; it does not
//! delete or overwrite unrelated state). The forked extraction agent runs
//! autonomously — a per-call approval gate would starve memory (the agent
//! would queue approvals no one answers) — so the gate is off here and
//! safety comes from the structured capability plus the what-not-to-save
//! guidance in the extraction prompt. Each seam registers its own
//! construction: origin is host-stamped per seam, and the extraction and
//! dream ones pin the storage root so their writes cannot open a root the
//! host did not name — a pinned save lands in the pinned root, except the
//! host-side refresh routing for keys already in the project root.
//!
//! The provider is shared with the runner that owns it, so a forked
//! extraction run in the same process lands writes under the same write lock
//! as an explicit user save — no cross-write orphan within the process.
//! Cross-process safety is a store-level concern (a planned journal), not
//! this tool concern.

use std::sync::Arc;

use houyicoder_api::memory::{MemoryProvider, MemoryWriteOutcome};
use houyicoder_async::PFut;
use houyicoder_context::{
    MemoryEntry, MemoryError, MemoryOrigin, MemoryScope, MemorySource, SessionEvent,
    SessionLogEntry,
};
use serde_json::{Value, json};

use super::{Tool, ToolCtx, ToolError};
use crate::agent::memory::MutationLog;
use houyicoder_api::agent_event::MemoryOperation;
use tracing::debug;

/// A structured memory-write tool. The forked extraction agent calls it to
/// persist a new memory entry; the provider owns the atomic write. Holds the
/// provider behind an Arc so it shares the write lock with any other caller
/// in the same process.
pub struct MemoryAddTool {
    provider: Arc<dyn MemoryProvider>,
    /// Optional write recorder the caller threads in to learn how many saves
    /// landed. Incremented on a successful add. The forked-extraction seam
    /// resets it before a pass and reads it after to fire one AutoMemory
    /// notice; the main-agent seam leaves it for the runtime to drain when the
    /// turn settles into a PrimaryAgent notice.
    recorder: Option<Arc<MutationLog>>,
    /// Which writer this tool saves on behalf of. Injected by the host at
    /// construction (the LLM never provides origin) so a dream cannot
    /// self-promote. Unknown for a bare tool (tests).
    origin: MemoryOrigin,
    /// Storage root pinned by the host. Some on the forked-extraction and
    /// dream seams, where the model is offered no scope field and every write
    /// lands in the auto root; None where the model picks the root per call.
    scope: Option<MemoryScope>,
    /// Whether saves must quote window evidence. Set by the extraction seam,
    /// which owns a real window; a pinned tool without a window (the dream)
    /// checks nothing rather than fabricate a ground.
    requires_evidence: bool,
    /// The evidence window the extraction seam validates quotes against. Empty
    /// on the unpinned main-agent tool (no grounding check); the host-owned
    /// window events on the extraction seam, so every save must quote text the
    /// forked agent actually saw rather than a manifest-only fact.
    evidence: Arc<[SessionLogEntry]>,
}

impl MemoryAddTool {
    /// Construct with a shared provider handle. The provider is shared with
    /// the runner memory so forked-extract writes land under the same lock.
    pub fn new(provider: Arc<dyn MemoryProvider>) -> Self {
        Self {
            provider,
            recorder: None,
            origin: MemoryOrigin::Unknown,
            scope: None,
            requires_evidence: false,
            evidence: Arc::from(Vec::new()),
        }
    }

    /// The forked-extraction seam in one step: recorder, extractor origin,
    /// the auto-root pin, and the evidence window every save must quote. A
    /// scope choice offered to the extraction model gets taken, and the write
    /// then lands outside the isolated auto root — the pinned tool exposes no
    /// scope field at all.
    pub(crate) fn new_extraction(
        provider: Arc<dyn MemoryProvider>,
        recorder: Arc<MutationLog>,
        evidence: Arc<[SessionLogEntry]>,
    ) -> Self {
        Self {
            provider,
            recorder: Some(recorder),
            origin: MemoryOrigin::Extractor,
            scope: Some(MemoryScope::Auto),
            requires_evidence: true,
            evidence,
        }
    }

    /// Pin the storage root without an evidence window. The consolidation
    /// dream writes outside any conversation window, so quoting one is
    /// impossible; the pin keeps every new-key write in the pinned root
    /// while the model is offered no scope choice, and the refresh routing
    /// updates a project-root key in place.
    pub fn with_pinned_scope(mut self, scope: MemoryScope) -> Self {
        self.scope = Some(scope);
        self
    }

    /// Thread a write recorder so a successful save bumps it. The forked
    /// extraction seam resets before a pass and reads after to fire one
    /// AutoMemory notice; the main-agent seam leaves the recorder for the
    /// runtime to drain when the turn settles into a PrimaryAgent notice.
    pub(crate) fn with_recorder(mut self, recorder: Arc<MutationLog>) -> Self {
        self.recorder = Some(recorder);
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
        let mut schema = json!({
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
                }
            },
            "required": ["key", "description", "source", "content"],
            "additionalProperties": false
        });
        if self.requires_evidence {
            // The extraction seam grounds every save in window evidence: one
            // to three quotes the forked agent copies from the conversation it
            // saw, validated host-side as a substring of a window event's body
            // so a manifest-only fact (no window quote) is rejected.
            let props = schema["properties"]
                .as_object_mut()
                .expect("properties object built above");
            props.insert(
                "evidence".to_string(),
                json!({
                    "type": "array",
                    "minItems": 1,
                    "maxItems": 3,
                    "items": {
                        "type": "object",
                        "properties": {
                            "quote": {
                                "type": "string",
                                "description": "A non-empty snippet copied verbatim from the conversation window. Normalized (whitespace folded, Unicode NFC) host-side; must be a substring of some window message body. Two or three short quotes beat one long one."
                            }
                        },
                        "required": ["quote"],
                        "additionalProperties": false
                    },
                    "description": "One to three verbatim quotes from the conversation window that ground this memory. A save without a real window quote is rejected; the manifest above is not evidence."
                }),
            );
            if let Some(req) = schema["required"].as_array_mut() {
                req.push(json!("evidence"));
            }
        } else if self.scope.is_none() {
            // Only an unpinned tool offers the choice; the enum lists every
            // root the parser accepts, so schema and parser cannot disagree.
            let props = schema["properties"]
                .as_object_mut()
                .expect("properties object built above");
            props.insert(
                "scope".to_string(),
                json!({
                    "type": "string",
                    "enum": ["user", "auto", "project"],
                    "description": "Storage root. auto (default) lands the entry in the auto-extracted store, recall-on-demand; user lands it in the cross-project user store; project lands it in the checked-in project memory dir — use project when refreshing an entry the dream promoted, so the refresh does not write a competing auto copy that would shadow it."
                }),
            );
        }
        schema
    }
    fn execute(&self, _ctx: ToolCtx, input: Value) -> PFut<'_, Result<Value, ToolError>> {
        let provider = Arc::clone(&self.provider);
        let recorder = self.recorder.clone();
        Box::pin(async move {
            let key = parse_string(&input, "key")?;
            let description = parse_string(&input, "description")?;
            let source = parse_source(&input)?;
            let content = parse_string(&input, "content")?;
            // The extraction seam grounds every save in window evidence before
            // the write: a save whose quotes are not substrings of a window
            // event body is rejected with a correctable error, so no write
            // lands and the recorder stays flat. Unpinned tools skip this.
            if self.requires_evidence {
                validate_extraction_evidence(&self.evidence, &input)?;
            }
            // A host-pinned tool ignores any scope in the input: the pin is
            // the answer, whatever the caller sent. The pin resolves per
            // key: a save lands in the pinned root, except that a key
            // already living in the project root refreshes that root in
            // place — a competing copy in the pinned root would shadow the
            // project entry by newest-mtime and lose the refresh.
            let scope = match self.scope {
                Some(pinned) => refresh_root(provider.as_ref(), &key, pinned),
                None => parse_scope(&input),
            };
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
                MemoryScope::Auto => provider.add_if_changed(entry),
                other => provider.add_in_scope_if_changed(entry, other),
            };
            match save {
                Ok(outcome) => {
                    if outcome.changed()
                        && let Some(recorder) = &recorder
                    {
                        let op = match outcome {
                            MemoryWriteOutcome::Created => MemoryOperation::Created,
                            MemoryWriteOutcome::Updated => MemoryOperation::Updated,
                            MemoryWriteOutcome::Unchanged => MemoryOperation::Updated,
                        };
                        recorder.record(&key, op, scope);
                    }
                    let label = match outcome {
                        MemoryWriteOutcome::Created => "created",
                        MemoryWriteOutcome::Updated => "updated",
                        MemoryWriteOutcome::Unchanged => "unchanged",
                    };
                    Ok(json!({"saved": key, "outcome": label}))
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

/// Parse the source enum from the label the model sent. Rejects unknown
/// labels with the accepted set so the model can self-correct.
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
/// scope — writes land in the auto-extracted root). Accepts user and project
/// so the main agent or the dream can pick a root per call. An unknown value
/// falls back to auto rather than rejecting: the field is advisory and the
/// call's intent was to save, so a typo must not starve the write.
fn parse_scope(input: &Value) -> MemoryScope {
    let Some(label) = input.get("scope").and_then(|v| v.as_str()) else {
        return MemoryScope::Auto;
    };
    MemoryScope::from_label(label).unwrap_or(MemoryScope::Auto)
}

/// Resolve the write root for a pinned save. The pin names the default
/// root; the one exception is a key already living in the project root,
/// where the save refreshes that root in place. Writing the refresh into
/// the pinned root would create a competing copy that shadows the explicit
/// entry by newest-mtime and the refresh would be lost on the next
/// reconcile. Other roots stay closed to the pinned seam.
fn refresh_root(provider: &dyn MemoryProvider, key: &str, pinned: MemoryScope) -> MemoryScope {
    let refreshes_project = provider.scopes_for_key(key).contains(&MemoryScope::Project);
    if refreshes_project && pinned != MemoryScope::Project {
        MemoryScope::Project
    } else {
        pinned
    }
}

/// Build a correctable evidence-rejection error and trace it host-side so a
/// recurring extraction failure is observable in the tracing stream.
fn reject_evidence(msg: String) -> ToolError {
    debug!(target: "memory_add", "extraction evidence rejected: {msg}");
    ToolError::Failed(msg)
}

/// Validate the extraction seam's evidence: one to three quotes, each a
/// normalized substring of some window event's body. A save keyed on a
/// manifest-only fact has no quote the window contains, so it is rejected
/// with a correctable error before any write or recorder bump.
fn validate_extraction_evidence(
    window: &[SessionLogEntry],
    input: &Value,
) -> Result<(), ToolError> {
    let evidence = input
        .get("evidence")
        .and_then(|v| v.as_array())
        .ok_or_else(|| {
            reject_evidence(
                "save_memory: 'evidence' must be an array of 1..3 quotes copied from the conversation window"
                    .to_string(),
            )
        })?;
    if evidence.is_empty() {
        return Err(reject_evidence(
            "save_memory: 'evidence' must contain at least one quote from the conversation window"
                .to_string(),
        ));
    }
    if evidence.len() > 3 {
        return Err(reject_evidence(
            "save_memory: 'evidence' may contain at most three quotes".to_string(),
        ));
    }
    for (i, item) in evidence.iter().enumerate() {
        let quote = item.get("quote").and_then(|v| v.as_str()).ok_or_else(|| {
            reject_evidence(format!(
                "save_memory: evidence[{i}] must have a non-empty 'quote' copied from the conversation window"
            ))
        })?;
        let needle = normalize_quote(quote);
        if needle.is_empty() {
            return Err(reject_evidence(format!(
                "save_memory: evidence[{i}] quote is empty after normalization"
            )));
        }
        if !window
            .iter()
            .any(|e| normalized_body(&e.event).contains(&needle))
        {
            return Err(reject_evidence(format!(
                "save_memory: evidence[{i}] quote was not found in the conversation window; copy the snippet verbatim from the messages above (the manifest is not evidence)"
            )));
        }
    }
    Ok(())
}

/// Normalize a quote the same way window bodies are normalized: NFC, then
/// collapse runs of whitespace to single spaces and trim. A quote copied
/// from the conversation matches its source body under this normalization
/// even if line breaks or extra spaces drifted.
fn normalize_quote(quote: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    let nfc: String = quote.nfc().collect();
    nfc.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The normalized text body of a window event the model could quote from.
/// User/assistant/mid-turn inputs contribute their text; tool calls and
/// results contribute their JSON payload (the model sees both in the
/// projection). Other events carry no quotable body.
fn normalized_body(event: &SessionEvent) -> String {
    let raw = match event {
        SessionEvent::UserInput { text }
        | SessionEvent::MidTurnInput { text, .. }
        | SessionEvent::AssistantMessage { text, .. } => text.as_str(),
        SessionEvent::ToolCall { input, .. } => &input.to_string(),
        SessionEvent::ToolResult { output, .. } => &output.to_string(),
        _ => "",
    };
    normalize_quote(raw)
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
#[path = "memory_add_tests.rs"]
mod tests;
