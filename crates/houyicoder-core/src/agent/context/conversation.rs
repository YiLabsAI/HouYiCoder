//! What the assembled conversation costs the session holding it: the per-tool
//! footprints, and what the session's delegated children returned and spent.
//!
//! Both are read off the whole selected history with a token count per tool
//! call input and per tool result. That history grows with the session, so the
//! measurement the /context pane reads derives them and a turn, which reports
//! only what the request pays for, does not.

use std::collections::HashMap;

use houyicoder_context::{SessionEvent, SessionLogEntry};
use houyicoder_protocol::frontend::context::{DelegatedContext, ToolFootprint};

use super::ContextBuilder;
use crate::agent::multi_agent::usage::aggregate_subagent_usage;

/// Whether an assembly derives the conversation cost. Deriving it walks the
/// whole selected history with a token count per tool call input and per tool
/// result, so only the measurement the /context pane reads pays for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ConversationCost {
    /// The sections alone: what the request pays for.
    SectionsOnly,
    /// The sections plus the per-tool footprints and the delegated spend.
    WithFootprints,
}

impl ContextBuilder {
    /// The conversation cost rows an assembly carries, per the cost it was
    /// asked for. Empty rows and no delegated spend for a measurement that does
    /// not derive them.
    pub(super) fn conversation_cost(
        &self,
        events: &[SessionLogEntry],
        cost: ConversationCost,
    ) -> (Vec<ToolFootprint>, Option<DelegatedContext>) {
        match cost {
            ConversationCost::SectionsOnly => (Vec::new(), None),
            ConversationCost::WithFootprints => {
                (self.tool_footprints(events), self.delegated_context(events))
            }
        }
    }

    /// Per-tool conversation footprints over the selected events: calls counted
    /// off the call blocks, result tokens joined through the call id a result
    /// answers. A result whose call the selection dropped carries no tool name,
    /// so it is counted under no tool rather than guessed. Heaviest total cost
    /// first, name breaking a tie, so the order is stable across runs.
    pub(super) fn tool_footprints(&self, events: &[SessionLogEntry]) -> Vec<ToolFootprint> {
        let call_tools: HashMap<&str, &str> = events
            .iter()
            .filter_map(|entry| match &entry.event {
                SessionEvent::ToolCall { call_id, tool, .. } => {
                    Some((call_id.as_str(), tool.as_str()))
                }
                _ => None,
            })
            .collect();
        let mut rows: HashMap<&str, ToolFootprint> = HashMap::new();
        for entry in events {
            match &entry.event {
                SessionEvent::ToolCall { tool, input, .. } => {
                    let row = rows
                        .entry(tool.as_str())
                        .or_insert_with(|| empty_footprint(tool));
                    row.calls = row.calls.saturating_add(1);
                    row.call_tokens = row
                        .call_tokens
                        .saturating_add(self.tokenizer.count(&input.to_string()));
                }
                SessionEvent::ToolResult {
                    call_id, output, ..
                } => {
                    if let Some(tool) = call_tools.get(call_id.as_str()) {
                        let row = rows.entry(tool).or_insert_with(|| empty_footprint(tool));
                        row.result_tokens = row
                            .result_tokens
                            .saturating_add(self.tokenizer.count(&output.to_string()));
                    }
                }
                _ => {}
            }
        }
        let mut out: Vec<ToolFootprint> = rows.into_values().collect();
        out.sort_by(|a, b| {
            let total = |f: &ToolFootprint| f.call_tokens.saturating_add(f.result_tokens);
            total(b).cmp(&total(a)).then_with(|| a.tool.cmp(&b.tool))
        });
        out
    }

    /// What the session's children cost this context: the tokens their returned
    /// summaries occupy here, and the totals they processed in their own
    /// windows. The child total stays unknown unless every return reported an
    /// input count, because the total is that count plus the output.
    pub(super) fn delegated_context(&self, events: &[SessionLogEntry]) -> Option<DelegatedContext> {
        let usage = aggregate_subagent_usage(events);
        if usage.calls == 0 {
            return None;
        }
        let returned_tokens = events
            .iter()
            .filter_map(|entry| match &entry.event {
                SessionEvent::SubagentReturn { summary, .. } => Some(self.tokenizer.count(summary)),
                _ => None,
            })
            .sum();
        let child_tokens = (usage.unmeasured_calls == 0 && usage.provenance.input_reported)
            .then(|| usage.to_usage().total_tokens);
        Some(DelegatedContext {
            returned_tokens,
            child_tokens,
            returns: usage.calls as u32,
        })
    }
}

/// A zeroed footprint row, so the call pass and the result pass both extend
/// one shape.
fn empty_footprint(tool: &str) -> ToolFootprint {
    ToolFootprint {
        tool: tool.to_string(),
        calls: 0,
        call_tokens: 0,
        result_tokens: 0,
    }
}
