//! agent::skill_listing — the per-turn skill-discovery attachment.
//!
//! The turn-entry step that lists model-invocable skills as a user-role
//! system-reminder attachment at the conversation tail (the same seam as
//! memory-recall), plus the budget-capped listing formatter. Progressive
//! disclosure: the attachment carries only descriptions; the full body
//! loads on demand when the model calls the Skill tool.
//!
//! The step scans the served view for a surviving listing event and skips
//! when one exists. Compaction gives a listing the Summarized disposition,
//! so a folded listing drops out of the view — the scan naturally empties
//! and the listing re-surfaces after a compaction with no provider-side
//! clear (the same natural-reset pattern memory-recall uses).

use houyicoder_api::skill::SkillDescriptor;
use houyicoder_context::{SessionId, TurnEventKind};

use super::append::new_event;
use super::{RunError, Runner, model_window, projection};

/// Rough chars-per-token rate for converting a token budget to a char
/// budget. An estimate; the listing is truncated to fit, not billed.
/// The budget is 1% of the context window (the ecosystem convention:
/// discovery only), computed in char_budget as window x this / 100.
const CHARS_PER_TOKEN: u32 = 4;

/// Fallback char budget when the context window is unknown (1% of a
/// 200k-token window at 4 chars/token).
const DEFAULT_CHAR_BUDGET: usize = 8_000;

/// Per-entry hard cap on the combined description + when-to-use string.
/// The listing is for discovery; verbose entries waste turn-1 cache
/// tokens without improving the match rate.
const MAX_LISTING_DESC_CHARS: usize = 250;

/// Below this per-entry length, descriptions drop to names-only so a
/// tight budget still lists every skill name.
const MIN_DESC_LENGTH: usize = 20;

/// The char budget for the listing. 1% of the context window (tokens x
/// chars/token), or the default when the window is unknown.
fn char_budget(context_window_tokens: u32) -> usize {
    if context_window_tokens == 0 {
        return DEFAULT_CHAR_BUDGET;
    }
    // window x CHARS_PER_TOKEN x SKILL_BUDGET_CONTEXT_PERCENT == window / 25
    (context_window_tokens as usize * CHARS_PER_TOKEN as usize) / 100
}

/// The per-entry description: description plus when-to-use when present,
/// capped at MAX_LISTING_DESC_CHARS with a trailing ellipsis on overflow.
fn entry_description(desc: &str, when_to_use: Option<&str>) -> String {
    let combined = match when_to_use.filter(|w| !w.is_empty()) {
        Some(w) => format!("{desc} - {w}"),
        None => desc.to_string(),
    };
    let char_count = combined.chars().count();
    if char_count > MAX_LISTING_DESC_CHARS {
        let keep = MAX_LISTING_DESC_CHARS.saturating_sub(3);
        let truncated: String = combined.chars().take(keep).collect();
        format!("{truncated}...")
    } else {
        combined
    }
}

/// Format the model-invocable skill listing within a char budget. Three
/// tiers: full descriptions if they fit; descriptions truncated to a
/// uniform per-entry length if not; names-only when the budget is too
/// tight for even short descriptions. Empty when there are no skills.
///
/// The budget is a heuristic (byte length, not display width) — the
/// listing truncates rather than overflows, and skill descriptions are
/// overwhelmingly ASCII, so byte length is a safe proxy.
pub fn format_skill_listing(descriptors: &[SkillDescriptor], context_window_tokens: u32) -> String {
    if descriptors.is_empty() {
        return String::new();
    }
    let budget = char_budget(context_window_tokens);

    let full: Vec<String> = descriptors
        .iter()
        .map(|d| {
            format!(
                "- {}: {}",
                d.name,
                entry_description(&d.description, d.when_to_use.as_deref())
            )
        })
        .collect();
    let full_total: usize =
        full.iter().map(|s| s.len()).sum::<usize>() + full.len().saturating_sub(1);

    if full_total <= budget {
        return full.join("\n");
    }

    // Names + "- " + ": " + the separator: per-entry overhead beyond the
    // description text. Compute what is left for descriptions.
    let names_overhead: usize = descriptors.iter().map(|d| d.name.len() + 4).sum::<usize>()
        + descriptors.len().saturating_sub(1);
    let available = budget.saturating_sub(names_overhead);
    let max_desc = available / descriptors.len();

    if max_desc < MIN_DESC_LENGTH {
        // Extreme case: drop descriptions, keep names so every skill is
        // at least discoverable.
        return descriptors
            .iter()
            .map(|d| format!("- {}", d.name))
            .collect::<Vec<_>>()
            .join("\n");
    }

    descriptors
        .iter()
        .map(|d| {
            let desc = entry_description(&d.description, d.when_to_use.as_deref());
            let desc = if desc.chars().count() > max_desc {
                let keep = max_desc.saturating_sub(3);
                let truncated: String = desc.chars().take(keep).collect();
                format!("{truncated}...")
            } else {
                desc
            };
            format!("- {}: {}", d.name, desc)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

impl Runner {
    /// Append a skill-discovery listing attachment for this turn when no
    /// listing survives in the served view. The listing carries
    /// descriptions only (progressive disclosure); the Skill tool loads
    /// full bodies on demand. No-op when no registry is wired, when the
    /// registry has no model-invocable skills, or when a listing already
    /// survives in the view (first-turn announce, then skip until
    /// compaction folds it out and the scan naturally resets).
    pub(crate) async fn inject_skill_listing(&self, session: SessionId) -> Result<(), RunError> {
        let Some(registry) = &self.skill_registry else {
            return Ok(());
        };
        let view = self.store.current_view(session).await?;
        // Scan the projected (manifest-applied) view, not the raw log: a
        // Summarized listing folded by compaction drops out of the view,
        // so the scan naturally empties and the listing re-surfaces.
        let filtered = match view.manifest.as_ref() {
            Some(m) => projection::apply_manifest(&view.events, m, Some(self.store.backend())),
            None => view.events.clone(),
        };
        let already_listed = filtered
            .iter()
            .any(|e| matches!(e.kind, TurnEventKind::SkillListing { .. }));
        if already_listed {
            return Ok(());
        }
        let descriptors = registry.list_model_invocable();
        if descriptors.is_empty() {
            return Ok(());
        }
        let window = model_window::resolve_context_window(&self.config.model);
        let text = format_skill_listing(&descriptors, window);
        if text.is_empty() {
            return Ok(());
        }
        let bytes = text.len() as u32;
        self.store
            .append(new_event(
                session,
                TurnEventKind::SkillListing { text, bytes },
            ))
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn descriptor(name: &str, desc: &str, when: Option<&str>) -> SkillDescriptor {
        SkillDescriptor {
            name: name.to_string(),
            description: desc.to_string(),
            when_to_use: when.map(String::from),
            argument_hint: None,
            disable_model_invocation: false,
            user_invocable: true,
            body_token_estimate: 0,
            allowed_tools: Vec::new(),
        }
    }

    #[test]
    fn test_empty_returns_empty() {
        assert_eq!(format_skill_listing(&[], 200_000), "");
    }

    #[test]
    fn test_full_listing_within_budget() {
        let descs = vec![
            descriptor("commit", "commit changes", Some("after edits")),
            descriptor("review", "review a PR", None),
        ];
        let out = format_skill_listing(&descs, 200_000);
        assert!(
            out.contains("- commit: commit changes - after edits"),
            "{out}"
        );
        assert!(out.contains("- review: review a PR"), "{out}");
        assert_eq!(out.matches('\n').count(), 1, "two entries, one newline");
    }

    #[test]
    fn test_when_to_use_appended() {
        let out = format_skill_listing(
            &[descriptor("commit", "commit changes", Some("after edits"))],
            200_000,
        );
        assert!(
            out.contains("commit changes - after edits"),
            "when_to_use appended with dash: {out}"
        );
    }

    #[test]
    fn test_entry_cap_truncates() {
        let long = "x".repeat(300);
        let out = format_skill_listing(&[descriptor("big", &long, None)], 200_000);
        let entry = out.strip_prefix("- big: ").unwrap_or(&out);
        assert!(
            entry.chars().count() <= MAX_LISTING_DESC_CHARS,
            "entry within cap: {} > {MAX_LISTING_DESC_CHARS}",
            entry.chars().count()
        );
        assert!(entry.ends_with("..."), "truncated with ellipsis: {entry}");
    }

    #[test]
    fn test_truncates_under_tight_budget() {
        // window 1000 -> budget 40; names+overhead exceed it, so
        // descriptions drop to names-only and names survive.
        let long = "x".repeat(500);
        let descs = vec![
            descriptor("alpha", &long, None),
            descriptor("beta", &long, None),
        ];
        let out = format_skill_listing(&descs, 1_000);
        assert!(out.contains("- alpha"), "name survives: {out}");
        assert!(out.contains("- beta"), "name survives: {out}");
        assert!(
            !out.contains(": x"),
            "descriptions dropped under extreme budget: {out}"
        );
    }

    #[test]
    fn test_names_only_extreme_budget() {
        let descs = vec![
            descriptor("alpha", "do alpha thing", None),
            descriptor("beta", "do beta thing", None),
        ];
        // window 100 -> budget 4 -> names-only.
        let out = format_skill_listing(&descs, 100);
        assert!(out.contains("- alpha"), "{out}");
        assert!(out.contains("- beta"), "{out}");
        assert!(!out.contains(": do"), "descriptions dropped: {out}");
    }

    #[test]
    fn test_zero_window_default_budget() {
        // context_window 0 -> default 8000 char budget; a small set fits.
        let descs = vec![descriptor("commit", "commit changes", None)];
        let out = format_skill_listing(&descs, 0);
        assert!(out.contains("- commit: commit changes"), "{out}");
    }

    // ---- inject_skill_listing method tests ----
    //
    // These need a Runner, so they build one with a stub provider + a stub
    // registry, mirroring the memory_gates pattern. The formatter is covered
    // above; these cover the scan-skip + append-when-absent paths.

    use houyicoder_api::provider::stream_from_response;
    use houyicoder_api::skill::SkillError;
    use houyicoder_memory::InMemoryBackend;
    use houyicoder_protocol::llm::{
        CompletionResponse, LlmEvent, ModelCapabilities, OutputItem, ProviderError,
    };
    use houyicoder_resilience::Retry;
    use houyicoder_session::SessionStore;
    use std::sync::Arc;

    /// A stub provider: only capabilities() is read at construction.
    struct StubProvider;
    impl houyicoder_api::provider::ModelProvider for StubProvider {
        fn complete(
            &self,
            _req: houyicoder_protocol::llm::CompletionRequest,
        ) -> houyicoder_async::PFut<'_, Result<CompletionResponse, ProviderError>> {
            Box::pin(async {
                Ok(CompletionResponse {
                    output: vec![OutputItem::Text {
                        text: "done".into(),
                    }],
                    usage: houyicoder_protocol::llm::Usage::default(),
                    model: "test".into(),
                })
            })
        }
        fn stream(
            &self,
            _req: houyicoder_protocol::llm::CompletionRequest,
        ) -> houyicoder_async::PStream<'_, Result<LlmEvent, ProviderError>> {
            stream_from_response(CompletionResponse {
                output: vec![OutputItem::Text {
                    text: "done".into(),
                }],
                usage: houyicoder_protocol::llm::Usage::default(),
                model: "test".into(),
            })
        }
        fn capabilities(&self) -> ModelCapabilities {
            ModelCapabilities::default()
        }
    }

    /// A stub registry that returns one model-invocable skill.
    struct OneSkillRegistry;
    impl houyicoder_api::skill::SkillRegistry for OneSkillRegistry {
        fn list_model_invocable(&self) -> Vec<SkillDescriptor> {
            vec![descriptor("commit", "commit changes", None)]
        }
        fn find(&self, name: &str) -> Option<SkillDescriptor> {
            if name == "commit" {
                Some(descriptor("commit", "commit changes", None))
            } else {
                None
            }
        }
        fn prepare_body(
            &self,
            _name: &str,
            _args: Option<&str>,
            _session_id: Option<&str>,
        ) -> Result<String, SkillError> {
            Ok("body".into())
        }
    }

    fn runner_with_skills() -> (Runner, SessionId) {
        use houyicoder_context::SessionId;
        let store: Arc<dyn houyicoder_api::session::SessionLog> =
            Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
        let runner = Runner::with_shared_store(
            store,
            Arc::new(StubProvider),
            crate::agent::ToolRegistry::new(),
            crate::agent::runner_config::RunnerConfig {
                model: "test".into(),
                instructions: String::new(),
                max_turns: 5,
                max_output_tokens: 8_000,
                retry: Retry::default(),
            },
        )
        .with_skill_registry(Arc::new(OneSkillRegistry));
        let session = SessionId::new();
        (runner, session)
    }

    /// When no listing survives in the view, inject appends a SkillListing
    /// event the projection merges into the user message.
    #[tokio::test]
    async fn test_inject_appends_when_absent() {
        let (runner, session) = runner_with_skills();
        runner.inject_skill_listing(session).await.unwrap();
        let view = runner.store().current_view(session).await.unwrap();
        let listing = view.events.iter().find_map(|e| match &e.kind {
            TurnEventKind::SkillListing { text, .. } => Some(text.clone()),
            _ => None,
        });
        let text = listing.expect("a SkillListing event was appended");
        assert!(text.contains("- commit: commit changes"), "{text}");
    }

    /// When a listing already survives in the view, inject is a no-op: the
    /// event count does not grow. This is the dedup that makes repeat calls
    /// at compact re-drive sites safe.
    #[tokio::test]
    async fn test_inject_skips_when_present() {
        let (runner, session) = runner_with_skills();
        runner.inject_skill_listing(session).await.unwrap();
        let after_first = runner
            .store()
            .current_view(session)
            .await
            .unwrap()
            .events
            .len();
        runner.inject_skill_listing(session).await.unwrap();
        let after_second = runner
            .store()
            .current_view(session)
            .await
            .unwrap()
            .events
            .len();
        assert_eq!(
            after_first, after_second,
            "second inject is a no-op when a listing survives"
        );
    }

    /// When no registry is wired, inject is a no-op (no event appended).
    #[tokio::test]
    async fn test_inject_noop_without_registry() {
        use houyicoder_context::SessionId;
        let store: Arc<dyn houyicoder_api::session::SessionLog> =
            Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
        // No with_skill_registry: skill_registry is None.
        let runner = Runner::with_shared_store(
            store.clone(),
            Arc::new(StubProvider),
            crate::agent::ToolRegistry::new(),
            crate::agent::runner_config::RunnerConfig {
                model: "test".into(),
                instructions: String::new(),
                max_turns: 5,
                max_output_tokens: 8_000,
                retry: Retry::default(),
            },
        );
        let session = SessionId::new();
        runner.inject_skill_listing(session).await.unwrap();
        let view = store.current_view(session).await.unwrap();
        assert!(
            view.events
                .iter()
                .all(|e| !matches!(e.kind, TurnEventKind::SkillListing { .. })),
            "no listing appended without a registry"
        );
    }
}
