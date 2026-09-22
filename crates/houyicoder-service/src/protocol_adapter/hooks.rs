//! Hook entries for the /hooks pane: registered hooks plus the framework's
//! declared event catalog.

use houyicoder_core::agent::{HookEntry as CoreHookEntry, HookEvent};
use houyicoder_protocol::frontend::hooks::HookEntry;

/// Convert registered hooks into the entries the /hooks pane renders. A hook
/// is marked fired when any of its events has a live dispatch point.
pub(crate) fn hook_entries(entries: Vec<CoreHookEntry>) -> Vec<HookEntry> {
    entries
        .into_iter()
        .map(|h| HookEntry {
            name: h.name,
            events: h.events.iter().map(|e| format!("{e:?}")).collect(),
            source: format!("{:?}", h.source),
            fired: h.events.iter().any(|e| e.is_fired()),
            summary: String::new(),
            description: String::new(),
        })
        .collect()
}

/// The framework's declared hook-event catalog, so /hooks shows what the hook
/// system supports even when no external hook is configured.
pub(crate) fn declared_events() -> Vec<HookEntry> {
    HookEvent::ALL
        .iter()
        .copied()
        .map(|e| HookEntry {
            name: e.label().to_string(),
            events: vec![e.label().to_string()],
            source: "framework".to_string(),
            fired: e.is_fired(),
            summary: e.summary().to_string(),
            description: e.description().to_string(),
        })
        .collect()
}

#[cfg(test)]
#[path = "hooks_tests.rs"]
mod tests;
