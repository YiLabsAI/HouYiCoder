//! Selects memory candidates before conversation content leaves active context.
//!
//! Compaction scans Summarized events; /clear scans the full conversation.
//! Both paths share deterministic signal detection and stable key derivation.

use std::collections::HashMap;

use houyicoder_context::{
    CheckpointManifest, Disposition, EventId, MemoryEntry, MemorySource, SessionEvent,
    SessionLogEntry,
};

/// A preservation signal hit: the reason it matters plus the matching phrase.
struct PreservationSignal {
    reason: PreservationReason,
    phrase: &'static str,
}

#[derive(Clone, Copy)]
enum PreservationReason {
    UnsolvedProblem,
    KeyDecision,
}

/// Select candidates from events the manifest marks Summarized.
pub(crate) fn preserve_folded_context(
    events: &[SessionLogEntry],
    manifest: &CheckpointManifest,
) -> Vec<MemoryEntry> {
    let plan: HashMap<EventId, Disposition> = manifest
        .plan
        .iter()
        .flat_map(|g| g.event_ids.iter().map(|id| (*id, g.disposition)))
        .collect();
    let mut out = Vec::new();
    for ev in events {
        if !matches!(plan.get(&ev.id), Some(Disposition::Summarized)) {
            continue;
        }
        let text = match &ev.event {
            SessionEvent::AssistantMessage { text, .. } => text.as_str(),
            SessionEvent::UserInput { text } => text.as_str(),
            _ => continue,
        };
        for signal in detect_preservation_signals(text) {
            let key = derive_memory_key(signal.reason, text);
            out.push(
                MemoryEntry::new(key, text.to_string(), MemorySource::Feedback)
                    .with_meta(signal.phrase.to_string(), ev.ts),
            );
        }
    }
    out
}

/// Select candidates from all user and assistant messages before /clear.
pub(crate) fn preserve_session(events: &[SessionLogEntry]) -> Vec<MemoryEntry> {
    let mut out = Vec::new();
    for ev in events {
        let text = match &ev.event {
            SessionEvent::AssistantMessage { text, .. } => text.as_str(),
            SessionEvent::UserInput { text } => text.as_str(),
            _ => continue,
        };
        for signal in detect_preservation_signals(text) {
            let key = derive_memory_key(signal.reason, text);
            out.push(
                MemoryEntry::new(key, text.to_string(), MemorySource::Feedback)
                    .with_meta(signal.phrase.to_string(), ev.ts),
            );
        }
    }
    out
}

/// Detect at most one unsolved-problem and one key-decision signal per text.
/// Matching is case-insensitive and deliberately omits ambiguous short words.
fn detect_preservation_signals(text: &str) -> Vec<PreservationSignal> {
    let lower = text.to_ascii_lowercase();
    let mut out = Vec::new();
    let mut unsolved = false;
    for phrase in UNSOLVED_PHRASES {
        if !unsolved && lower.contains(phrase) {
            out.push(PreservationSignal {
                reason: PreservationReason::UnsolvedProblem,
                phrase,
            });
            unsolved = true;
        }
    }
    if !unsolved && lower.lines().any(|l| l.trim_end().ends_with('?')) {
        out.push(PreservationSignal {
            reason: PreservationReason::UnsolvedProblem,
            phrase: "?",
        });
    }
    let mut decision = false;
    for phrase in DECISION_PHRASES {
        if !decision && lower.contains(phrase) {
            out.push(PreservationSignal {
                reason: PreservationReason::KeyDecision,
                phrase,
            });
            decision = true;
        }
    }
    out
}

const UNSOLVED_PHRASES: &[&str] = &[
    "error",
    "panic",
    "traceback",
    "todo",
    "fixme",
    "not sure",
    "broken",
];

const DECISION_PHRASES: &[&str] = &[
    "chose",
    "decided",
    "go with",
    "keep doing",
    "exactly",
    "perfect",
];

/// Derive a stable, filesystem-safe key from the reason and source text.
fn derive_memory_key(reason: PreservationReason, text: &str) -> String {
    let prefix = match reason {
        PreservationReason::UnsolvedProblem => "compact-unsolved",
        PreservationReason::KeyDecision => "compact-decision",
    };
    let slug: String = text
        .to_ascii_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(32)
        .collect();
    format!("{prefix}-{slug}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use houyicoder_context::{CheckpointId, SessionId, TurnGroup};

    fn ev(session: SessionId, id: EventId, kind: SessionEvent) -> SessionLogEntry {
        SessionLogEntry {
            id,
            session,
            ts: 0,
            prev_hash: None,
            event: kind,
        }
    }

    #[test]
    fn test_preserve_folded_finds_unsolved() {
        let s = SessionId::new();
        let id1 = EventId::new();
        let id2 = EventId::new();
        let id3 = EventId::new();
        let events = vec![
            ev(
                s,
                id1,
                SessionEvent::AssistantMessage {
                    text: "hit an error here".into(),
                    thinking: None,
                },
            ),
            ev(
                s,
                id2,
                SessionEvent::AssistantMessage {
                    text: "we decided to use rust".into(),
                    thinking: None,
                },
            ),
            ev(
                s,
                id3,
                SessionEvent::AssistantMessage {
                    text: "latest".into(),
                    thinking: None,
                },
            ),
        ];
        let manifest = CheckpointManifest {
            id: CheckpointId::new(),
            session: s,
            last_event: id3,
            summary: None,
            plan: vec![
                TurnGroup {
                    turn_id: id1,
                    disposition: Disposition::Summarized,
                    event_ids: vec![id1, id2],
                },
                TurnGroup {
                    turn_id: id3,
                    disposition: Disposition::Verbatim,
                    event_ids: vec![id3],
                },
            ],
            ts: 0,
        };
        let candidates = preserve_folded_context(&events, &manifest);
        assert!(
            candidates.len() >= 2,
            "both signals found: {:?}",
            candidates.iter().map(|m| &m.key).collect::<Vec<_>>()
        );
        assert!(
            candidates
                .iter()
                .any(|m| m.key.starts_with("compact-unsolved")),
            "unsolved signal present"
        );
        assert!(
            candidates
                .iter()
                .any(|m| m.key.starts_with("compact-decision")),
            "decision signal present"
        );
        assert!(
            candidates.iter().all(|m| m.content != "latest"),
            "verbatim event not scanned"
        );
    }

    #[test]
    fn test_preserve_session_scans_all() {
        let s = SessionId::new();
        let id1 = EventId::new();
        let id2 = EventId::new();
        let id3 = EventId::new();
        let events = vec![
            ev(
                s,
                id1,
                SessionEvent::AssistantMessage {
                    text: "hit an error here".into(),
                    thinking: None,
                },
            ),
            ev(
                s,
                id2,
                SessionEvent::AssistantMessage {
                    text: "we decided to use rust".into(),
                    thinking: None,
                },
            ),
            ev(
                s,
                id3,
                SessionEvent::UserInput {
                    text: "latest".into(),
                },
            ),
        ];
        let candidates = preserve_session(&events);
        assert!(
            candidates.len() >= 2,
            "both signals found across all events: {:?}",
            candidates.iter().map(|m| &m.key).collect::<Vec<_>>()
        );
        assert!(
            candidates
                .iter()
                .any(|m| m.key.starts_with("compact-unsolved")),
            "unsolved signal present"
        );
        assert!(
            candidates
                .iter()
                .any(|m| m.key.starts_with("compact-decision")),
            "decision signal present"
        );
        assert!(
            candidates.iter().all(|m| m.content != "latest"),
            "non-signal text not extracted"
        );
    }

    #[test]
    fn test_preservation_dedup_stable() {
        let s = SessionId::new();
        let id1 = EventId::new();
        let id2 = EventId::new();
        let text = "the build is broken again";
        let events = vec![
            ev(
                s,
                id1,
                SessionEvent::AssistantMessage {
                    text: text.into(),
                    thinking: None,
                },
            ),
            ev(
                s,
                id2,
                SessionEvent::AssistantMessage {
                    text: text.into(),
                    thinking: None,
                },
            ),
        ];
        let manifest = CheckpointManifest {
            id: CheckpointId::new(),
            session: s,
            last_event: id2,
            summary: None,
            plan: vec![TurnGroup {
                turn_id: id1,
                disposition: Disposition::Summarized,
                event_ids: vec![id1, id2],
            }],
            ts: 0,
        };
        let candidates = preserve_folded_context(&events, &manifest);
        let keys: Vec<&str> = candidates.iter().map(|m| m.key.as_str()).collect();
        assert_eq!(keys[0], keys[1], "same content yields same key for dedup");
    }

    #[test]
    fn test_detect_signals_dedups_reason() {
        let signals = detect_preservation_signals("hit an error and it's broken");
        let unsolved = signals
            .iter()
            .filter(|s| matches!(s.reason, PreservationReason::UnsolvedProblem))
            .count();
        assert_eq!(unsolved, 1, "one unsolved signal per text, not per phrase");
        let signals = detect_preservation_signals("hit an error, decided to retry");
        assert_eq!(signals.len(), 2, "one unsolved + one decision");
    }
}
