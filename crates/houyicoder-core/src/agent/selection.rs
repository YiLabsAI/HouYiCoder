//! Selection: apply a CheckpointManifest to an event log, producing the
//! filtered event sequence for the assembled context. This is the Select stage's
//! plan-application step (the bridge between Compress's manifest and the
//! assembled context).
//!
//! The event to InputItem grouping lives in turn_group; the CAS retention
//! (materialize/externalize) lives in retention. This module owns only the
//! manifest-application seam.

use houyicoder_context::{ContextBackend, Disposition, EventId, SessionEvent, SessionLogEntry};
use std::collections::HashMap;

/// Apply a CheckpointManifest to an event log, producing the filtered event
/// sequence for the assembled context. This is the Select stage's plan-application
/// step (the bridge between Compress's manifest and the assembled context).
///
/// Disposition handling:
/// - Verbatim: event stays as-is. A tool_result the Isolate stage
///   externalized already carries a block_ref marker; it rides in the group
///   verbatim, and the turn-group assembler materializes it on demand.
/// - Summarized: event is dropped. The manifest's summary text is injected
///   once as a UserInput at the position of the first Summarized
///   event, so the model sees a summary of the folded span, not the raw
///   events (no full re-send of Summarized content).
/// - Referenced: not a Compress disposition (the Isolate stage, PostToolUse,
///   owns the externalization). Unreachable for a manifest build_manifest
///   produced; kept here only to keep the match exhaustive.
///
/// Events not in the manifest's plan (appended after the manifest was built)
/// default to Verbatim — they are newer than the plan covers.
///
/// AssistantTextDelta events are always skipped (subsumed by the
/// authoritative AssistantMessage, same as the turn-group assembler).
///
/// The pair invariant (tool_use and its tool_result share a group, so they
/// share a fate) is structural: the manifest builder groups one API round
/// per TurnGroup, so the pair can never split across dispositions.
pub fn apply_manifest(
    events: &[SessionLogEntry],
    manifest: &houyicoder_context::CheckpointManifest,
    _backend: Option<&dyn ContextBackend>,
) -> Vec<SessionLogEntry> {
    // Flatten the per-turn-group plan into an event id to disposition lookup.
    // The group is the atomic unit; every event in a group shares its
    // disposition, so a flat lookup preserves the integral fate.
    let plan: HashMap<EventId, Disposition> = manifest
        .plan
        .iter()
        .flat_map(|g| g.event_ids.iter().map(|id| (*id, g.disposition)))
        .collect();
    let mut result: Vec<SessionLogEntry> = Vec::with_capacity(events.len());
    let mut summary_injected = false;
    let mut summary_text = manifest.summary.clone();

    for event in events {
        if matches!(event.event, SessionEvent::AssistantTextDelta { .. }) {
            continue;
        }
        let disposition = plan
            .get(&event.id)
            .copied()
            .unwrap_or(Disposition::Verbatim);
        match disposition {
            Disposition::Verbatim => {
                result.push(event.clone());
            }
            Disposition::Summarized => {
                if !summary_injected {
                    summary_injected = true;
                    if let Some(text) = summary_text.take() {
                        result.push(SessionLogEntry {
                            id: EventId::new(),
                            session: event.session,
                            ts: event.ts,
                            prev_hash: None,
                            event: SessionEvent::UserInput { text },
                        });
                    }
                }
            }
            // Referenced is not a Compress disposition. A tool_result the
            // Isolate stage externalized already carries a block_ref marker
            // in the event log; a Verbatim group keeps it as-is, and the
            // turn-group assembler materializes it on demand. This branch
            // is unreachable for a manifest build_manifest produced.
            Disposition::Referenced => {
                result.push(event.clone());
            }
        }
    }
    result
}

#[cfg(test)]
mod tests;
