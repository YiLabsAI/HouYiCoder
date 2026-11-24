//! Local lexical recall after a tool failure or a blind retry.
//!
//! A long task that fails at step fifteen is far from the query it started
//! with, so the opening recall no longer describes the work. This module
//! turns the failure itself into a query: it extracts deterministic features
//! from the failed call and its output, then ranks the stored records with
//! the same field-weighted lexical scan the turn-start recall uses. No model
//! call and no network request happen here.

use std::collections::HashSet;
use std::sync::OnceLock;

use houyicoder_api::memory::MemoryProvider;
use houyicoder_context::{MemoryEntry, SessionEvent, SessionId, tokens_for};
use regex::Regex;
use serde_json::Value;

use crate::agent::RunError;
use crate::agent::append::new_event;
use crate::agent::context;
use crate::agent::memory::{MemoryRuntime, recall};

/// Distinct features a candidate must carry before its lesson is injected.
pub(crate) const MIN_FEATURE_HITS: usize = 2;

/// Entries one failure may inject.
pub(crate) const FAILURE_RECALL_CAP: usize = 2;

/// Token budget one failure may inject.
pub(crate) const FAILURE_RECALL_TOKENS: usize = 600;

/// High-signal features kept per failure. The tail of a long error output is
/// mostly noise, so the first dozen carry the distinction.
const MAX_FEATURES: usize = 12;

/// One failed tool call: the tool, the arguments it ran with, and the output
/// that reported the failure.
#[derive(Debug, Clone)]
pub(crate) struct FailureSignal {
    pub(crate) tool: String,
    pub(crate) input: Value,
    pub(crate) output: Value,
}

/// Words that appear in nearly every tool output or schema and therefore
/// distinguish nothing. Keeping them would let any two failures look alike.
const STOPWORDS: &[&str] = &[
    "error",
    "failed",
    "failure",
    "false",
    "true",
    "null",
    "none",
    "some",
    "this",
    "that",
    "with",
    "from",
    "when",
    "then",
    "than",
    "into",
    "only",
    "must",
    "have",
    "does",
    "were",
    "will",
    "would",
    "should",
    "could",
    "command",
    "output",
    "result",
    "status",
    "message",
    "details",
    "reason",
    "stderr",
    "stdout",
    "traceback",
    "exception",
    "called",
    "cannot",
    "unable",
    "string",
    "file_path",
];

/// Program and tool names that count as tool features, never as the
/// non-tool feature the injection threshold requires.
const TOOL_NAMES: &[&str] = &[
    "bash",
    "sh",
    "zsh",
    "fish",
    "cargo",
    "rustc",
    "git",
    "rm",
    "cp",
    "mv",
    "ls",
    "cat",
    "echo",
    "grep",
    "rg",
    "fd",
    "find",
    "xargs",
    "sed",
    "awk",
    "python",
    "python3",
    "node",
    "npm",
    "make",
    "test",
    "tests",
    "edit",
    "write",
    "read",
    "multiedit",
    "glob",
];

fn error_code() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\bE\d{4}\b").expect("error code pattern"))
}

fn exit_code() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\bexit \d+\b").expect("exit code pattern"))
}

fn error_type() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\b[A-Z][a-zA-Z]+Error\b").expect("error type pattern"))
}

fn identifier() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"[A-Za-z_][A-Za-z0-9_.-]{3,}").expect("identifier pattern"))
}

fn error_phrase() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)operation not permitted|permission denied|not found|refused|truncated")
            .expect("error phrase pattern")
    })
}

/// Recursively collect string values from a JSON Value, skipping object keys
/// so schema argument names (like "path", "command") do not enter the features.
fn collect_json_strings(val: &Value, out: &mut Vec<String>) {
    match val {
        Value::String(s) => out.push(s.clone()),
        Value::Array(arr) => {
            for item in arr {
                collect_json_strings(item, out);
            }
        }
        Value::Object(map) => {
            for (_key, v) in map {
                collect_json_strings(v, out);
            }
        }
        _ => {}
    }
}

/// Collapse path tokens to their file name with extension, dropping directory
/// segments and trailing line or column numbers so directory names do not take
/// feature slots ahead of the file that failed.
fn clean_token(text: &str) -> String {
    text.split_whitespace()
        .map(|raw| {
            let trimmed =
                raw.trim_matches(|c: char| matches!(c, '"' | '\'' | '(' | ')' | ',' | ';'));
            if trimmed.contains('/') || trimmed.contains('\\') {
                let base = trimmed.rsplit(['/', '\\']).next().unwrap_or(trimmed);
                base.split(':').next().unwrap_or(base)
            } else {
                raw
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Extract the deterministic features of one failure, prioritizing error
/// output before input text so tool parameters do not crowd out the failure reason.
pub(crate) fn error_features(signal: &FailureSignal) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    push_feature(&mut out, &signal.tool);
    if let Some(command) = signal.input.get("command").and_then(Value::as_str)
        && let Some(program) = command.split_whitespace().next()
    {
        push_feature(&mut out, program);
    }
    // Scan output first (error cause), then input (target).
    let mut output_strings = Vec::new();
    collect_json_strings(&signal.output, &mut output_strings);
    let output_text = clean_token(&output_strings.join("\n"));

    let mut input_strings = Vec::new();
    collect_json_strings(&signal.input, &mut input_strings);
    let input_text = clean_token(&input_strings.join("\n"));

    let mut phrase_spans: Vec<(usize, usize)> = Vec::new();
    for found in error_phrase().find_iter(&output_text) {
        phrase_spans.push((found.start(), found.end()));
        push_feature(&mut out, found.as_str());
    }
    for pattern in [error_code(), exit_code(), error_type()] {
        for found in pattern.find_iter(&output_text) {
            push_feature(&mut out, found.as_str());
        }
    }
    for found in identifier().find_iter(&output_text) {
        if phrase_spans
            .iter()
            .any(|(s, e)| found.start() >= *s && found.end() <= *e)
        {
            continue;
        }
        let word = found.as_str();
        if word.len() >= 4 && !STOPWORDS.contains(&word.to_ascii_lowercase().as_str()) {
            push_feature(&mut out, word);
        }
    }
    // Append salient tokens from input if room remains.
    if out.len() < MAX_FEATURES {
        for found in identifier().find_iter(&input_text) {
            let word = found.as_str();
            if word.len() >= 4 && !STOPWORDS.contains(&word.to_ascii_lowercase().as_str()) {
                push_feature(&mut out, word);
            }
            if out.len() >= MAX_FEATURES {
                break;
            }
        }
    }
    out.truncate(MAX_FEATURES);
    out
}

/// Add one feature once, case-insensitively. A one-character token is too
/// broad to rank on.
fn push_feature(out: &mut Vec<String>, feature: &str) {
    let feature = feature.trim();
    if feature.chars().count() < 2 {
        return;
    }
    if out.iter().any(|seen| seen.eq_ignore_ascii_case(feature)) {
        return;
    }
    out.push(feature.to_string());
}

/// Whether a feature is a tool or program name rather than a distinguishing
/// detail of the failure.
pub(crate) fn is_tool_name(feature: &str) -> bool {
    TOOL_NAMES.contains(&feature.to_ascii_lowercase().as_str())
}

/// A stable identity for one failure, so the same signature recalls at most
/// once per turn.
pub(crate) fn signature(features: &[String]) -> String {
    let mut sorted: Vec<String> = features.iter().map(|f| f.to_ascii_lowercase()).collect();
    sorted.sort();
    sorted.dedup();
    sorted.join("|")
}

/// Whether a text carries a feature. A short Latin word needs a whole-word
/// match so a three-letter command does not hit a longer word; everything
/// else is a case-insensitive substring.
fn contains_feature(text: &str, feature: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    let feature = feature.to_ascii_lowercase();
    let short_latin =
        feature.chars().count() <= 3 && feature.chars().all(|c| c.is_ascii_alphanumeric());
    if !short_latin {
        return lower.contains(&feature);
    }
    let mut from = 0;
    while let Some(found) = lower[from..].find(&feature) {
        let start = from + found;
        let end = start + feature.len();
        let before_ok = lower[..start]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_alphanumeric());
        let after_ok = lower[end..]
            .chars()
            .next()
            .is_none_or(|c| !c.is_alphanumeric());
        if before_ok && after_ok {
            return true;
        }
        from = start + feature.len().max(1);
    }
    false
}

/// Pick the stored lessons whose text carries at least two distinct failure
/// features, at least one of them not a tool name. The returned entries fit
/// the injection budget.
pub(crate) fn select_failure_entries(
    provider: &dyn MemoryProvider,
    features: &[String],
    injected_keys: &HashSet<String>,
) -> Vec<MemoryEntry> {
    if features.len() < MIN_FEATURE_HITS {
        return Vec::new();
    }
    let query = features.join(" ");
    let mut selected: Vec<MemoryEntry> = Vec::new();
    let mut used = 0usize;
    for hit in provider.rank_candidates(&query, injected_keys) {
        if selected.len() >= FAILURE_RECALL_CAP {
            break;
        }
        // Candidates are sorted score-descending. Once hits reach zero,
        // no further candidates have feature overlap.
        if hit.score == 0 {
            break;
        }
        // Fast-path filter on hit key + description before loading full body.
        let key_desc = format!("{} {}", hit.key, hit.description);
        let has_early_hit = features.iter().any(|f| contains_feature(&key_desc, f));
        if !has_early_hit && hit.score < 2 {
            continue;
        }
        let Some(entry) = provider.show_memory(&hit.key) else {
            continue;
        };
        let haystack = format!("{} {} {}", entry.key, entry.description, entry.content);
        let matched_raw: Vec<&String> = features
            .iter()
            .filter(|feature| contains_feature(&haystack, feature))
            .collect();
        // Discard sub-features if a longer enclosing feature also matched
        // (for example "found" when "not found" matched).
        let matched: Vec<&String> = matched_raw
            .iter()
            .copied()
            .filter(|short| {
                !matched_raw.iter().any(|longer| {
                    longer.len() > short.len()
                        && longer
                            .to_ascii_lowercase()
                            .contains(&short.to_ascii_lowercase())
                })
            })
            .collect();
        if matched.len() < MIN_FEATURE_HITS || matched.iter().all(|feature| is_tool_name(feature)) {
            continue;
        }
        let cost = tokens_for(&entry.content);
        if used + cost > FAILURE_RECALL_TOKENS {
            break;
        }
        used += cost;
        selected.push(entry);
    }
    selected
}

impl MemoryRuntime {
    /// Recall lessons for tool failures in the current turn. The whole path
    /// is local: feature extraction, ranking, and rendering make no model
    /// call. One failure signature recalls at most once per turn.
    pub(crate) async fn recall_after_failures(
        &self,
        session: SessionId,
        signals: &[FailureSignal],
    ) -> Result<(), RunError> {
        if !self.gates.auto_memory_enabled() {
            return Ok(());
        }
        let Some(provider) = self.provider() else {
            return Ok(());
        };
        for signal in signals {
            let features = error_features(signal);
            if features.len() < MIN_FEATURE_HITS || features.iter().all(|f| is_tool_name(f)) {
                continue;
            }
            let signature = signature(&features);
            {
                let mut seen = self.failure_signatures.lock().expect("failure signatures");
                if !seen.insert(signature) {
                    continue;
                }
            }
            let view = self.store.current_view(session).await?;
            let (mut injected_keys, injected_bytes) = recall::injected_key_scan(
                &view.events,
                view.manifest.as_ref(),
                Some(self.store.backend()),
            );
            if injected_bytes >= recall::MAX_SESSION_BYTES {
                break;
            }
            injected_keys.extend(recall::session_written_keys(&view.events));
            let entries = select_failure_entries(provider.as_ref(), &features, &injected_keys);
            if entries.is_empty() {
                continue;
            }
            let text = context::render_recall_text(&entries);
            let keys: Vec<String> = entries.iter().map(|entry| entry.key.clone()).collect();
            let bytes = text.len() as u32;
            provider.record_recall_hits(&keys);
            self.store
                .append(new_event(
                    session,
                    SessionEvent::MemoryRecall { text, keys, bytes },
                ))
                .await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
