//! Projection: apply a CheckpointManifest to an event log, producing the
//! filtered event sequence for projection. This is the Select stage's
//! plan-application step (the bridge between Compress's manifest and the
//! served view).
//!
//! The event to InputItem grouping lives in turn_group; the CAS retention
//! (materialize/externalize) lives in retention. This module owns only the
//! manifest-application seam.

use houyicoder_context::{ContextBackend, Disposition, EventId, TurnEvent, TurnEventKind};

/// Apply a CheckpointManifest to an event log, producing the filtered event
/// sequence for projection. This is the Select stage's plan-application step
/// (the bridge between Compress's manifest and the served view).
///
/// Disposition handling:
/// - Verbatim: event stays as-is. A tool_result the Isolate stage
///   externalized already carries a block_ref marker; it rides in the group
///   verbatim, and the turn-group projection materializes it on demand.
/// - Summarized: event is dropped. The manifest's summary text is injected
///   once as a synthetic UserInput at the position of the first Summarized
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
/// authoritative AssistantMessage, same as the turn-group projection).
///
/// The pair invariant (tool_use and its tool_result share a group, so they
/// share a fate) is structural: the manifest builder groups one API round
/// per TurnGroup, so the pair can never split across dispositions.
pub fn apply_manifest(
    events: &[TurnEvent],
    manifest: &houyicoder_context::CheckpointManifest,
    _backend: Option<&dyn ContextBackend>,
) -> Vec<TurnEvent> {
    use std::collections::HashMap;

    // Flatten the per-turn-group plan into an event id to disposition lookup.
    // The group is the atomic unit; every event in a group shares its
    // disposition, so a flat lookup preserves the integral fate.
    let plan: HashMap<EventId, Disposition> = manifest
        .plan
        .iter()
        .flat_map(|g| g.event_ids.iter().map(|id| (*id, g.disposition)))
        .collect();
    let mut result: Vec<TurnEvent> = Vec::with_capacity(events.len());
    let mut summary_injected = false;
    let mut summary_text = manifest.summary.clone();

    for event in events {
        if matches!(event.kind, TurnEventKind::AssistantTextDelta { .. }) {
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
                        result.push(TurnEvent {
                            id: EventId::new(),
                            session: event.session,
                            ts: event.ts,
                            prev_hash: None,
                            kind: TurnEventKind::UserInput { text },
                        });
                    }
                }
            }
            // Referenced is not a Compress disposition. A tool_result the
            // Isolate stage externalized already carries a block_ref marker
            // in the event log; a Verbatim group keeps it as-is, and the
            // turn-group projection materializes it on demand. This branch
            // is unreachable for a manifest build_manifest produced.
            Disposition::Referenced => {
                result.push(event.clone());
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::super::manifest::{CompressPolicy, HeuristicSummarizer, build_manifest};
    use super::super::turn_group::project_input_items;
    use super::*;
    use houyicoder_context::{
        BlockHash, CheckpointId, ContextError, EventId, SessionId, TurnEvent, TurnEventKind,
    };
    use houyicoder_protocol::llm::InputItem;

    fn ev(id: EventId, session: SessionId, kind: TurnEventKind) -> TurnEvent {
        TurnEvent {
            id,
            session,
            ts: 0,
            prev_hash: None,
            kind,
        }
    }

    fn user(text: &str) -> TurnEventKind {
        TurnEventKind::UserInput { text: text.into() }
    }

    fn assistant(text: &str) -> TurnEventKind {
        TurnEventKind::AssistantMessage {
            text: text.into(),
            thinking: None,
        }
    }

    fn call(cid: &str, tool: &str) -> TurnEventKind {
        TurnEventKind::ToolCall {
            call_id: cid.into(),
            tool: tool.into(),
            input: serde_json::json!({}),
        }
    }

    fn result(cid: &str, output: serde_json::Value) -> TurnEventKind {
        TurnEventKind::tool_result(cid, output)
    }
    fn ids(n: usize) -> Vec<EventId> {
        (0..n).map(|_| EventId::new()).collect()
    }

    /// Verify every ToolResult in the slice has a matching ToolCall in the
    /// same slice (the pair invariant the projection debug_assert guards).
    fn pairs_intact(events: &[TurnEvent]) -> bool {
        let calls: std::collections::HashSet<&str> = events
            .iter()
            .filter_map(|e| match &e.kind {
                TurnEventKind::ToolCall { call_id, .. } => Some(call_id.as_str()),
                _ => None,
            })
            .collect();
        events.iter().all(|e| match &e.kind {
            TurnEventKind::ToolResult { call_id, .. } => calls.contains(call_id.as_str()),
            _ => true,
        })
    }

    #[tokio::test]
    async fn test_manifest_verbatim_keeps() {
        // All-Verbatim manifest: events pass through unchanged (deltas still
        // stripped). No summary injected.
        let s = SessionId::new();
        let ids = ids(3);
        let events = vec![
            ev(ids[0], s, user("task")),
            ev(ids[1], s, assistant("a1")),
            ev(ids[2], s, assistant("a2")),
        ];
        let policy = CompressPolicy {
            tail_turns: 4,
            preserve_recent_tokens: 0,
            large_output_bytes: 0,
        };
        let manifest = build_manifest(&events, &policy, &HeuristicSummarizer, None).await;
        let result = apply_manifest(&events, &manifest, None);
        assert_eq!(result.len(), 3);
        assert_eq!(result[0].id, ids[0]);
        assert_eq!(result[1].id, ids[1]);
        assert_eq!(result[2].id, ids[2]);
    }

    #[tokio::test]
    async fn test_manifest_summarized_injects() {
        // tail_turns=1 keeps only the last assistant turn verbatim; older
        // events are Summarized. A summary UserInput is injected at the
        // position of the first Summarized event; the raw events are dropped.
        let s = SessionId::new();
        let ids = ids(4);
        let events = vec![
            ev(ids[0], s, user("do work")),
            ev(ids[1], s, assistant("old response")),
            ev(ids[2], s, assistant("middle")),
            ev(ids[3], s, assistant("latest")),
        ];
        let policy = CompressPolicy {
            tail_turns: 1,
            preserve_recent_tokens: 0,
            large_output_bytes: 0,
        };
        let manifest = build_manifest(&events, &policy, &HeuristicSummarizer, None).await;
        assert!(manifest.summary.is_some(), "manifest must have a summary");
        let result = apply_manifest(&events, &manifest, None);
        // Summary UserInput + the one verbatim assistant turn = 2 items.
        assert_eq!(result.len(), 2);
        // First item is the injected summary (a UserInput).
        assert!(matches!(result[0].kind, TurnEventKind::UserInput { .. }));
        // The summary text comes from the manifest.
        if let TurnEventKind::UserInput { text } = &result[0].kind {
            assert_eq!(
                text,
                manifest.summary.as_ref().unwrap(),
                "summary text matches manifest"
            );
        }
        // Second item is the verbatim tail.
        assert_eq!(result[1].id, ids[3]);
    }

    #[tokio::test]
    async fn test_materialize_block_restores_output() {
        // The CAS round-trip now lives at the materialize layer, not
        // apply_manifest. The Isolate stage (PostToolUse) stores a large
        // output and writes a block_ref marker into the event log; Compress
        // keeps the marker verbatim in its round's group; the turn-group
        // projection materializes it on demand so the model sees the real
        // output. This test simulates the Isolate step by storing the output
        // and constructing the marker by hand.
        use houyicoder_memory::InMemoryBackend;
        let s = SessionId::new();
        let ids = ids(5);
        let big = serde_json::json!({"out": "y".repeat(200)});
        let backend = InMemoryBackend::new();
        let hash =
            pollster::block_on(backend.block_put(serde_json::to_vec(&big).unwrap())).unwrap();
        let marker = serde_json::json!({
            "block_ref": hash.0,
            "hint": "large output compacted; re-invoke the tool to retrieve it",
        });
        // The result event already carries the marker (as Isolate would write).
        let events = vec![
            ev(ids[0], s, user("task")),
            ev(ids[1], s, assistant("a1")),
            ev(ids[2], s, call("c1", "run")),
            ev(ids[3], s, result("c1", marker.clone())),
            ev(ids[4], s, assistant("a2")),
        ];
        let policy = CompressPolicy {
            tail_turns: 2,
            preserve_recent_tokens: 0,
            large_output_bytes: 50,
        };
        let manifest = build_manifest(&events, &policy, &HeuristicSummarizer, None).await;
        let filtered = apply_manifest(&events, &manifest, Some(&backend as &dyn ContextBackend));
        // The marker-bearing result stays in the verbatim tail, unchanged.
        let tr = filtered.iter().find(
            |e| matches!(&e.kind, TurnEventKind::ToolResult { call_id, .. } if call_id == "c1"),
        );
        if let TurnEventKind::ToolResult { output, .. } = &tr.expect("tr in view").kind {
            assert_eq!(
                output, &marker,
                "apply_manifest keeps the block_ref marker verbatim"
            );
        }
        // Materialization restores the real output from the CAS.
        let items = project_input_items(&filtered, Some(&backend as &dyn ContextBackend));
        let tr_item = items
            .iter()
            .find(|i| matches!(i, InputItem::ToolResult { .. }));
        if let InputItem::ToolResult { output, .. } = tr_item.expect("tr item exists") {
            assert_eq!(output, &big, "materialized output matches original");
        }
    }

    #[tokio::test]
    #[expect(clippy::too_many_lines, reason = "long by design, kept whole")]
    async fn test_materialize_fail_closed() {
        // When the backend cannot retrieve the block (Unsupported block_get),
        // materialize_block returns the block_ref marker as-is. The model
        // sees the hint and can re-invoke the tool. No content is lost
        // (the raw output is in the CAS) and no dangling pointer is sent.
        struct StubBackend;
        impl houyicoder_context::ContextBackend for StubBackend {
            fn append(
                &self,
                _: TurnEvent,
            ) -> houyicoder_async::PFut<'_, Result<EventId, ContextError>> {
                Box::pin(async move { Err(ContextError::Unsupported) })
            }
            fn read_range(
                &self,
                _: SessionId,
                _: Option<EventId>,
                _: Option<EventId>,
            ) -> houyicoder_async::PFut<'_, Result<Vec<TurnEvent>, ContextError>> {
                Box::pin(async move { Ok(Vec::new()) })
            }
            fn replay(
                &self,
                _: SessionId,
            ) -> houyicoder_async::PFut<'_, Result<Vec<TurnEvent>, ContextError>> {
                Box::pin(async move { Ok(Vec::new()) })
            }
            fn write_checkpoint(
                &self,
                _: houyicoder_context::CheckpointManifest,
            ) -> houyicoder_async::PFut<'_, Result<CheckpointId, ContextError>> {
                Box::pin(async move { Err(ContextError::Unsupported) })
            }
            fn read_checkpoint(
                &self,
                _: CheckpointId,
            ) -> houyicoder_async::PFut<
                '_,
                Result<houyicoder_context::CheckpointManifest, ContextError>,
            > {
                Box::pin(async move { Err(ContextError::Unsupported) })
            }
            fn list_checkpoints(
                &self,
                _: SessionId,
            ) -> houyicoder_async::PFut<'_, Result<Vec<CheckpointId>, ContextError>> {
                Box::pin(async move { Ok(Vec::new()) })
            }
        }
        let s = SessionId::new();
        let ids = ids(5);
        let marker = serde_json::json!({
            "block_ref": "deadbeef",
            "hint": "large output compacted; re-invoke the tool to retrieve it",
        });
        let events = vec![
            ev(ids[0], s, user("task")),
            ev(ids[1], s, assistant("a1")),
            ev(ids[2], s, call("c1", "run")),
            ev(ids[3], s, result("c1", marker.clone())),
            ev(ids[4], s, assistant("a2")),
        ];
        let policy = CompressPolicy {
            tail_turns: 2,
            preserve_recent_tokens: 0,
            large_output_bytes: 50,
        };
        let manifest = build_manifest(&events, &policy, &HeuristicSummarizer, None).await;
        let backend = StubBackend;
        let filtered = apply_manifest(&events, &manifest, Some(&backend as &dyn ContextBackend));
        let items = project_input_items(&filtered, Some(&backend as &dyn ContextBackend));
        let tr_item = items
            .iter()
            .find(|i| matches!(i, InputItem::ToolResult { .. }));
        if let InputItem::ToolResult { output, .. } = tr_item.expect("tr item exists") {
            assert_eq!(
                output.get("unavailable").and_then(|v| v.as_bool()),
                Some(true),
                "Unsupported block_get surfaces a typed unavailable marker: {output}"
            );
            assert_eq!(
                output.get("block_ref").and_then(|v| v.as_str()),
                Some("deadbeef"),
                "unavailable marker still carries the block_ref"
            );
        }
        // Pin the no-op stub contract: the backend's block_get returns
        // Unsupported (the trait default) and every required method returns
        // Unsupported or empty — the premise of the fail-closed path above.
        assert!(backend.append(events[0].clone()).await.is_err());
        assert!(backend.replay(s).await.unwrap().is_empty());
        assert!(
            backend.write_checkpoint(manifest.clone()).await.is_err(),
            "write_checkpoint returns Unsupported"
        );
        assert!(
            backend.read_checkpoint(CheckpointId::new()).await.is_err(),
            "read_checkpoint returns Unsupported"
        );
        assert!(backend.list_checkpoints(s).await.unwrap().is_empty());
        assert!(
            backend.block_put(Vec::new()).await.is_err(),
            "block_put default = Unsupported"
        );
        assert!(
            backend
                .block_get(&BlockHash("missing".into()))
                .await
                .is_err(),
            "block_get default = Unsupported"
        );
    }

    #[tokio::test]
    async fn test_manifest_new_events_verbatim() {
        // Events appended after the manifest was built are not in the plan.
        // They default to Verbatim so the model sees the latest turns.
        let s = SessionId::new();
        let ids = ids(4);
        let events = vec![
            ev(ids[0], s, user("task")),
            ev(ids[1], s, assistant("old")),
            ev(ids[2], s, assistant("mid")),
            ev(ids[3], s, assistant("new")),
        ];
        let policy = CompressPolicy {
            tail_turns: 1,
            preserve_recent_tokens: 0,
            large_output_bytes: 0,
        };
        let manifest = build_manifest(&events, &policy, &HeuristicSummarizer, None).await;
        // Now append an event NOT covered by the manifest.
        let extra_id = EventId::new();
        let mut events_with_extra = events.clone();
        events_with_extra.push(ev(extra_id, s, assistant("extra")));
        let result = apply_manifest(&events_with_extra, &manifest, None);
        // The extra event must be in the output (defaulted to Verbatim).
        assert!(
            result.iter().any(|e| e.id == extra_id),
            "new event not in plan must default to Verbatim"
        );
    }

    #[tokio::test]
    async fn test_pair_integral() {
        // The pair invariant is now structural: a tool_use and its
        // tool_result share a TurnGroup, so they share a fate — both kept
        // (Verbatim) or both dropped (Summarized). After apply_manifest,
        // no orphan ToolResult survives without its ToolCall in the view.
        // The big pair sits in the Summarized span (dropped together);
        // the small pair sits in the verbatim tail (kept together).
        let s = SessionId::new();
        let ids = ids(7);
        let big = serde_json::json!({"out": "y".repeat(300)});
        let small = serde_json::json!({"ok": true});
        let events = vec![
            ev(ids[0], s, user("first")),
            ev(ids[1], s, call("big", "run")),
            ev(ids[2], s, result("big", big)),
            ev(ids[3], s, assistant("a1")),
            ev(ids[4], s, call("sm", "run")),
            ev(ids[5], s, result("sm", small)),
            ev(ids[6], s, assistant("a2")),
        ];
        let policy = CompressPolicy {
            tail_turns: 2,
            preserve_recent_tokens: 0,
            large_output_bytes: 50,
        };
        let manifest = build_manifest(&events, &policy, &HeuristicSummarizer, None).await;
        let result = apply_manifest(&events, &manifest, None);
        assert!(pairs_intact(&result), "pair invariant must hold");
        // The big pair is Summarized (both dropped); the small pair is
        // Verbatim (both kept) — integral fate, no orphan.
        assert!(
            result.iter().any(|e| matches!(&e.kind,
                TurnEventKind::ToolCall { call_id, .. } if call_id == "sm")),
            "small pair tool_call kept in verbatim tail"
        );
        assert!(
            result.iter().any(|e| matches!(&e.kind,
                TurnEventKind::ToolResult { call_id, .. } if call_id == "sm")),
            "small pair tool_result kept in verbatim tail"
        );
        assert!(
            !result.iter().any(|e| matches!(&e.kind,
                TurnEventKind::ToolCall { call_id, .. } if call_id == "big")),
            "big pair tool_call dropped with its group"
        );
        // Also verify project_input_items does not trip its debug_assert.
        let _items = project_input_items(&result, None);
    }

    #[tokio::test]
    async fn test_manifest_empty_plan() {
        // An empty manifest plan: all events default to Verbatim. No summary
        // injected (no Summarized events). Behaves like full replay.
        let s = SessionId::new();
        let ids = ids(2);
        let events = vec![ev(ids[0], s, user("hi")), ev(ids[1], s, assistant("hello"))];
        let manifest = houyicoder_context::CheckpointManifest {
            id: CheckpointId::new(),
            session: s,
            last_event: ids[1],
            summary: None,
            plan: Vec::new(),
            ts: 0,
        };
        let result = apply_manifest(&events, &manifest, None);
        assert_eq!(result.len(), 2, "empty plan => all verbatim");
        assert_eq!(result[0].id, ids[0]);
        assert_eq!(result[1].id, ids[1]);
    }

    #[tokio::test]
    async fn test_manifest_verbatim_and_summarized() {
        // A combined manifest with a Verbatim tail and a Summarized span.
        // The summary is injected once; the Summarized tool pair (c0) is
        // dropped with its group (integral fate — no Referenced override
        // splits it); the Verbatim tool pair (c1) in the tail keeps its
        // raw output. The big result's size no longer matters at Compress.
        let s = SessionId::new();
        let ids = ids(8);
        let big = serde_json::json!({"data": "z".repeat(200)});
        let events = vec![
            ev(ids[0], s, user("start")),
            ev(ids[1], s, call("c0", "run")),
            ev(ids[2], s, result("c0", big)),
            ev(ids[3], s, assistant("summarized turn")),
            ev(ids[4], s, assistant("boundary turn")),
            ev(ids[5], s, call("c1", "run")),
            ev(ids[6], s, result("c1", serde_json::json!({"ok": true}))),
            ev(ids[7], s, assistant("latest turn")),
        ];
        let policy = CompressPolicy {
            tail_turns: 2,
            preserve_recent_tokens: 0,
            large_output_bytes: 50,
        };
        let manifest = build_manifest(&events, &policy, &HeuristicSummarizer, None).await;
        let result = apply_manifest(&events, &manifest, None);
        // The pair invariant must hold: no orphan ToolResult in the view.
        assert!(pairs_intact(&result));
        // Exactly one summary UserInput injected.
        let summary_count = result.iter().filter(|e| {
            matches!(&e.kind, TurnEventKind::UserInput { text } if text == manifest.summary.as_ref().unwrap())
        }).count();
        assert_eq!(summary_count, 1, "exactly one summary injected");
        // The big tool pair (c0) is Summarized — dropped with its group.
        assert!(
            !result.iter().any(|e| matches!(&e.kind,
                TurnEventKind::ToolResult { call_id, .. } if call_id == "c0")),
            "summarized big result dropped with its group"
        );
        // The small tool pair (c1) in the verbatim tail keeps its raw output.
        let small_tr = result.iter().find(
            |e| matches!(&e.kind, TurnEventKind::ToolResult { call_id, .. } if call_id == "c1"),
        );
        if let Some(TurnEvent {
            kind: TurnEventKind::ToolResult { output, .. },
            ..
        }) = small_tr
        {
            assert!(
                output.get("ok").is_some(),
                "small verbatim result keeps raw output"
            );
        }
    }
}
