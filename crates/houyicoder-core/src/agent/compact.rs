//! Manual and automatic session compaction.
//!
//! Both paths share hooks, marker extraction, manifest persistence, and
//! provider-facing measurements. Hooks cannot block compaction. Automatic
//! retries use reason-scoped suppression; manual requests bypass it.

use houyicoder_context::SessionId;

use super::hook::{CompactTrigger, HookContext, HookEvent, HookPayload, HookVerdict, arbitrate};
use super::lifecycle::{commit_manifest, extract_precompact_markers};
use super::manifest::{CompressPolicy, build_manifest, estimate_transcript_tokens};
use super::selection;
use super::{RunError, Runner};
use houyicoder_context::Disposition;

/// Consecutive transient failures become sticky to prevent retry churn.
const MAX_CONSECUTIVE_OTHER_FAILURES: u32 = 3;

/// Lock-free suppression state for automatic compaction retries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum CompactSuppress {
    /// Not suppressed — the steady state. Auto-compact may fire.
    None = 0,
    /// Resolvable failure: suppressed for the current turn, cleared at the
    /// next turn start so compaction self-heals once the cause clears.
    Turn = 1,
    /// Fatal failure retrying cannot fix: survives turn boundaries, cleared
    /// only when the context budget changes (a model switch to a larger
    /// window).
    Sticky = 2,
}

impl CompactSuppress {
    pub fn as_u8(self) -> u8 {
        self as u8
    }
    /// Decode the raw u8; an unknown value (a removed/future level) reads as
    /// None so a stale value never bricks auto-compact.
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Turn,
            2 => Self::Sticky,
            _ => Self::None,
        }
    }
    /// True when the level self-heals at the next turn start (only Turn).
    pub fn clears_at_turn_start(self) -> bool {
        matches!(self, Self::Turn)
    }
}

/// Failure category controlling automatic retry scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuppressReason {
    /// Storage I/O (transient): optimistic per-turn retry.
    Other,
    /// Corrupt log / hash-chain break (retrying cannot fix): sticky.
    Schema,
    /// A compact that made no progress (all-Verbatim) AND the view is still
    /// over the threshold (a re-loop would not shrink): sticky.
    StillOver,
}

impl SuppressReason {
    pub fn suppress_state(self) -> CompactSuppress {
        match self {
            Self::Other => CompactSuppress::Turn,
            Self::Schema | Self::StillOver => CompactSuppress::Sticky,
        }
    }
}

impl Runner {
    /// The current auto-compact suppression level. None in the steady state.
    pub fn compact_suppress(&self) -> CompactSuppress {
        CompactSuppress::from_u8(
            self.compact_suppress
                .load(std::sync::atomic::Ordering::Relaxed),
        )
    }
    /// Set the suppress level (the auto gates read it; manual /compact
    /// bypasses). A successful compact clears it to None.
    pub(crate) fn set_compact_suppress(&self, level: CompactSuppress) {
        self.compact_suppress
            .store(level.as_u8(), std::sync::atomic::Ordering::Relaxed);
    }
    /// Clear a sticky suppress when the context budget changes
    /// (a model switch to a larger window). Turn-level is left to the
    /// turn-start self-heal.
    pub fn clear_sticky_compact_suppress(&self) {
        let prev = self.compact_suppress();
        if !matches!(prev, CompactSuppress::None | CompactSuppress::Turn) {
            self.set_compact_suppress(CompactSuppress::None);
        }
        // A context-budget change is a fresh start: a prior transient streak
        // no longer applies under the new window.
        self.compact_consecutive_failures
            .store(0, std::sync::atomic::Ordering::Relaxed);
    }
    /// Record an auto-compact failure by reason + set the matching suppress
    /// level. A fatal cause (Schema/StillOver) is Sticky on the first failure;
    /// a transient cause (Other) increments the streak + promotes to Sticky
    /// after MAX_CONSECUTIVE_OTHER_FAILURES so a persistently-failing
    /// transient cause stops hammering a doomed compact each turn. Manual
    /// /compact does not call this (it bypasses suppress).
    pub(crate) fn record_compact_failure(&self, reason: SuppressReason) {
        let level = match reason {
            SuppressReason::Schema | SuppressReason::StillOver => CompactSuppress::Sticky,
            SuppressReason::Other => {
                let prev = self
                    .compact_consecutive_failures
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if prev + 1 >= MAX_CONSECUTIVE_OTHER_FAILURES {
                    CompactSuppress::Sticky
                } else {
                    CompactSuppress::Turn
                }
            }
        };
        self.set_compact_suppress(level);
    }
    /// Turn-start self-heal: a Turn-level suppress (a transient failure last
    /// turn) clears so auto-compact retries this turn. Sticky survives
    /// (it clears only on a context-budget change).
    pub(crate) fn heal_turn_start_suppress(&self) {
        if self.compact_suppress().clears_at_turn_start() {
            self.set_compact_suppress(CompactSuppress::None);
        }
    }
}

/// Compaction result and selected-transcript token estimates.
pub struct CompactOutcome {
    pub made_progress: bool,
    pub folded_count: usize,
    pub manifest_id: houyicoder_context::CheckpointId,
    pub pre_compact_tokens: u64,
    pub post_compact_tokens: u64,
    /// Folded-span recalls divided by events folded in this compaction.
    pub recall_rate: Option<f64>,
    /// Summary-only file paths divided by the backbone's touched-file set.
    pub conflict_rate: Option<f64>,
}

impl Runner {
    /// Compact a session through the shared manual and automatic pipeline.
    #[expect(clippy::too_many_lines, reason = "long by design, kept whole")]
    pub(crate) async fn compact_internal(
        &self,
        session: SessionId,
        trigger: CompactTrigger,
    ) -> Result<CompactOutcome, RunError> {
        let current = self.store.current_view(session).await?;
        let pre_compact_tokens =
            estimate_selected_transcript(&current.events, current.manifest.as_ref());
        let events = current.events;

        // Hooks may steer summarization but cannot block overflow recovery.
        let custom_instructions = self
            .fire_pre_compact(session, trigger, events.len(), pre_compact_tokens as usize)
            .await;

        let policy = CompressPolicy::default();
        let mut manifest = build_manifest(
            &events,
            &policy,
            self.summarizer.as_ref(),
            custom_instructions.as_deref(),
        )
        .await;
        let folded_count = manifest
            .plan
            .iter()
            .filter(|g| g.disposition == Disposition::Summarized)
            .map(|g| g.event_ids.len())
            .sum::<usize>();

        // Swapping starts a fresh recall interval for the next compaction.
        let recalls = self
            .recall_meter
            .swap(0, std::sync::atomic::Ordering::Relaxed);
        let recall_rate = if folded_count > 0 && recalls > 0 {
            Some(recalls as f64 / folded_count as f64)
        } else {
            None
        };

        // The deterministic backbone preserves facts independently of the LLM
        // summary and exposes fabricated file references as a metric.
        let folded_ids: std::collections::HashSet<houyicoder_context::EventId> = manifest
            .plan
            .iter()
            .filter(|g| g.disposition == Disposition::Summarized)
            .flat_map(|g| g.event_ids.iter().cloned())
            .collect();
        let backbone =
            super::backbone::derive_backbone(&events, &folded_ids, self.workspace_probe.as_deref());
        let conflict_rate = match &manifest.summary {
            Some(llm_summary) => {
                let (merged, conflict) = super::backbone::merge_summary(llm_summary, &backbone);
                manifest.summary = Some(merged);
                Some(conflict.rate)
            }
            None => None,
        };

        // Marker persistence is best-effort; failure must not block compaction.
        if let Some(memory) = &self.memory {
            let existing: std::collections::HashSet<String> =
                memory.list_memories().into_iter().map(|s| s.key).collect();
            for entry in extract_precompact_markers(&events, &manifest) {
                if existing.contains(&entry.key) {
                    continue;
                }
                if let Err(e) = memory.add(entry) {
                    tracing::warn!("before-compact marker write failed: {e}");
                }
            }
        }

        let summary = manifest.summary.clone().unwrap_or_default();
        let manifest_id = manifest.id;
        let result = match commit_manifest(&*self.store, session, &manifest).await {
            Ok(r) => r,
            Err(e) => {
                // Manual failures must not suppress later automatic recovery.
                if trigger == CompactTrigger::Auto {
                    let reason = match &e {
                        houyicoder_context::ContextError::Corrupt(_) => {
                            super::compact::SuppressReason::Schema
                        }
                        _ => super::compact::SuppressReason::Other,
                    };
                    self.record_compact_failure(reason);
                }
                return Err(RunError::from(e));
            }
        };
        self.set_compact_suppress(super::compact::CompactSuppress::None);
        self.compact_consecutive_failures
            .store(0, std::sync::atomic::Ordering::Relaxed);
        // The pre-compact provider count cannot floor the rebuilt view.
        if let Ok(mut ol) = self.observability.lock() {
            ol.clear_last_turn_delta();
        }
        // The rebuilt transcript invalidates the prior cache baseline.
        self.cached_prefix.invalidate();

        let post_compact_tokens = estimate_selected_transcript(&events, Some(&manifest));

        let compression_ratio = if pre_compact_tokens > 0 {
            post_compact_tokens as f64 / pre_compact_tokens as f64
        } else {
            1.0
        };
        self.fire_post_compact(
            session,
            trigger,
            manifest_id,
            result.folded_count,
            compression_ratio,
            &summary,
        )
        .await;

        Ok(CompactOutcome {
            made_progress: result.made_progress,
            folded_count,
            manifest_id,
            pre_compact_tokens,
            post_compact_tokens,
            recall_rate,
            conflict_rate,
        })
    }

    /// Fire PreCompact hooks and return the merged custom instructions (the
    /// return channel). Inject verdict outputs are joined into one string;
    /// all other verdicts are recorded as observations via append_hook_signals
    /// and do not block. None when no hook produced Inject content.
    async fn fire_pre_compact(
        &self,
        session: SessionId,
        trigger: CompactTrigger,
        event_count: usize,
        token_estimate: usize,
    ) -> Option<String> {
        let reg = self.hooks.as_ref()?;
        let ctx = HookContext {
            event: HookEvent::PreCompact,
            payload: HookPayload::PreCompact {
                trigger,
                pre_compact_event_count: event_count,
                pre_compact_token_estimate: token_estimate,
            },
            session,
        };
        let outcomes = self.dispatch_hooks(reg, &ctx);
        self.append_hook_signals(session, HookEvent::PreCompact, None, &outcomes)
            .await;
        // Arbitrate records triggers + collects the composite verdict for the
        // durable signal. The Inject reasons are extracted directly from the
        // per-hook outcomes so every Inject contribution is captured, not just
        // the primary.
        let _verdict = arbitrate(outcomes.iter().map(|o| o.result.clone()));
        let injects: Vec<String> = outcomes
            .iter()
            .filter_map(|o| match &o.result {
                Ok(HookVerdict::Inject(content)) => Some(content.clone()),
                _ => None,
            })
            .filter(|s| !s.is_empty())
            .collect();
        if injects.is_empty() {
            None
        } else {
            Some(injects.join("\n\n"))
        }
    }

    /// Fire PostCompact hooks after the summary commits. Non-blocking: the
    /// verdict is recorded for audit but does not affect flow (compaction
    /// already happened). Carries the summary text + structured metrics.
    async fn fire_post_compact(
        &self,
        session: SessionId,
        trigger: CompactTrigger,
        checkpoint_id: houyicoder_context::CheckpointId,
        folded_turns: usize,
        compression_ratio: f64,
        compact_summary: &str,
    ) {
        let Some(reg) = self.hooks.as_ref() else {
            return;
        };
        let ctx = HookContext {
            event: HookEvent::PostCompact,
            payload: HookPayload::PostCompact {
                trigger,
                checkpoint_id,
                folded_turns,
                compression_ratio,
                compact_summary: compact_summary.to_string(),
            },
            session,
        };
        let outcomes = self.dispatch_hooks(reg, &ctx);
        self.append_hook_signals(session, HookEvent::PostCompact, None, &outcomes)
            .await;
        let _verdict = arbitrate(outcomes.into_iter().map(|o| o.result));
    }
}

fn estimate_selected_transcript(
    events: &[houyicoder_context::SessionLogEntry],
    manifest: Option<&houyicoder_context::CheckpointManifest>,
) -> u64 {
    let selected = match manifest {
        Some(manifest) => selection::apply_manifest(events, manifest, None),
        None => events.to_vec(),
    };
    estimate_transcript_tokens(&selected)
}

#[cfg(test)]
#[path = "compact_hook_tests.rs"]
mod compact_hook_tests;

#[cfg(test)]
#[path = "compact_suppress_tests.rs"]
mod compact_suppress_tests;
