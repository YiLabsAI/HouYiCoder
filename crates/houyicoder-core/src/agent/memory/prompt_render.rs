//! Prompt-side rendering of active rules, memory indexes, and read-only child
//! memory blocks under explicit token budgets.

use std::cmp::Reverse;
use std::collections::HashMap;

use houyicoder_api::memory::MemoryProvider;
use houyicoder_context::{
    MemoryScope, MemorySummary, MemoryType, memory_index_row, render_memory_index_groups,
    tokens_for,
};

/// Maximum number of memories the system prompt index carries.
pub(crate) const INDEX_ENTRY_CAP: usize = 200;

/// Token budget for the injected memory index.
pub(crate) const INDEX_TOKEN_BUDGET: usize = 4_000;

/// Token budget for the active rules section.
pub(crate) const ACTIVE_RULES_TOKEN_BUDGET: usize = 1_600;

/// Token cap for one compact active rule card.
pub(crate) const RULE_CARD_TOKEN_BUDGET: usize = 80;

pub(crate) fn format_rule_card(key: &str, content: &str) -> String {
    let card = rule_card_text(content);
    format!("- [{key}] {card}\n")
}

fn rule_card_text(body: &str) -> String {
    let first = body
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    let folded: String = first
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let compact = folded.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out = String::new();
    for c in compact.chars() {
        let mut probe = out.clone();
        probe.push(c);
        if tokens_for(&probe) > RULE_CARD_TOKEN_BUDGET {
            break;
        }
        out = probe;
    }
    out.trim_end().to_string()
}

/// One line of the system prompt memory index.
pub(crate) fn index_line(summary: &MemorySummary) -> String {
    format!("{}\n", memory_index_row(&summary.key, &summary.description))
}

/// Render the injected index under one token budget, prioritizing explicit
/// scopes before automatic entries.
pub(crate) fn render_index(summaries: &[MemorySummary]) -> String {
    let mut selected: Vec<&MemorySummary> = Vec::new();
    let mut used = 0usize;
    for scope in [MemoryScope::User, MemoryScope::Project, MemoryScope::Auto] {
        for summary in summaries.iter().filter(|s| s.scope == scope) {
            if selected.len() >= INDEX_ENTRY_CAP {
                break;
            }
            let cost = tokens_for(&index_line(summary));
            if used + cost > INDEX_TOKEN_BUDGET {
                break;
            }
            used += cost;
            selected.push(summary);
        }
    }
    render_memory_index_groups(selected.iter().map(|summary| {
        (
            summary.memory_type,
            summary.key.as_str(),
            summary.description.as_str(),
        )
    }))
}

pub(crate) fn render_active_rules(memory: &dyn MemoryProvider) -> Option<String> {
    let summaries = memory.list_memories();
    let stats: HashMap<String, _> = memory
        .read_recall_stats()
        .into_iter()
        .map(|s| (s.key.clone(), s))
        .collect();

    let mut candidates: Vec<&MemorySummary> = summaries
        .iter()
        .filter(|s| {
            if s.scope == MemoryScope::Auto || s.memory_type != MemoryType::Feedback {
                return false;
            }
            if s.scope == MemoryScope::User {
                return true;
            }
            let stat = stats.get(&s.key);
            let violations = stat.map(|r| r.gate_violations).unwrap_or(0);
            let hits = stat.map(|r| r.recall_hits).unwrap_or(0);
            let occurrences = stat.map(|r| r.occurrences).unwrap_or(1);
            violations >= 1 || hits >= 3 || occurrences >= 2
        })
        .collect();
    if candidates.is_empty() {
        return None;
    }

    candidates.sort_by_key(|s| {
        let stat = stats.get(&s.key);
        let violations = stat.map(|r| r.gate_violations).unwrap_or(0);
        let hits = stat.map(|r| r.recall_hits).unwrap_or(0);
        let occurrences = stat.map(|r| r.occurrences).unwrap_or(1);
        Reverse((violations, hits, occurrences, s.mtime_secs))
    });

    let mut out = String::new();
    let mut used = 0usize;
    for s in candidates {
        let content = memory
            .show_memory_in_scope(&s.key, s.scope)
            .map(|e| e.content)
            .unwrap_or_else(|| s.description.clone());
        let line = format_rule_card(&s.key, &content);
        let cost = tokens_for(&line);
        if used + cost > ACTIVE_RULES_TOKEN_BUDGET {
            break;
        }
        used += cost;
        out.push_str(&line);
    }
    if out.is_empty() { None } else { Some(out) }
}

/// Render the active rules and memory index block for a read-only sub-agent.
pub fn render_child_memory(memory: &dyn MemoryProvider) -> Option<String> {
    let summaries = memory.list_memories();
    if summaries.is_empty() {
        return None;
    }
    let mut out = String::new();
    if let Some(rules) = render_active_rules(memory) {
        out.push_str("# Active Rules\n\n");
        out.push_str(&rules);
    }
    let idx = render_index(&summaries);
    if !idx.is_empty() {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str("## Memory index\n\n");
        out.push_str(&idx);
    }
    if out.is_empty() { None } else { Some(out) }
}
