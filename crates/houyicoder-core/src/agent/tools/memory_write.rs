//! Structured memory-write tool used by the main agent, extractor, and dream.
//!
//! Callers pass key, description, type, and content fields; the tool routes
//! them through the memory provider under its write lock and path validation.

use std::sync::Arc;

use houyicoder_api::memory::{MemoryProvider, MemoryWriteOutcome};
use houyicoder_async::PFut;
use houyicoder_context::{
    MemoryEntry, MemoryError, MemoryOrigin, MemoryScope, MemoryType, SessionEvent, SessionLogEntry,
};
use serde_json::{Value, json};

use super::{Tool, ToolCtx, ToolError};
use crate::agent::memory::MutationLog;
use houyicoder_api::agent_event::MemoryOperation;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;
use tracing::debug;
use unicode_normalization::UnicodeNormalization;

/// Structured memory-write tool backed by a shared memory provider.
pub struct MemoryWriteTool {
    provider: Arc<dyn MemoryProvider>,
    recorder: Option<Arc<MutationLog>>,
    origin: MemoryOrigin,
    scope: Option<MemoryScope>,
    requires_evidence: bool,
    evidence: Arc<[SessionLogEntry]>,
}

impl MemoryWriteTool {
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

    /// The forked-extraction tool setup in one step: recorder, extractor origin,
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
    /// extraction pass resets before a run and reads after to fire one
    /// AutoMemory notice; the main-agent tool leaves the recorder for the
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

impl Tool for MemoryWriteTool {
    fn name(&self) -> &str {
        "memory_write"
    }
    fn description(&self) -> &str {
        "Write a memory entry that captures context NOT derivable from the \
         current project state (code, git, file structure). Use ONLY for \
         non-obvious facts: user role/preferences, corrective or validating \
         feedback, project goals/decisions/why, or pointers to external \
         systems. Do NOT save code patterns, architecture, file paths, fix \
         recipes, or ephemeral task state — those are derivable or already \
         in the code. Provide a short kebab-case key, a one-line description \
         (specific, naming the entities it relates to), the memory type, and \
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
                "type": {
                    "type": "string",
                    "enum": ["user", "feedback", "project", "reference"],
                    "description": "user = user role/preferences; feedback = corrective or validating guidance on how to work; project = ongoing work/goals/decisions not in git; reference = pointer to an external system."
                },
                "content": {
                    "type": "string",
                    "description": "The memory body. For feedback/project, lead with the rule or fact then add Why and How-to-apply lines."
                },
                "replaces": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional list of superseded memory keys consolidated into this entry. Their counters merge into this key and the old files move to the archive directory."
                }
            },
            "required": ["key", "description", "type", "content"],
            "additionalProperties": false
        });
        if self.requires_evidence {
            // Extraction grounds every save in window evidence: one
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
            let memory_type = parse_type(&input)?;
            let content = parse_string(&input, "content")?;
            if self.requires_evidence {
                validate_extraction_evidence(&self.evidence, &input)?;
            }
            let scope = match self.scope {
                Some(pinned) => refresh_root(provider.as_ref(), &key, pinned),
                None => parse_scope(&input, provider.as_ref(), &key),
            };
            let replaces = parse_replaces(&input);
            let now_secs = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let entry = MemoryEntry::new(key.clone(), content, memory_type)
                .with_meta(description, now_secs)
                .with_origin(self.origin);
            let save = if !replaces.is_empty() {
                provider.replace_in_scope(entry, scope, &replaces)
            } else {
                match scope {
                    MemoryScope::Auto => provider.add_if_changed(entry),
                    other => provider.add_in_scope_if_changed(entry, other),
                }
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

fn parse_string(input: &Value, field: &str) -> Result<String, ToolError> {
    input
        .get(field)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| {
            ToolError::Failed(format!(
                "memory_write: '{field}' must be a non-empty string"
            ))
        })
}

fn parse_type(input: &Value) -> Result<MemoryType, ToolError> {
    let label = input.get("type").and_then(|v| v.as_str()).ok_or_else(|| {
        ToolError::Failed(
            "memory_write: 'type' must be one of user, feedback, project, reference".to_string(),
        )
    })?;
    MemoryType::from_label(label).ok_or_else(|| {
        ToolError::Failed(format!(
            "memory_write: 'type' must be user, feedback, project, or reference; got '{label}'"
        ))
    })
}

/// Parse the optional scope field. Defaults to Auto (the documented default
/// scope — writes land in the auto-extracted root). Accepts user and project
/// so the main agent or the dream can pick a root per call. An unknown value
/// falls back to auto rather than rejecting: the field is advisory and the
/// call's intent was to save, so a typo must not starve the write.
fn parse_scope(input: &Value, provider: &dyn MemoryProvider, key: &str) -> MemoryScope {
    if let Some(label) = input.get("scope").and_then(|v| v.as_str()) {
        return MemoryScope::from_label(label).unwrap_or(MemoryScope::Auto);
    }
    let existing = provider.scopes_for_key(key);
    if existing.contains(&MemoryScope::User) {
        MemoryScope::User
    } else if existing.contains(&MemoryScope::Project) {
        MemoryScope::Project
    } else {
        MemoryScope::Auto
    }
}

fn parse_replaces(input: &Value) -> Vec<String> {
    input
        .get("replaces")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::trim))
                .filter(|s| !s.is_empty())
                .map(ToString::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Resolve the write root for a pinned save. The pin names the default
/// root; the exception is a key already living in the user or project root,
/// where the save refreshes that root in place. Writing the refresh into
/// the pinned root would create a competing copy that shadows the explicit
/// entry by newest-mtime and the refresh would be lost on the next
/// reconcile.
fn refresh_root(provider: &dyn MemoryProvider, key: &str, pinned: MemoryScope) -> MemoryScope {
    let existing = provider.scopes_for_key(key);
    if existing.contains(&MemoryScope::User) && pinned != MemoryScope::User {
        MemoryScope::User
    } else if existing.contains(&MemoryScope::Project) && pinned != MemoryScope::Project {
        MemoryScope::Project
    } else {
        pinned
    }
}

fn reject_evidence(msg: String) -> ToolError {
    debug!(target: "memory_write", "extraction evidence rejected: {msg}");
    ToolError::Failed(msg)
}

fn validate_extraction_evidence(
    window: &[SessionLogEntry],
    input: &Value,
) -> Result<(), ToolError> {
    let evidence = input
        .get("evidence")
        .and_then(|v| v.as_array())
        .ok_or_else(|| {
            reject_evidence(
                "memory_write: 'evidence' must be an array of 1..3 quotes copied from the conversation window"
                    .to_string(),
            )
        })?;
    if evidence.is_empty() {
        return Err(reject_evidence(
            "memory_write: 'evidence' must contain at least one quote from the conversation window"
                .to_string(),
        ));
    }
    if evidence.len() > 3 {
        return Err(reject_evidence(
            "memory_write: 'evidence' may contain at most three quotes".to_string(),
        ));
    }
    for (i, item) in evidence.iter().enumerate() {
        let quote = item.get("quote").and_then(|v| v.as_str()).ok_or_else(|| {
            reject_evidence(format!(
                "memory_write: evidence[{i}] must have a non-empty 'quote' copied from the conversation window"
            ))
        })?;
        let needle = normalize_quote(quote);
        if needle.is_empty() {
            return Err(reject_evidence(format!(
                "memory_write: evidence[{i}] quote is empty after normalization"
            )));
        }
        if !window
            .iter()
            .any(|e| normalized_body(&e.event).contains(&needle))
        {
            return Err(reject_evidence(format!(
                "memory_write: evidence[{i}] quote was not found in the conversation window; copy the snippet verbatim from the messages above (the manifest is not evidence)"
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
    let nfc: String = quote.nfc().collect();
    nfc.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The normalized text body of a window event the model could quote from.
/// User/assistant/mid-turn inputs contribute their text; tool calls and
/// results contribute their JSON payload (the model sees both in the
/// assembly). Other events carry no quotable body.
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

fn map_memory_error(e: MemoryError) -> ToolError {
    match e {
        MemoryError::InvalidPath(msg) => {
            ToolError::Failed(format!("memory_write: invalid key/path: {msg}"))
        }
        MemoryError::Corrupt(msg) => {
            ToolError::Failed(format!("memory_write: corrupt store: {msg}"))
        }
        MemoryError::Io => ToolError::Failed("memory_write: storage I/O failure".to_string()),
        MemoryError::Secret(rule) => ToolError::Failed(format!(
            "memory_write: the entry looks like it carries a credential ({rule}); \
             store a pointer to the secret instead of the value"
        )),
        MemoryError::NotFound => ToolError::Failed("memory_write: entry not found".to_string()),
    }
}

#[cfg(test)]
mod tests;
