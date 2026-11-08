//! Externalize a large tool output to the block store.
//!
//! A result past the isolation threshold becomes a small marker carrying an
//! inline preview, with the raw bytes kept in the block store for on-demand
//! retrieval. A structured result externalizes only its largest string field
//! so the envelope's other keys stay inline; a blob falls back to replacing
//! the whole output. Any block-store failure keeps the raw output, so the
//! isolation step never loses content.

use std::collections::HashSet;

use houyicoder_context::ContextBackend;
use houyicoder_protocol::block_ref::{
    BLOCK_BYTES_KEY, BLOCK_REF_KEY, CONTAINS_KEY, HINT_KEY, PREVIEW_KEY, block_addresses,
};
use serde_json::Value;

use crate::agent::Runner;
use crate::agent::reducer::{ReduceCtx, TrustLevel};

/// Tool outputs larger than this (serialized bytes) are externalized to
/// the CAS at the PostToolUse point. The 32KB threshold sits above the bash
/// output bound (a 4KB head plus an 8KB tail, or a verbatim result under the
/// 30KB cap), so a result the tool already bounded is not externalized a
/// second time into a 200-char preview that hides the tail the model and the
/// user need. Genuinely large results (a raw capture the tool did not bound)
/// still externalize.
pub(crate) const ISOLATE_LARGE_OUTPUT_BYTES: usize = 32_768;

/// The most distinct block addresses one externalized output may restate.
///
/// A whole-output externalize replaces the object that named those blocks, so
/// its marker has to name each of them again for the reclaim pass. Past this
/// count the restatement costs the event more than the externalization saves,
/// and the output stays inline instead. That fallback is bounded: the tool has
/// already bounded its own result before this runs.
const MAX_RESTATED_ADDRESSES: usize = 64;

/// Inline preview length, in chars, stored alongside a block_ref marker so
/// the Summarize tier can serve a preview without a retrieval. Matches the
/// retention layer's preview length.
const PREVIEW_CHARS: usize = 200;

impl Runner {
    /// Externalize a large tool output to the CAS at the PostToolUse point.
    /// A structured result (a JSON object) externalizes only its largest
    /// top-level string field, so the envelope's other keys (agentId, color,
    /// status, usage) survive inline — the whole-output replacement would
    /// destroy them, and the TUI reads those keys to render the Subagent
    /// fold-group. Blob outputs (no top-level string field, e.g. a grep
    /// matches array) and objects whose largest string field cannot buy
    /// enough headroom fall back to whole-output externalize. Smaller
    /// outputs and any block_put failure pass through unchanged (fail-closed).
    pub(super) async fn isolate_large_output(
        &self,
        tool: &str,
        output: serde_json::Value,
    ) -> serde_json::Value {
        let bytes = match serde_json::to_vec(&output) {
            Ok(b) => b,
            Err(_) => return output,
        };
        if bytes.len() <= ISOLATE_LARGE_OUTPUT_BYTES {
            return output;
        }
        let backend = self.store.backend();
        // Field-level path: externalize only the largest top-level string
        // field so the envelope's other keys stay inline. The candidate is
        // sized with an upper-bound marker (see marker_upper_bound for the
        // assumption on the reducer).
        if let Some(obj) = output.as_object()
            && let Some((key, field_len)) = largest_string_field(obj)
        {
            let marker_bound = serde_json::to_vec(&marker_upper_bound())
                .map(|b| b.len())
                .unwrap_or(0);
            // The field is no bigger than the marker that would replace
            // it: externalizing buys nothing. Blob outputs (large array,
            // small string fields) land here and keep the whole-output
            // path that grep relies on.
            if field_len > marker_bound {
                let mut candidate = output.clone();
                candidate[key.clone()] = marker_upper_bound();
                let fits = serde_json::to_vec(&candidate)
                    .map(|b| b.len() <= ISOLATE_LARGE_OUTPUT_BYTES)
                    .unwrap_or(false);
                if fits {
                    return self
                        .externalize_field(tool, output, key, Some(backend))
                        .await;
                }
            }
        }
        self.externalize_whole(tool, output, bytes, Some(backend))
            .await
    }

    /// Externalize a single string field of a structured result: block_put
    /// the field's value, replace the field with a marker carrying an inline
    /// preview, keep every other top-level key. Fail-closed: any block_put
    /// or serialization failure returns the original output so no content
    /// is lost.
    async fn externalize_field(
        &self,
        tool: &str,
        output: serde_json::Value,
        key: String,
        backend: Option<&dyn ContextBackend>,
    ) -> serde_json::Value {
        let Some(backend) = backend else {
            return output;
        };
        let field_value = output.get(&key).cloned().unwrap_or(Value::Null);
        let field_bytes = match serde_json::to_vec(&field_value) {
            Ok(b) => b,
            Err(_) => return output,
        };
        let hash = match backend.block_put(field_bytes.clone()).await {
            Ok(h) => h,
            Err(_) => return output,
        };
        // Preview from the raw string content, not the JSON-serialized form,
        // so the fold-group summary shows clean text (real newlines, no
        // escape sequences) instead of "{\"...\\n...\"}".
        let preview_src = field_value
            .as_str()
            .map(|s| s.as_bytes())
            .unwrap_or(&field_bytes);
        let mut out = output;
        out[key] = self.marker_for(tool, &hash.0, preview_src);
        out
    }

    /// Replace the whole output with one marker.
    ///
    /// The stored output may itself name blocks (a tool that published the
    /// raw it dropped). The marker replaces the object that named them, so
    /// they are restated beside it under contains: the reclaim pass walks
    /// markers and never opens stored bytes, and without the restatement it
    /// would release blocks the log still needs. Each address is restated
    /// once, and an output naming more distinct blocks than the restatement
    /// allows keeps its references inline.
    async fn externalize_whole(
        &self,
        tool: &str,
        output: serde_json::Value,
        bytes: Vec<u8>,
        backend: Option<&dyn ContextBackend>,
    ) -> serde_json::Value {
        let Some(backend) = backend else {
            return output;
        };
        // One restated entry per address, in document order: an output that
        // names the same block six hundred times must not cost the event six
        // hundred copies of its address.
        let contained: Vec<String> = {
            let mut seen = HashSet::new();
            block_addresses(&output)
                .into_iter()
                .filter(|hash| seen.insert(*hash))
                .map(str::to_string)
                .collect()
        };
        if contained.len() > MAX_RESTATED_ADDRESSES {
            // The restatement has to name every address it restates: the
            // reclaim pass can only follow what it sees. Past this many, the
            // output stays inline, where the walk reaches its own references
            // without a copy.
            return output;
        }
        let hash = match backend.block_put(bytes.clone()).await {
            Ok(h) => h,
            Err(_) => return output,
        };
        let mut marker = self.marker_for(tool, &hash.0, &bytes);
        if !contained.is_empty() {
            marker[CONTAINS_KEY] = serde_json::json!(
                contained
                    .into_iter()
                    .map(|hash| serde_json::json!({BLOCK_REF_KEY: hash}))
                    .collect::<Vec<_>>()
            );
        }
        marker
    }

    /// Build the block_ref marker (preview + hint + data_tag) from the
    /// serialized bytes that were stored. Shared by field-level and
    /// whole-output externalize so the marker vocabulary has one producer.
    fn marker_for(&self, tool: &str, hash: &str, bytes: &[u8]) -> Value {
        let raw_preview = preview_string(bytes);
        let (preview, data_tag) = match &self.reducer {
            Some(r) => {
                let reduced = r.reduce(
                    &raw_preview,
                    tool,
                    &ReduceCtx {
                        raw: false,
                        trust: TrustLevel::Untrusted,
                    },
                );
                (reduced.text, reduced.data_tag)
            }
            None => (raw_preview, false),
        };
        serde_json::json!({
            BLOCK_REF_KEY: hash,
            PREVIEW_KEY: preview,
            "data_tag": data_tag,
            BLOCK_BYTES_KEY: bytes.len(),
            HINT_KEY: "large output compacted; search the conversation to read it",
        })
    }
}

/// Build the inline preview string for a serialized output: the first
/// PREVIEW_CHARS chars (UTF-8 lossy) with an ellipsis when truncated.
fn preview_string(bytes: &[u8]) -> String {
    let s = String::from_utf8_lossy(bytes);
    if s.chars().count() > PREVIEW_CHARS {
        let truncated: String = s.chars().take(PREVIEW_CHARS).collect();
        format!("{truncated}\u{2026}")
    } else {
        s.into_owned()
    }
}

/// The largest top-level string field of a JSON object, by escaped byte
/// length. Returns None when the object has no string field (e.g. a grep
/// result whose payload is a matches array). Used to pick which field to
/// externalize so the envelope's other keys stay inline.
fn largest_string_field(
    obj: &serde_json::Map<String, serde_json::Value>,
) -> Option<(String, usize)> {
    obj.iter()
        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), json_string_byte_len(s))))
        .max_by_key(|(_, n)| *n)
}

/// Escaped byte length a JSON encoder writes for this string, including the
/// surrounding quotes. Matches serde_json's escaping: " and \ cost 2 bytes,
/// control chars (< 0x20) cost 6 (\uXXXX), everything else its UTF-8 byte
/// count. Used so the field-level decision accounts for escaping, which the
/// whole-output gate measures in the same serialized bytes.
fn json_string_byte_len(s: &str) -> usize {
    2 + s
        .chars()
        .map(|c| match c {
            '"' | '\\' => 2,
            c if (c as u32) < 0x20 => 6,
            _ => c.len_utf8(),
        })
        .sum::<usize>()
}

/// A strict upper bound on the marker object that replaces an externalized
/// field: hash + preview (capped at PREVIEW_CHARS, multi-byte safe via the
/// x4 factor) + hint + data_tag. Used to size-check a candidate envelope
/// before committing to field-level. Assumes the reducer does not expand
/// the preview past this bound; a reducer that annotates or prefixes the
/// preview can exceed it, leaving the inline output over the threshold. The
/// envelope still holds either way (only the inline size is off, and the
/// result is not lost — the raw is in the CAS).
fn marker_upper_bound() -> serde_json::Value {
    serde_json::json!({
        BLOCK_REF_KEY: "x".repeat(128),
        "preview": "x".repeat(PREVIEW_CHARS * 4 + 4),
        "data_tag": false,
        "hint": "x".repeat(120),
    })
}
