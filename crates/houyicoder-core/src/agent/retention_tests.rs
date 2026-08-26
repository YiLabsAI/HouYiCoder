use super::*;

use std::sync::Arc;

use crate::agent::cache_liveness::{CACHE_TTL_MS, CacheLivenessRetentionPolicy, CachedPrefixState};

/// A backend that counts block_get calls so a test can assert the
/// Summarize and Evict tiers skip the retrieval. block_put stores bytes
/// so a marker with a real hash round-trips when Materialize fires.
struct CountingBackend {
    gets: std::sync::atomic::AtomicUsize,
    store: std::sync::Mutex<std::collections::HashMap<String, Vec<u8>>>,
}

impl CountingBackend {
    fn new() -> Self {
        Self {
            gets: 0.into(),
            store: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }
    fn gets(&self) -> usize {
        self.gets.load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl houyicoder_context::ContextBackend for CountingBackend {
    fn append(
        &self,
        _: houyicoder_context::TurnEvent,
    ) -> houyicoder_async::PFut<
        '_,
        Result<houyicoder_context::EventId, houyicoder_context::ContextError>,
    > {
        Box::pin(async move { Err(houyicoder_context::ContextError::Unsupported) })
    }
    fn read_range(
        &self,
        _: houyicoder_context::SessionId,
        _: Option<houyicoder_context::EventId>,
        _: Option<houyicoder_context::EventId>,
    ) -> houyicoder_async::PFut<
        '_,
        Result<Vec<houyicoder_context::TurnEvent>, houyicoder_context::ContextError>,
    > {
        Box::pin(async move { Ok(Vec::new()) })
    }
    fn replay(
        &self,
        _: houyicoder_context::SessionId,
    ) -> houyicoder_async::PFut<
        '_,
        Result<Vec<houyicoder_context::TurnEvent>, houyicoder_context::ContextError>,
    > {
        Box::pin(async move { Ok(Vec::new()) })
    }
    fn write_checkpoint(
        &self,
        _: houyicoder_context::CheckpointManifest,
    ) -> houyicoder_async::PFut<
        '_,
        Result<houyicoder_context::CheckpointId, houyicoder_context::ContextError>,
    > {
        Box::pin(async move { Err(houyicoder_context::ContextError::Unsupported) })
    }
    fn read_checkpoint(
        &self,
        _: houyicoder_context::CheckpointId,
    ) -> houyicoder_async::PFut<
        '_,
        Result<houyicoder_context::CheckpointManifest, houyicoder_context::ContextError>,
    > {
        Box::pin(async move { Err(houyicoder_context::ContextError::Unsupported) })
    }
    fn list_checkpoints(
        &self,
        _: houyicoder_context::SessionId,
    ) -> houyicoder_async::PFut<
        '_,
        Result<Vec<houyicoder_context::CheckpointId>, houyicoder_context::ContextError>,
    > {
        Box::pin(async move { Ok(Vec::new()) })
    }
    fn block_put(
        &self,
        block: Vec<u8>,
    ) -> houyicoder_async::PFut<'_, Result<BlockHash, houyicoder_context::ContextError>> {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut store = self.store.lock().unwrap();
        let mut hasher = DefaultHasher::new();
        block.hash(&mut hasher);
        let hash = format!("{:016x}", hasher.finish());
        store.insert(hash.clone(), block);
        Box::pin(async move { Ok(BlockHash(hash)) })
    }
    fn block_get(
        &self,
        hash: &BlockHash,
    ) -> houyicoder_async::PFut<'_, Result<Vec<u8>, houyicoder_context::ContextError>> {
        self.gets.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let store = self.store.lock().unwrap();
        let out = store.get(&hash.0).cloned();
        Box::pin(async move { out.ok_or(houyicoder_context::ContextError::Unsupported) })
    }
}

fn marker_with_preview(hash: &str, preview: &str) -> serde_json::Value {
    serde_json::json!({
        "block_ref": hash,
        "preview": preview,
        "hint": "large output compacted; re-invoke the tool to retrieve it",
    })
}

/// The 3-tier policy must not unconditionally retrieve. Materialize pays
/// the block_get; Summarize serves the preview and skips retrieval; Evict
/// serves the pointer and skips retrieval. A superseded result evicts
/// regardless of age. This is the fix for the prior semantic bug where
/// every block_ref was re-read into the view — zero savings.
#[test]
fn test_rehydrate_3tier_not_unconditional() {
    let backend = CountingBackend::new();
    // Store a real block so Materialize round-trips and the hash resolves.
    let big = serde_json::json!({"out": "x".repeat(300)});
    let hash = pollster::block_on(backend.block_put(serde_json::to_vec(&big).unwrap())).unwrap();
    let marker = marker_with_preview(&hash.0, "first 200 chars...");
    let policy = AgeRetentionPolicy::default();

    // Recent (age 0) materializes — block_get fires, full content served.
    let ctx = RetentionContext {
        age_in_turns: 0,
        is_superseded: false,
        block_ref: None,
        now_ms: 0,
        cache_cold: false,
    };
    let out = materialize_block(
        &marker,
        Some(&backend as &dyn ContextBackend),
        &policy,
        &ctx,
    );
    assert_eq!(out, big, "recent result materializes in full");
    assert_eq!(backend.gets(), 1, "Materialize paid one block_get");

    // Middle (age 3) summarizes — preview served, no block_get.
    let ctx = RetentionContext {
        age_in_turns: 3,
        is_superseded: false,
        block_ref: None,
        now_ms: 0,
        cache_cold: false,
    };
    let out = materialize_block(
        &marker,
        Some(&backend as &dyn ContextBackend),
        &policy,
        &ctx,
    );
    assert!(
        out.get("summarized").is_some(),
        "middle result summarized, got: {out}"
    );
    assert!(out.get("preview").is_some(), "summary carries the preview");
    assert_eq!(
        backend.gets(),
        1,
        "Summarize skipped block_get (still 1, not 2)"
    );

    // Old (age 10) evicts — typed evicted marker, no block_get.
    let ctx = RetentionContext {
        age_in_turns: 10,
        is_superseded: false,
        block_ref: None,
        now_ms: 0,
        cache_cold: false,
    };
    let out = materialize_block(
        &marker,
        Some(&backend as &dyn ContextBackend),
        &policy,
        &ctx,
    );
    assert_eq!(
        out.get("evicted").and_then(|v| v.as_bool()),
        Some(true),
        "old result evicts to a typed evicted marker: {out}"
    );
    assert_eq!(
        out.get("block_ref").and_then(|v| v.as_str()),
        Some(hash.0.as_str()),
        "evicted marker still carries the block_ref"
    );
    assert!(
        out.get("preview").is_none(),
        "evicted marker carries no preview"
    );
    assert_eq!(
        backend.gets(),
        1,
        "Evict skipped block_get (still 1, not 2)"
    );

    // Superseded evicts regardless of age — typed evicted marker, no
    // block_get.
    let ctx = RetentionContext {
        age_in_turns: 0,
        is_superseded: true,
        block_ref: None,
        now_ms: 0,
        cache_cold: false,
    };
    let out = materialize_block(
        &marker,
        Some(&backend as &dyn ContextBackend),
        &policy,
        &ctx,
    );
    assert_eq!(
        out.get("evicted").and_then(|v| v.as_bool()),
        Some(true),
        "superseded result evicts even at age 0: {out}"
    );
    assert_eq!(
        backend.gets(),
        1,
        "superseded evicted, no block_get (still 1)"
    );
}

/// A marker without a preview field, under Summarize, falls back to the
/// pointer form rather than paying a retrieval — the preview is the
/// savings, and without it there is nothing to summarize to.
#[test]
fn test_summarize_without_preview_marker() {
    let backend = CountingBackend::new();
    let big = serde_json::json!({"out": "y".repeat(300)});
    let hash = pollster::block_on(backend.block_put(serde_json::to_vec(&big).unwrap())).unwrap();
    let marker_no_preview = serde_json::json!({
        "block_ref": hash.0,
        "hint": "large output compacted; re-invoke the tool to retrieve it",
    });
    let policy = AgeRetentionPolicy::default();
    let ctx = RetentionContext {
        age_in_turns: 3,
        is_superseded: false,
        block_ref: None,
        now_ms: 0,
        cache_cold: false,
    };
    let out = materialize_block(
        &marker_no_preview,
        Some(&backend as &dyn ContextBackend),
        &policy,
        &ctx,
    );
    assert_eq!(
        out, marker_no_preview,
        "no preview => pointer form, no retrieval"
    );
    assert_eq!(backend.gets(), 0, "no block_get without a preview");
}

/// A field-level marker (isolate_large_output externalized the largest
/// string field) round-trips through the Materialize tier with the envelope
/// intact: agentId/color/status stay inline, the content field is restored
/// byte-exact. Content is newline + quote dense — the shape that broke the
/// first B2 fix, where escaping grew the serialized bytes past the raw-byte
/// budget. This asserts the round-trip is byte-exact through serde_json.
#[test]
fn test_materialize_field_restores() {
    let backend = CountingBackend::new();
    // Newline + quote dense: every char that serde_json escapes, so a
    // raw-byte budget would under-count and the round-trip would diverge.
    let content = "line with \"quotes\" and \\ backslash\nnext line\n".repeat(400);
    let (marker, _hash) = nested_marker(&content, &backend);
    let ctx = age_ctx(0);
    let out = materialize_block(
        &marker,
        Some(&backend as &dyn ContextBackend),
        &AgeRetentionPolicy::default(),
        &ctx,
    );
    // Envelope keys preserved across the Materialize tier — the whole point
    // of field-level: agentId survives so the TUI renders the fold-group.
    assert_eq!(out["agentId"].as_str(), Some("child-xyz"));
    assert_eq!(out["color"].as_str(), Some("red"));
    assert_eq!(out["status"].as_str(), Some("completed"));
    // Content field restored byte-exact (Materialize retrieved the block).
    assert_eq!(
        out["content"].as_str(),
        Some(content.as_str()),
        "materialize restores the field content byte-exact through serde_json"
    );
    assert_eq!(
        backend.gets(),
        1,
        "Materialize fires exactly one block_get for the field"
    );
}

/// The Summarize tier (age in the summarize band) keeps the envelope's
/// agentId and replaces only the content field with a summarized marker
/// (preview + block_ref + hint). No block_get fires.
#[test]
fn test_materialize_field_summarize() {
    let backend = CountingBackend::new();
    let content = "summary content with \"quotes\"\n".repeat(400);
    let (marker, _hash) = nested_marker(&content, &backend);
    let ctx = age_ctx(3); // 2 <= age < 6 → Summarize
    let out = materialize_block(
        &marker,
        Some(&backend as &dyn ContextBackend),
        &AgeRetentionPolicy::default(),
        &ctx,
    );
    assert_eq!(
        out["agentId"].as_str(),
        Some("child-xyz"),
        "envelope survives Summarize"
    );
    assert_eq!(
        out["content"]["summarized"].as_bool(),
        Some(true),
        "content field becomes the summarized marker"
    );
    assert!(
        out["content"]["preview"].is_string(),
        "preview carried into the summarized field"
    );
    assert!(
        out["content"]["block_ref"].is_string(),
        "block_ref kept so a later re-invoke can retrieve"
    );
    assert_eq!(backend.gets(), 0, "Summarize skips the block_get");
}

/// The Evict tier (age >= summarize_turns) keeps the envelope's agentId
/// and replaces only the content field with the evicted pointer. No
/// block_get fires. This is the tier the first B2 fix would have silently
/// broken — agentId lost N turns after the delegation, not immediately.
#[test]
fn test_materialize_field_evict() {
    let backend = CountingBackend::new();
    let content = "evicted content with \"quotes\"\n".repeat(400);
    let (marker, _hash) = nested_marker(&content, &backend);
    let ctx = age_ctx(6); // age >= 6 → Evict
    let out = materialize_block(
        &marker,
        Some(&backend as &dyn ContextBackend),
        &AgeRetentionPolicy::default(),
        &ctx,
    );
    assert_eq!(
        out["agentId"].as_str(),
        Some("child-xyz"),
        "envelope survives Evict"
    );
    assert_eq!(
        out["content"]["evicted"].as_bool(),
        Some(true),
        "content field becomes the evicted pointer"
    );
    assert!(
        out["content"]["block_ref"].is_string(),
        "block_ref kept on evict"
    );
    assert_eq!(backend.gets(), 0, "Evict skips the block_get");
}

/// A top-level marker (whole-output externalize — blob tools, or old-session
/// logs written before field-level isolation) still whole-replaces: the
/// Materialize tier restores the entire output. Backward compat for resumed
/// sessions whose logs carry the old shape.
#[test]
fn test_materialize_top_level_compat() {
    let backend = CountingBackend::new();
    let original = serde_json::json!({"stdout": "x".repeat(300)});
    let bytes = serde_json::to_vec(&original).unwrap();
    let hash = pollster::block_on(backend.block_put(bytes)).unwrap();
    let marker = serde_json::json!({
        "block_ref": hash.0,
        "preview": "...",
        "hint": "large output compacted; re-invoke the tool to retrieve it",
    });
    let ctx = age_ctx(0);
    let out = materialize_block(
        &marker,
        Some(&backend as &dyn ContextBackend),
        &AgeRetentionPolicy::default(),
        &ctx,
    );
    assert_eq!(
        out, original,
        "top-level marker whole-replaces with the stored output"
    );
}

/// Pin the CountingBackend no-op stub contract: the six required methods the
/// CAS tests do not exercise return Unsupported or empty. Salvaged from the
/// deleted externalize_output test twin so the contract stays pinned.
#[test]
fn test_counting_backend_noop() {
    let backend = CountingBackend::new();
    use houyicoder_context::{CheckpointId, EventId, SessionId, TurnEvent};
    let ev = TurnEvent {
        id: EventId::new(),
        session: SessionId::new(),
        ts: 0,
        prev_hash: None,
        kind: houyicoder_context::TurnEventKind::UserInput { text: "x".into() },
    };
    assert!(pollster::block_on(backend.append(ev.clone())).is_err());
    assert!(
        pollster::block_on(backend.read_range(SessionId::new(), None, None))
            .unwrap()
            .is_empty()
    );
    assert!(
        pollster::block_on(backend.replay(SessionId::new()))
            .unwrap()
            .is_empty()
    );
    assert!(
        pollster::block_on(
            backend.write_checkpoint(houyicoder_context::CheckpointManifest {
                id: CheckpointId::new(),
                session: SessionId::new(),
                last_event: EventId::new(),
                summary: None,
                plan: Vec::new(),
                ts: 0,
            })
        )
        .is_err()
    );
    assert!(pollster::block_on(backend.read_checkpoint(CheckpointId::new())).is_err());
    assert!(
        pollster::block_on(backend.list_checkpoints(SessionId::new()))
            .unwrap()
            .is_empty()
    );
}

/// Build a field-level marker: block_put the content string, nest the
/// marker under the content key, keep the envelope (agentId/color/status/
/// usage) inline. Mirrors what isolate_large_output produces for a
/// structured result with a large content field.
fn nested_marker(content: &str, backend: &CountingBackend) -> (serde_json::Value, String) {
    let content_val = serde_json::json!(content);
    let bytes = serde_json::to_vec(&content_val).unwrap();
    let hash = pollster::block_on(
        <CountingBackend as houyicoder_context::ContextBackend>::block_put(backend, bytes),
    )
    .unwrap();
    let marker = serde_json::json!({
        "status": "completed",
        "content": {
            "block_ref": hash.0,
            "preview": "preview...",
            "data_tag": false,
            "hint": "large output compacted; re-invoke the tool to retrieve it",
        },
        "agentId": "child-xyz",
        "color": "red",
        "usage": {"input_tokens": 100, "output_tokens": 20},
    });
    (marker, hash.0)
}

fn age_ctx(age: u32) -> RetentionContext<'static> {
    RetentionContext {
        age_in_turns: age,
        is_superseded: false,
        block_ref: None,
        now_ms: 0,
        cache_cold: false,
    }
}

#[test]
fn test_superseded_file_re_read() {
    use houyicoder_context::{EventId, SessionId, TurnEvent, TurnEventKind};
    fn tc(cid: &str, tool: &str, input: serde_json::Value) -> TurnEvent {
        TurnEvent {
            id: EventId::new(),
            session: SessionId::new(),
            ts: 0,
            prev_hash: None,
            kind: TurnEventKind::ToolCall {
                call_id: cid.into(),
                tool: tool.into(),
                input,
            },
        }
    }
    fn tr(cid: &str) -> TurnEvent {
        TurnEvent {
            id: EventId::new(),
            session: SessionId::new(),
            ts: 1,
            prev_hash: None,
            kind: TurnEventKind::ToolResult {
                call_id: cid.into(),
                output: serde_json::json!({"ok": true}),
                duration_ms: 0,
            },
        }
    }
    // read a.rs → result; a later read of a.rs supersedes the first.
    let events = vec![
        tc("c1", "read", serde_json::json!({"path": "a.rs"})),
        tr("c1"),
        tc("c2", "read", serde_json::json!({"path": "a.rs"})),
        tr("c2"),
    ];
    assert!(
        superseded_by_later(&events, 1),
        "first read superseded by the later read"
    );
    assert!(
        !superseded_by_later(&events, 3),
        "the latest read is not superseded"
    );
}

#[test]
fn test_different_file_not_superseded() {
    use houyicoder_context::{EventId, SessionId, TurnEvent, TurnEventKind};
    fn tc(cid: &str, path: &str) -> TurnEvent {
        TurnEvent {
            id: EventId::new(),
            session: SessionId::new(),
            ts: 0,
            prev_hash: None,
            kind: TurnEventKind::ToolCall {
                call_id: cid.into(),
                tool: "read".into(),
                input: serde_json::json!({"path": path}),
            },
        }
    }
    fn tr(cid: &str) -> TurnEvent {
        TurnEvent {
            id: EventId::new(),
            session: SessionId::new(),
            ts: 1,
            prev_hash: None,
            kind: TurnEventKind::ToolResult {
                call_id: cid.into(),
                output: serde_json::json!({}),
                duration_ms: 0,
            },
        }
    }
    // read a.rs, then read b.rs — different files, not superseded.
    let events = vec![tc("c1", "a.rs"), tr("c1"), tc("c2", "b.rs"), tr("c2")];
    assert!(
        !superseded_by_later(&events, 1),
        "different file is not superseded"
    );
}

#[test]
fn test_edit_supersedes_prior_read() {
    use houyicoder_context::{EventId, SessionId, TurnEvent, TurnEventKind};
    fn tc(cid: &str, tool: &str, input: serde_json::Value) -> TurnEvent {
        TurnEvent {
            id: EventId::new(),
            session: SessionId::new(),
            ts: 0,
            prev_hash: None,
            kind: TurnEventKind::ToolCall {
                call_id: cid.into(),
                tool: tool.into(),
                input,
            },
        }
    }
    fn tr(cid: &str) -> TurnEvent {
        TurnEvent {
            id: EventId::new(),
            session: SessionId::new(),
            ts: 1,
            prev_hash: None,
            kind: TurnEventKind::ToolResult {
                call_id: cid.into(),
                output: serde_json::json!({}),
                duration_ms: 0,
            },
        }
    }
    // read a.rs, then edit a.rs — the edit changes the file, the read is stale.
    let events = vec![
        tc("c1", "read", serde_json::json!({"path": "a.rs"})),
        tr("c1"),
        tc(
            "c2",
            "edit",
            serde_json::json!({"path": "a.rs", "old_string": "x", "new_string": "y"}),
        ),
        tr("c2"),
    ];
    assert!(
        superseded_by_later(&events, 1),
        "a later edit of the same file supersedes the read"
    );
}

/// While the cached prefix is live, a block_ref's retention decision is
/// stable across serves — the same block served again at a later age does
/// not demote, so the prefix bytes do not change + the cache does not miss.
#[test]
fn test_holds_decision_while_alive() {
    let backend = CountingBackend::new();
    let big = serde_json::json!({"out": "x".repeat(300)});
    let hash = pollster::block_on(backend.block_put(serde_json::to_vec(&big).unwrap())).unwrap();
    let marker = marker_with_preview(&hash.0, "preview...");
    let state = Arc::new(CachedPrefixState::new());
    state.record_turn(1_000, 800); // cache hit + fresh → live
    let policy = CacheLivenessRetentionPolicy::new(Arc::clone(&state));

    // First serve at age 1 (alive, conservative band): Materialize.
    let ctx = RetentionContext {
        age_in_turns: 1,
        is_superseded: false,
        block_ref: None,
        now_ms: 1_100,
        cache_cold: false,
    };
    let out1 = materialize_block(
        &marker,
        Some(&backend as &dyn ContextBackend),
        &policy,
        &ctx,
    );
    assert_eq!(out1, big, "age 1 materializes");
    assert_eq!(backend.gets(), 1);

    // Second serve at age 8 while still alive. A fresh age-8 decision
    // would Summarize (8 >= conservative materialize 6), but the stored
    // Materialize holds — the prefix does not change.
    let ctx = RetentionContext {
        age_in_turns: 8,
        is_superseded: false,
        block_ref: None,
        now_ms: 1_200,
        cache_cold: false,
    };
    let out2 = materialize_block(
        &marker,
        Some(&backend as &dyn ContextBackend),
        &policy,
        &ctx,
    );
    assert_eq!(out2, big, "decision stable: Materialize held at age 8");
}

/// Once the cache expires (TTL elapses + the last turn missed), the stored
/// decision is no longer consulted — the aggressive band recomputes, so
/// an age-8 block Evicts instead of holding the stale Materialize.
#[test]
fn test_expired_recomputes_aggressively() {
    let backend = CountingBackend::new();
    let big = serde_json::json!({"out": "x".repeat(300)});
    let hash = pollster::block_on(backend.block_put(serde_json::to_vec(&big).unwrap())).unwrap();
    let marker = marker_with_preview(&hash.0, "preview...");
    let state = Arc::new(CachedPrefixState::new());
    state.record_turn(1_000, 800); // live
    let policy = CacheLivenessRetentionPolicy::new(Arc::clone(&state));

    // Store a Materialize decision at age 1 while alive.
    let ctx = RetentionContext {
        age_in_turns: 1,
        is_superseded: false,
        block_ref: None,
        now_ms: 1_100,
        cache_cold: false,
    };
    let _out = materialize_block(
        &marker,
        Some(&backend as &dyn ContextBackend),
        &policy,
        &ctx,
    );

    // The cache expires + the next turn missed (zero cache read).
    state.record_turn(1_000 + CACHE_TTL_MS + 5_000, 0);
    let ctx = RetentionContext {
        age_in_turns: 8,
        is_superseded: false,
        block_ref: None,
        now_ms: 1_000 + CACHE_TTL_MS + 5_001,
        cache_cold: false,
    };
    let out = materialize_block(
        &marker,
        Some(&backend as &dyn ContextBackend),
        &policy,
        &ctx,
    );
    assert!(
        out.get("evicted").and_then(|v| v.as_bool()) == Some(true),
        "expired cache recomputes aggressively: age 8 evicts, got: {out}"
    );
}

#[test]
fn test_cache_cold_downgrades_recent() {
    let backend = CountingBackend::new();
    let big = serde_json::json!({"out": "x".repeat(300)});
    let hash = pollster::block_on(backend.block_put(serde_json::to_vec(&big).unwrap())).unwrap();
    let marker = marker_with_preview(&hash.0, "first 200 chars...");
    let policy = AgeRetentionPolicy::default();
    let ctx = RetentionContext {
        age_in_turns: 0,
        is_superseded: false,
        block_ref: None,
        now_ms: 0,
        cache_cold: true,
    };
    let out = materialize_block(
        &marker,
        Some(&backend as &dyn ContextBackend),
        &policy,
        &ctx,
    );
    assert!(
        out.get("summarized").is_some(),
        "cache cold downgrades recent to Summarize: {out}"
    );
    assert_eq!(
        backend.gets(),
        0,
        "Summarize skipped block_get (no cache to hit)"
    );
    let ctx_warm = RetentionContext {
        age_in_turns: 0,
        is_superseded: false,
        block_ref: None,
        now_ms: 0,
        cache_cold: false,
    };
    let out_warm = materialize_block(
        &marker,
        Some(&backend as &dyn ContextBackend),
        &policy,
        &ctx_warm,
    );
    assert_eq!(out_warm, big, "warm cache materializes in full");
    assert_eq!(backend.gets(), 1, "warm paid one block_get");
}

#[test]
fn test_cache_cold_evicts_results() {
    let policy = AgeRetentionPolicy::default();
    // age beyond summarize_turns (6) + cache_cold = Evict (not Summarize).
    let ctx = RetentionContext {
        age_in_turns: 10,
        is_superseded: false,
        block_ref: None,
        now_ms: 0,
        cache_cold: true,
    };
    assert_eq!(
        policy.decide(&ctx),
        super::RetentionDecision::Evict,
        "cache cold + old age = Evict"
    );
}
