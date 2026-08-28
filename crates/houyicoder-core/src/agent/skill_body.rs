//! Revive invoked skill bodies after a compaction. SkillBody events are
//! Summarized (folded out of the served view post-compact) and excluded
//! from the folded summary span, so a compaction drops them from the view.
//! This step re-appends the most-recent bodies from the raw log, bounded
//! by a per-agent byte budget (most-recent-first), so the model retains
//! invoked skill directives across a compaction boundary.

use houyicoder_context::{SessionId, TurnEventKind};

use super::append::new_event;
use super::{RunError, Runner, projection};

/// Per-agent byte budget for revived skill bodies. Most-recent-first: when
/// exceeded, the body that would overflow is head-truncated to the
/// remainder so the model keeps the most recent directives, not the whole
/// history.
const SKILL_BODY_BUDGET_BYTES: usize = 25_000;

/// Per-skill byte budget. A single oversized body (one with embedded
/// scripts or large reference) cannot starve the others.
const PER_SKILL_BUDGET_BYTES: usize = 5_000;

/// Head-truncate a string to a byte budget, keeping the head + an ellipsis
/// so the model sees the directive's opening + knows it was trimmed.
fn head_truncate(s: &str, budget: usize) -> String {
    if s.len() <= budget {
        return s.to_string();
    }
    let keep = budget.saturating_sub(3);
    let mut t: String = s.chars().take(keep).collect();
    t.push_str("...");
    t
}

impl Runner {
    /// Re-append invoked skill bodies a compaction folded out of the served
    /// view. No-op when a SkillBody already survives in the view. When none
    /// survives, the most-recent body per skill (dedup by name, R23g) is
    /// re-appended within a per-skill + per-agent byte budget (most-recent-
    /// first head-truncate). The re-appended events land after the
    /// compaction boundary, so they are Verbatim and survive until the next
    /// compaction folds them out again.
    pub(crate) async fn inject_skill_body(&self, session: SessionId) -> Result<(), RunError> {
        let view = self.store.current_view(session).await?;
        let filtered = match view.manifest.as_ref() {
            Some(m) => projection::apply_manifest(&view.events, m, Some(self.store.backend())),
            None => view.events.clone(),
        };
        if filtered
            .iter()
            .any(|e| matches!(e.kind, TurnEventKind::SkillBody { .. }))
        {
            return Ok(());
        }
        // Most-recent body per skill (dedup by name) within the per-skill +
        // per-agent budget. Reverse-collect so the most recent land first;
        // reverse back to chronological for append order.
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut revived: Vec<(&houyicoder_context::TurnEvent, String)> = Vec::new();
        let mut total: usize = 0;
        for ev in view.events.iter().rev() {
            if let TurnEventKind::SkillBody {
                skill_name,
                content,
                ..
            } = &ev.kind
            {
                if !seen.insert(skill_name.clone()) {
                    continue;
                }
                let body = head_truncate(content, PER_SKILL_BUDGET_BYTES);
                if total + body.len() > SKILL_BODY_BUDGET_BYTES {
                    let remain = SKILL_BODY_BUDGET_BYTES.saturating_sub(total);
                    if remain == 0 {
                        break;
                    }
                    revived.push((ev, head_truncate(&body, remain)));
                    break;
                }
                total += body.len();
                revived.push((ev, body));
            }
        }
        for (ev, body) in revived.into_iter().rev() {
            if let TurnEventKind::SkillBody {
                skill_name,
                agent_id,
                untrusted,
                ..
            } = &ev.kind
            {
                self.store
                    .append(new_event(
                        session,
                        TurnEventKind::SkillBody {
                            skill_name: skill_name.clone(),
                            content: body,
                            agent_id: agent_id.clone(),
                            untrusted: *untrusted,
                        },
                    ))
                    .await?;
            }
        }
        Ok(())
    }

    /// Re-announce the skill listing + revive invoked skill bodies after a
    /// compaction. The two always run together post-compact: the listing
    /// tells the model what is available, the bodies retain invoked
    /// directives. Pairing them at every compact site keeps the call sites
    /// to one line so the drive loop stays readable.
    pub(crate) async fn inject_skill_listing_and_body(
        &self,
        session: SessionId,
    ) -> Result<(), RunError> {
        self.inject_skill_listing(session).await?;
        self.inject_skill_body(session).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use houyicoder_api::skill::{SkillDescriptor, SkillError, SkillRegistry};
    use houyicoder_context::{SessionId, TurnEventKind};
    use houyicoder_memory::InMemoryBackend;
    use houyicoder_resilience::Retry;
    use houyicoder_session::SessionStore;
    use std::sync::Arc;

    struct EmptyRegistry;
    impl SkillRegistry for EmptyRegistry {
        fn list_model_invocable(&self) -> Vec<SkillDescriptor> {
            Vec::new()
        }
        fn find(&self, _: &str) -> Option<SkillDescriptor> {
            None
        }
        fn prepare_body(
            &self,
            _: &str,
            _: Option<&str>,
            _: Option<&str>,
        ) -> Result<String, SkillError> {
            Err(SkillError::NotFound("none".into()))
        }
    }

    fn runner() -> Runner {
        let store: Arc<dyn houyicoder_api::session::SessionLog> =
            Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
        Runner::with_shared_store(
            store,
            Arc::new(crate::provider::test_support::FakeProvider::text("done")),
            crate::agent::ToolRegistry::new(),
            crate::agent::runner_config::RunnerConfig {
                model: "test".into(),
                instructions: String::new(),
                max_turns: 5,
                max_output_tokens: 8_000,
                retry: Retry::default(),
            },
        )
        .with_skill_registry(Arc::new(EmptyRegistry))
    }

    fn body_kind(name: &str, content: &str, untrusted: bool) -> TurnEventKind {
        TurnEventKind::SkillBody {
            skill_name: name.into(),
            content: content.into(),
            agent_id: None,
            untrusted,
        }
    }

    /// When a SkillBody already survives in the view, inject is a no-op: the
    /// event count does not grow (the model already sees the body).
    #[tokio::test]
    async fn test_inject_noop_when_survives() {
        let runner = runner();
        let session = SessionId::new();
        runner
            .store()
            .append(super::new_event(
                session,
                TurnEventKind::SkillBody {
                    skill_name: "commit".into(),
                    content: "body".into(),
                    agent_id: None,
                    untrusted: false,
                },
            ))
            .await
            .unwrap();
        let before = runner
            .store()
            .current_view(session)
            .await
            .unwrap()
            .events
            .len();
        runner.inject_skill_body(session).await.unwrap();
        let after = runner
            .store()
            .current_view(session)
            .await
            .unwrap()
            .events
            .len();
        assert_eq!(before, after, "no-op when a SkillBody survives in the view");
    }

    /// When the raw log has no SkillBody at all, inject is a no-op.
    #[tokio::test]
    async fn test_inject_noop_when_empty() {
        let runner = runner();
        let session = SessionId::new();
        runner.inject_skill_body(session).await.unwrap();
        let view = runner.store().current_view(session).await.unwrap();
        assert!(
            view.events
                .iter()
                .all(|e| !matches!(e.kind, TurnEventKind::SkillBody { .. })),
            "no SkillBody appended when none exists to revive"
        );
    }

    /// Post-compact (a manifest folds the SkillBody out of the served view),
    /// inject re-appends the most-recent body so the model retains the
    /// directive across the boundary. Pins the durable-survival behavior:
    /// an invoked skill's guidance is not lost to a compaction.
    #[tokio::test]
    async fn test_inject_revives_after_fold() {
        use houyicoder_context::TurnEvent;
        use houyicoder_context::{
            CheckpointId, CheckpointManifest, Disposition, EventId, TurnGroup,
        };
        let runner = runner();
        let session = SessionId::new();
        let body_id = EventId::new();
        runner
            .store()
            .append(TurnEvent {
                id: body_id,
                session,
                ts: 1,
                prev_hash: None,
                kind: TurnEventKind::SkillBody {
                    skill_name: "commit".into(),
                    content: "commit body".into(),
                    agent_id: None,
                    untrusted: false,
                },
            })
            .await
            .unwrap();
        let manifest = CheckpointManifest {
            id: CheckpointId::new(),
            session,
            last_event: body_id,
            summary: Some("prior compact".into()),
            plan: vec![TurnGroup {
                turn_id: body_id,
                disposition: Disposition::Summarized,
                event_ids: vec![body_id],
            }],
            ts: 2,
        };
        runner
            .store()
            .backend()
            .write_checkpoint(manifest)
            .await
            .unwrap();
        let view_before = runner.store().current_view(session).await.unwrap();
        let filtered = match view_before.manifest.as_ref() {
            Some(m) => {
                projection::apply_manifest(&view_before.events, m, Some(runner.store().backend()))
            }
            None => view_before.events.clone(),
        };
        assert!(
            filtered
                .iter()
                .all(|e| !matches!(e.kind, TurnEventKind::SkillBody { .. })),
            "manifest folded the SkillBody out of the served view"
        );
        runner.inject_skill_body(session).await.unwrap();
        let view_after = runner.store().current_view(session).await.unwrap();
        let count = view_after
            .events
            .iter()
            .filter(|e| matches!(e.kind, TurnEventKind::SkillBody { .. }))
            .count();
        assert_eq!(count, 2, "revive appended a second SkillBody: {count}");
    }

    /// Revive dedups by skill name (R23g): the most-recent body per skill
    /// is kept, older invocations of the same skill are not revived
    /// (would double the budget + grow the log without bound). A body over
    /// the per-skill cap is head-truncated.
    #[tokio::test]
    async fn test_revive_dedup_and_truncate() {
        use houyicoder_context::TurnEvent;
        use houyicoder_context::{
            CheckpointId, CheckpointManifest, Disposition, EventId, TurnGroup,
        };
        let runner = runner();
        let session = SessionId::new();
        let id1 = EventId::new();
        runner
            .store()
            .append(TurnEvent {
                id: id1,
                session,
                ts: 0,
                prev_hash: None,
                kind: body_kind("a", "a-old", false),
            })
            .await
            .unwrap();
        let id2 = EventId::new();
        runner
            .store()
            .append(TurnEvent {
                id: id2,
                session,
                ts: 0,
                prev_hash: None,
                kind: body_kind("b", &"x".repeat(6_000), false),
            })
            .await
            .unwrap();
        let id3 = EventId::new();
        runner
            .store()
            .append(TurnEvent {
                id: id3,
                session,
                ts: 0,
                prev_hash: None,
                kind: body_kind("a", "a-new", false),
            })
            .await
            .unwrap();
        let manifest = CheckpointManifest {
            id: CheckpointId::new(),
            session,
            last_event: id3,
            summary: Some("compact".into()),
            plan: vec![TurnGroup {
                turn_id: id1,
                disposition: Disposition::Summarized,
                event_ids: vec![id1, id2, id3],
            }],
            ts: 1,
        };
        runner
            .store()
            .backend()
            .write_checkpoint(manifest)
            .await
            .unwrap();
        runner.inject_skill_body(session).await.unwrap();
        let view = runner.store().current_view(session).await.unwrap();
        let revived: Vec<_> = view
            .events
            .iter()
            .filter_map(|e| match &e.kind {
                TurnEventKind::SkillBody {
                    skill_name,
                    content,
                    ..
                } => Some((skill_name.clone(), content.clone())),
                _ => None,
            })
            .collect();
        // 3 original + 2 revived (a most-recent + b truncated) = 5.
        assert_eq!(revived.len(), 5, "3 original + 2 revived: {:?}", revived);
        let a = revived[3..]
            .iter()
            .find(|(n, _)| n == "a")
            .expect("a revived");
        assert_eq!(a.1, "a-new", "dedup keeps the most-recent body per skill");
        let b = revived[3..]
            .iter()
            .find(|(n, _)| n == "b")
            .expect("b revived");
        assert!(
            b.1.len() <= PER_SKILL_BUDGET_BYTES,
            "b head-truncated to per-skill cap: {} > {}",
            b.1.len(),
            PER_SKILL_BUDGET_BYTES
        );
        assert!(b.1.ends_with("..."), "b truncated with ellipsis: {}", b.1);
    }
}
