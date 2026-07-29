//! Manual + auto compaction with PreCompact/PostCompact hook fire. The
//! single entry point compact_internal unifies the manual /compact path and
//! the auto overflow path so both fire the same hooks and both run the
//! before-compact marker extraction. Split from append.rs / builder.rs so
//! each stays under the file-size gate.
//!
//! Auto-compact suppress: a deterministic compact failure raises a
//! reason-scoped suppress level so a transient blip only skips one turn
//! (self-heals at the next turn start) while a fatal cause stays suppressed
//! until a context-budget change clears it — preventing flapping. Manual
//! /compact bypasses suppress (the user asked). The overflow guard stays
//! fail-closed regardless; suppress only gates the proactive economy path +
//! the retry decision.
//!
//! Hook fire model: PreCompact fires before the summarizer and is
//! non-blocking — a hook cannot deny compaction (that would brick the
//! session on overflow). Its return channel is the Inject verdict: hook
//! output becomes custom summarization instructions merged into the
//! summarizer prompt. PostCompact fires after the summary commits, with the
//! summary text, and is non-blocking. The trigger (manual / auto) rides
//! both payloads so a hook can behave differently for a user-initiated
//! compact versus an automatic one.

use houyicoder_context::SessionId;

use super::hook::{CompactTrigger, HookContext, HookEvent, HookPayload, HookVerdict, arbitrate};
use super::lifecycle::{commit_manifest, extract_precompact_markers};
use super::manifest::{CompressPolicy, build_manifest, estimate_span_tokens};
use super::{RunError, Runner};
use houyicoder_context::Disposition;

/// After this many consecutive transient (Other-class) auto-compact failures,
/// the suppress promotes to Sticky so a persistently-failing transient cause
/// stops retrying every turn. A fatal cause (Schema/StillOver) is Sticky on
/// the first failure, so the streak only applies to the self-healing path.
const MAX_CONSECUTIVE_OTHER_FAILURES: u32 = 3;

/// The auto-compact suppression level. Stored as a u8 behind an atomic so
/// the pre-flight economy gate + the turn-start self-heal read/write it
/// lock-free. None is the steady state; the others are set by a
/// deterministic compact failure + cleared by the matching recovery.
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

/// Why auto-compact was suppressed. Maps a failure cause to a scope so a
/// transient blip does not sticky-block and a fatal cause does not
/// retry-pointlessly. The provider summarizer falls back to heuristic, so a
/// provider error rarely fails compact; the dominant failures are storage
/// I/O, a corrupt log, and a no-progress compact that is still over-window.
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

/// The outcome of a compaction, carried to the wire reply. Wraps the
/// CompressResult (manifest + folded count + made-progress flag) with the
/// pre/post token estimates captured around the summarizer call so the
/// reply can show the token drop.
pub struct CompactOutcome {
    pub made_progress: bool,
    pub folded_count: usize,
    pub manifest_id: houyicoder_context::CheckpointId,
    pub pre_compact_tokens: u64,
    pub post_compact_tokens: u64,
    /// Recall rate since the previous compaction: conversation_search matches
    /// that landed in the folded (Summarized) span, divided by this
    /// compaction's folded count. None when no recall was measured (no folded
    /// events, or no conversation_search fired since the last compaction). An
    /// instrumentation signal, not a correctness gate.
    pub recall_rate: Option<f64>,
    /// Conflict rate: file paths the LLM summary fabricated (mentioned a
    /// touched file that was never touched) over the backbone's ground-truth
    /// file-touch set. None when no summary was produced (nothing to merge).
    /// A free measurement under v1's add-only coexistence: the v2 signal for
    /// shrinking the LLM path to only the non-rederivable part.
    pub conflict_rate: Option<f64>,
}

impl Runner {
    /// Drive a compaction of the session: replay the event log, fire
    /// PreCompact, run before-compact marker extraction, build + commit a
    /// manifest with the (hook-injected) custom instructions, then fire
    /// PostCompact. The trigger labels the path (manual /compact or auto
    /// overflow). Returns the outcome for the wire reply; the served view
    /// picks up the manifest on the next turn's build_with_manifest.
    #[expect(clippy::too_many_lines, reason = "long by design, kept whole")]
    pub(crate) async fn compact_internal(
        &self,
        session: SessionId,
        trigger: CompactTrigger,
    ) -> Result<CompactOutcome, RunError> {
        let events = self.store.replay(session).await?;
        let pre_compact_tokens = estimate_span_tokens(&events) as u64;

        // 1. PreCompact: non-blocking. The return channel is Inject verdicts —
        //    hook output becomes custom summarization instructions. A Deny
        //    verdict is NOT honored for compact (denying compaction would
        //    brick the session on overflow); other verdicts are recorded as
        //    observations + always proceed.
        let custom_instructions = self
            .fire_pre_compact(session, trigger, events.len(), pre_compact_tokens as usize)
            .await;

        // 2. Build the manifest with the merged custom instructions. Built
        //    once here so marker extraction + commit share one manifest.
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

        // Snapshot the recall meter (swaps to 0 so the next interval starts
        // clean): how many conversation_search matches landed in the folded
        // span since the previous compaction. The rate normalizes by this
        // compaction's folded count so a folded-detail recall shows a
        // non-zero rate on the next report. None when nothing was folded or
        // no recall fired this interval.
        let recalls = self
            .recall_meter
            .swap(0, std::sync::atomic::Ordering::Relaxed);
        let recall_rate = if folded_count > 0 && recalls > 0 {
            Some(recalls as f64 / folded_count as f64)
        } else {
            None
        };

        // Re-derivable backbone (v1 add-only): derive the structured block
        // from the folded events + the workspace probe, then merge it after
        // the LLM summary. The merged summary replaces the manifest's summary
        // so the committed checkpoint + the served view carry both the LLM
        // narrative + the authoritative derived-from-log block. The conflict
        // rate measures LLM fabrications against the backbone's ground-truth
        // file set. None when no LLM summary was produced (nothing to merge).
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

        // 3. Before-compact marker extraction over the manifest's Summarized
        //    span: deterministic, no model. Saves unsolved-problem + key-
        //    decision markers to the auto scope so key facts survive the
        //    fold. Best-effort: a write failure logs and continues.
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

        // 4. Commit: write_checkpoint + CompactionBoundary + Summary events.
        let summary = manifest.summary.clone().unwrap_or_default();
        let manifest_id = manifest.id;
        let result = match commit_manifest(&*self.store, session, &manifest).await {
            Ok(r) => r,
            Err(e) => {
                // A deterministic compact failure suppresses the auto path by
                // reason (manual /compact bypasses: its failure must not
                // poison the auto gates). The replay error above propagates
                // as-is — only the commit failure classifies here.
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
        // A successful compact clears any prior suppress (the view shrank;
        // auto-compact may fire again) + resets the transient-failure streak.
        self.set_compact_suppress(super::compact::CompactSuppress::None);
        self.compact_consecutive_failures
            .store(0, std::sync::atomic::Ordering::Relaxed);
        // A compaction rewrites the served view from the manifest, so the
        // prior provider-reported input tokens are no longer a valid floor
        // for effective_served_tokens. Clear the stale observation so the
        // pre-flight / overflow gate reads the post-compact estimate, not
        // the pre-compact value (which would false-trip the gate on a view
        // that is now well under threshold).
        if let Ok(mut ol) = self.observability.lock() {
            ol.clear_last_turn_delta();
        }
        // A compact rewrites the provider-facing transcript, so the prior
        // cached prefix is no longer a cache baseline: bump the generation +
        // clear the per-block retention decisions so the next serve recomputes
        // against the new prefix.
        self.cached_prefix.invalidate();

        // 5. Post-compact token estimate: the verbatim tail + the summary.
        //    Best-effort: re-estimate the events the manifest keeps verbatim.
        let verbatim_ids: std::collections::HashSet<&houyicoder_context::EventId> = manifest
            .plan
            .iter()
            .filter(|g| g.disposition == Disposition::Verbatim)
            .flat_map(|g| g.event_ids.iter())
            .collect();
        let post_events: Vec<houyicoder_context::TurnEvent> = events
            .iter()
            .filter(|e| verbatim_ids.contains(&e.id))
            .cloned()
            .collect();
        let mut post_compact_tokens = estimate_span_tokens(&post_events) as u64;
        post_compact_tokens += estimate_span_tokens_summary(&summary);

        // 6. PostCompact: non-blocking, carries the summary text + the
        //    structured metrics. Observations recorded; no flow control.
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

/// Estimate the token footprint of a summary string. Reuses the shared
/// tokenizer so the post-compact estimate uses the same BPE as the
/// pre-compact estimate_span_tokens path; a summary is plain text, so a
/// direct Tokenizer::count over the string is the honest count (not a
/// chars/4 floor that over-counts CJK and under-counts code).
fn estimate_span_tokens_summary(summary: &str) -> u64 {
    if summary.is_empty() {
        return 0;
    }
    super::context::Tokenizer::new().count(summary) as u64
}

#[cfg(test)]
#[path = "compact_hook_tests.rs"]
mod compact_hook_tests;

#[cfg(test)]
#[path = "compact_suppress_tests.rs"]
mod compact_suppress_tests;
