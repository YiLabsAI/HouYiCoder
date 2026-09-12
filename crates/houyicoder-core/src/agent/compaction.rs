//! Manual and automatic session compaction.
//!
//! Both paths share hooks, preservation, manifest persistence, and
//! provider-facing measurements. Hooks cannot block compaction. Automatic
//! retries use reason-scoped suppression; manual requests bypass it.

mod recording;
mod summarization;

pub use recording::RecordedCompaction;
use recording::record_compaction;
pub use summarization::LlmSummarizer;

use std::collections::HashSet;
use std::sync::atomic::Ordering;

use houyicoder_context::{
    CheckpointId, CheckpointManifest, ContextError, Disposition, EventId, SessionId,
    SessionLogEntry,
};

use super::backbone::{derive_backbone, merge_summary};
use super::hook::{CompactTrigger, HookContext, HookEvent, HookPayload, HookVerdict, arbitrate};
use super::manifest::{CompressPolicy, build_manifest, estimate_transcript_tokens};
use super::selection;
use super::{RunError, Runner};

/// Consecutive transient failures become sticky to prevent retry churn.
const MAX_CONSECUTIVE_TRANSIENT_FAILURES: u32 = 3;

/// Lock-free suppression state for automatic compaction retries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum CompactionSuppression {
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

impl CompactionSuppression {
    pub(crate) fn as_u8(self) -> u8 {
        self as u8
    }
    /// Decode the raw value. Unknown levels map to None so stale state cannot
    /// disable automatic compaction.
    pub(crate) fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Turn,
            2 => Self::Sticky,
            _ => Self::None,
        }
    }
    /// True when the level self-heals at the next turn start (only Turn).
    pub(crate) fn clears_at_turn_start(self) -> bool {
        matches!(self, Self::Turn)
    }
}

/// Failure category controlling automatic retry scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SuppressionCause {
    /// A failure that may resolve before the next turn.
    Transient,
    /// Corruption that retrying cannot repair.
    CorruptLog,
    /// A compaction that cannot reduce the overflowing context.
    NoProgress,
}

impl SuppressionCause {
    pub(crate) fn suppression_state(self) -> CompactionSuppression {
        match self {
            Self::Transient => CompactionSuppression::Turn,
            Self::CorruptLog | Self::NoProgress => CompactionSuppression::Sticky,
        }
    }
}

impl Runner {
    /// The current auto-compact suppression level. None in the steady state.
    pub(crate) fn compaction_suppression(&self) -> CompactionSuppression {
        CompactionSuppression::from_u8(self.compaction_suppression.load(Ordering::Relaxed))
    }
    /// Set the suppress level (the auto gates read it; manual /compact
    /// bypasses). A successful compact clears it to None.
    pub(crate) fn set_compaction_suppression(&self, level: CompactionSuppression) {
        self.compaction_suppression
            .store(level.as_u8(), Ordering::Relaxed);
    }
    /// Clear a sticky suppress when the context budget changes
    /// (a model switch to a larger window). Turn-level is left to the
    /// turn-start self-heal.
    pub(crate) fn clear_sticky_compaction_suppression(&self) {
        let prev = self.compaction_suppression();
        if !matches!(
            prev,
            CompactionSuppression::None | CompactionSuppression::Turn
        ) {
            self.set_compaction_suppression(CompactionSuppression::None);
        }
        // A context-budget change is a fresh start: a prior transient streak
        // no longer applies under the new window.
        self.compaction_transient_failures
            .store(0, Ordering::Relaxed);
    }
    /// Apply retry suppression for an automatic compaction failure.
    /// Transient failures become sticky after repeated retries; terminal
    /// causes become sticky immediately. Manual compaction bypasses this.
    pub(crate) fn record_compaction_failure(&self, reason: SuppressionCause) {
        let level = match reason {
            SuppressionCause::CorruptLog | SuppressionCause::NoProgress => {
                CompactionSuppression::Sticky
            }
            SuppressionCause::Transient => {
                let prev = self
                    .compaction_transient_failures
                    .fetch_add(1, Ordering::Relaxed);
                if prev + 1 >= MAX_CONSECUTIVE_TRANSIENT_FAILURES {
                    CompactionSuppression::Sticky
                } else {
                    CompactionSuppression::Turn
                }
            }
        };
        self.set_compaction_suppression(level);
    }
    /// Turn-start self-heal: a Turn-level suppress (a transient failure last
    /// turn) clears so auto-compact retries this turn. Sticky survives
    /// (it clears only on a context-budget change).
    pub(crate) fn heal_turn_start_suppression(&self) {
        if self.compaction_suppression().clears_at_turn_start() {
            self.set_compaction_suppression(CompactionSuppression::None);
        }
    }
}

/// Compaction result and selected-transcript token estimates.
pub struct CompactionOutcome {
    pub made_progress: bool,
    pub folded_count: usize,
    pub manifest_id: CheckpointId,
    pub pre_compact_tokens: u64,
    pub post_compact_tokens: u64,
    /// Folded-span recalls divided by events folded in this compaction.
    pub recall_rate: Option<f64>,
    /// Summary-only file paths divided by the backbone's touched-file set.
    pub conflict_rate: Option<f64>,
}

impl Runner {
    /// Compact a session through the shared manual and automatic pipeline.
    pub(crate) async fn run_compaction(
        &self,
        session: SessionId,
        trigger: CompactTrigger,
    ) -> Result<CompactionOutcome, RunError> {
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
        let folded_count = folded_event_count(&manifest);
        let recall_rate = self.take_recall_rate(folded_count);
        let conflict_rate = self.merge_backbone_summary(&events, &mut manifest);
        self.preserve_compacted_memory(&events, &manifest);

        let summary = manifest.summary.clone().unwrap_or_default();
        let manifest_id = manifest.id;
        let result = match record_compaction(&*self.store, session, &manifest).await {
            Ok(r) => r,
            Err(e) => {
                // Manual failures must not suppress later automatic recovery.
                if trigger == CompactTrigger::Auto {
                    let reason = match &e {
                        ContextError::Corrupt(_) => SuppressionCause::CorruptLog,
                        _ => SuppressionCause::Transient,
                    };
                    self.record_compaction_failure(reason);
                }
                return Err(RunError::from(e));
            }
        };
        self.set_compaction_suppression(CompactionSuppression::None);
        self.compaction_transient_failures
            .store(0, Ordering::Relaxed);
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

        Ok(CompactionOutcome {
            made_progress: result.made_progress,
            folded_count,
            manifest_id,
            pre_compact_tokens,
            post_compact_tokens,
            recall_rate,
            conflict_rate,
        })
    }

    fn take_recall_rate(&self, folded_count: usize) -> Option<f64> {
        let recalls = self.recall_meter.swap(0, Ordering::Relaxed);
        (folded_count > 0 && recalls > 0).then(|| recalls as f64 / folded_count as f64)
    }

    fn merge_backbone_summary(
        &self,
        events: &[SessionLogEntry],
        manifest: &mut CheckpointManifest,
    ) -> Option<f64> {
        let summary = manifest.summary.take()?;
        let folded_ids: HashSet<EventId> = manifest
            .plan
            .iter()
            .filter(|group| group.disposition == Disposition::Summarized)
            .flat_map(|group| group.event_ids.iter().copied())
            .collect();
        let backbone = derive_backbone(events, &folded_ids, self.workspace_probe.as_deref());
        let (merged, conflict) = merge_summary(&summary, &backbone);
        manifest.summary = Some(merged);
        Some(conflict.rate)
    }

    fn preserve_compacted_memory(&self, events: &[SessionLogEntry], manifest: &CheckpointManifest) {
        self.memory.preserve_folded(events, manifest);
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

    /// Fire PostCompact hooks after the summary records. Non-blocking: the
    /// verdict is recorded for audit but does not affect flow (compaction
    /// already happened). Carries the summary text + structured metrics.
    async fn fire_post_compact(
        &self,
        session: SessionId,
        trigger: CompactTrigger,
        checkpoint_id: CheckpointId,
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

fn folded_event_count(manifest: &CheckpointManifest) -> usize {
    manifest
        .plan
        .iter()
        .filter(|group| group.disposition == Disposition::Summarized)
        .map(|group| group.event_ids.len())
        .sum()
}

fn estimate_selected_transcript(
    events: &[SessionLogEntry],
    manifest: Option<&CheckpointManifest>,
) -> u64 {
    let selected = match manifest {
        Some(manifest) => selection::apply_manifest(events, manifest, None),
        None => events.to_vec(),
    };
    estimate_transcript_tokens(&selected)
}

#[cfg(test)]
#[path = "compaction_hook_tests.rs"]
mod compaction_hook_tests;

#[cfg(test)]
#[path = "suppression_tests.rs"]
mod suppression_tests;
