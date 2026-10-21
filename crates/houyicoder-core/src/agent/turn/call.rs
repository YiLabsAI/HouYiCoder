//! Streaming model calls, request preflight, and context overflow recovery.
use futures::StreamExt;
use tokio_util::sync::CancellationToken;

use houyicoder_context::{SessionEvent, SessionId};
use houyicoder_protocol::frontend::model::SpeedMode;
use houyicoder_protocol::llm::{
    CompletionResponse, EffortLevel, ModelSettings, ProviderError, Usage,
};

use crate::agent::compaction::{CompactionSuppression, SuppressionCause};
use crate::agent::context::AssembledContext;
use crate::agent::context::turn_group::append_user_text;
use crate::agent::economy::{economy_decision, estimate_compaction};
use crate::agent::model_window::{
    conservative_input_tokens, fill_omitted_usage, record_learned_context_window,
};
use crate::agent::{RequestInferenceConfig, RunError, Runner, new_event, obs_wire};

use super::stream_fold;
use crate::agent::append::timing::{BlockSpans, is_first_output};
use crate::agent::apply_effort_settings;
use crate::agent::compaction::pre_flight_threshold;
use crate::agent::resolve_applied_effort;
use fold::{classify_stream_truncation, note_exhausted_truncation};
use std::time::Duration;
use std::time::Instant;
use stream_fold::StreamFold;
use tokio::time::sleep;

mod fold;
mod request;

/// Max gap between stream chunks before the run aborts the call as a stall
/// (dead socket / half-open gateway). On by default; without it a dead socket
/// hangs the run forever unless the user presses Esc. Tests use a tiny value
/// so the stall path runs instantly — a normal test stream emits chunks far faster than 50ms, so only a truly pending stream (no chunks at all) trips it.
#[cfg(not(test))]
fn stream_idle_timeout() -> Duration {
    Duration::from_secs(90)
}
#[cfg(test)]
fn stream_idle_timeout() -> Duration {
    Duration::from_millis(50)
}

/// What one logical turn's model calls returned, and what they billed.
///
/// A turn usually makes one call, but a reply cut at the output cap is
/// recovered by re-calling, so a turn can bill several. The usage here covers
/// every call the turn accounted for, which lets the drive loop fold a turn's
/// whole cost once instead of counting the recovery retries somewhere else.
pub(crate) struct TurnCall {
    /// The response the turn ended on, or None when the run or the turn was
    /// cancelled before one arrived.
    pub response: Option<CompletionResponse>,
    /// The tokens the turn's calls billed, or None for a turn that ended
    /// without recording a call. A cancelled turn leaves no usage record
    /// behind, so a zero here would state a measured cost nobody measured and
    /// would reset the window footprint the status bar reads.
    pub usage: Option<Usage>,
}

impl TurnCall {
    /// A turn that has not recorded a call yet.
    fn started() -> Self {
        Self {
            response: None,
            usage: None,
        }
    }

    /// Fold one call's usage into the turn's total, which starts at the turn's
    /// first recorded call.
    fn account(&mut self, usage: &Usage) {
        match &mut self.usage {
            Some(total) => accumulate_usage(total, usage),
            slot => *slot = Some(usage.clone()),
        }
    }
}

impl Runner {
    /// Freeze the inference config and assemble one request's context.
    ///
    /// The two steps belong together and in this order: the snapshot is where
    /// a model switch resets the cached-prefix decisions, so a request built
    /// before it would be assembled under the previous model's decisions.
    pub(crate) fn assemble_request(
        &self,
        events: &[houyicoder_context::SessionLogEntry],
        manifest: Option<&houyicoder_context::CheckpointManifest>,
    ) -> (RequestInferenceConfig, AssembledContext) {
        let inference = self.snapshot_inference();
        let parts = self.assembly_parts();
        let assembled = self.context_builder.build_for_turn(
            events,
            manifest,
            Some(self.store.backend()),
            &parts.input(),
        );
        (inference, assembled)
    }

    /// One streaming model call sequence: re-assemble the event log into input,
    /// build the request, drive the provider stream, and fold it into a
    /// CompletionResponse while appending each text delta to the session log and
    /// notifying the live sink. Retry wraps only stream establishment (a
    /// retryable error before the first real event retries the whole call); once
    /// an event lands the turn is committed — a mid-stream error is terminal;
    /// there is no partial-recovery path here yet.
    ///
    /// The result reports the turn's calls and their total usage; the caller
    /// folds that usage, not the response's, so a recovery retry reaches the
    /// run and session totals along with the response it ends on.
    ///
    /// Pre-flight (fail-closed): if the assembled context exceeds the absolute reserve
    /// (window minus the model response room and an estimation margin), compress
    /// before sending to the provider. Overflow handler: when
    /// the stream returns ContextOverflow, compress and retry (bounded 2). If
    /// compress makes no progress (all Verbatim), fail-closed. Still overflow
    /// after retries → fail-closed bounded (no infinite retry avalanche).
    #[expect(clippy::too_many_lines, reason = "long by design, kept whole")]
    #[expect(clippy::cognitive_complexity, reason = "inherent dispatch complexity")]
    pub(crate) async fn model_call_stream(
        &self,
        session: SessionId,
        turn: u32,
        max_turns: u32,
        token: &CancellationToken,
        turn_token: &CancellationToken,
    ) -> Result<TurnCall, RunError> {
        const MAX_OVERFLOW_RETRIES: u32 = 2;
        const CONVERGE_REMINDER_TURNS: u32 = 5;

        // Retries stay within this turn. The durable start marker preserves
        // its boundary even when the call fails before usage is recorded.
        obs_wire::start_turn(&self.observability);
        self.heal_turn_start_suppression();
        self.append_turn_started(session).await?;

        let mut overflow_retries = 0u32;
        // Proactive compaction gets one chance per turn; the bounded ceiling
        // handler owns any remaining overflow.
        let mut economy_fired_this_turn = false;
        // Keep a cut reply and continue from it; streamed text cannot be
        // retracted. Bound retries when the model repeatedly hits the cap.
        const MAX_LENGTH_RETRIES: u32 = 3;
        let mut length_retries = 0u32;
        let mut call = TurnCall::started();
        'outer: loop {
            let snapshot = self.store.current_view(session).await?;
            // A model switch must invalidate the cached prefix before context
            // assembly; the request and the context report use that same view.
            let (inference, mut assembled) =
                self.assemble_request(&snapshot.events, snapshot.manifest.as_ref());
            // Capture before assembled.messages moves into the request;
            // token_count() can't run after that. Fallback for
            // provider-omits-usage, passed to record_turn at turn end.
            let estimated_input_tokens = assembled.measurement.token_count();

            // Do not nudge the model before its first turn, even when the
            // configured turn limit is small.
            let remaining = max_turns.saturating_sub(turn);
            if turn > 1 && remaining <= CONVERGE_REMINDER_TURNS {
                let reminder = "If you have gathered enough information to \
                     answer, synthesize your findings and produce the final \
                     answer now. Prioritize answering over further exploration.";
                append_user_text(&mut assembled.messages, reminder);
            }

            // Pre-flight: an absolute reserve (response room, capped, +
            // estimation margin) beats a 95% ratio, which on a 200k window
            // left only 10k headroom. Fail closed so an oversized request
            // never reaches the provider. The window and the output cap come
            // frozen on the inference snapshot, so the gate, the request
            // body and the pane row read one value.
            let window = inference.context_window;
            if window > 0 {
                // The reserve uses the frozen output cap — the same value
                // the request body sends — so the room the gate reserves
                // matches the room the request asks for.
                let threshold = pre_flight_threshold(window, inference.max_output_tokens);
                // Floor the estimate to the last observed input tokens so a
                // tiktoken undercount on a non-native model cannot false-trip
                // the gate. The max is the conservative floor.
                let last_observed = self
                    .observability
                    .lock()
                    .ok()
                    .and_then(|ol| ol.last_turn_delta().map(|d| d.input));
                let conservative_input_tokens =
                    conservative_input_tokens(estimated_input_tokens, last_observed);
                // Economy gate: compact proactively when the remaining turns
                // make the rewrite + summarizer cost pay back in cache-read
                // savings. Runs before the ceiling so an expensive view
                // compacts early. Guarded by conservative_input_tokens >
                // window/2 so a small test window does not false-fire.
                let cost = self.cost_model.cost();
                if conservative_input_tokens > window / 2
                    && remaining > 0
                    && !economy_fired_this_turn
                    && self.compaction_suppression() == CompactionSuppression::None
                {
                    let estimate = estimate_compaction(conservative_input_tokens, remaining as u64);
                    let decision = economy_decision(estimate, &cost);
                    if decision.compact {
                        // Mark fired before the await so a re-entry after
                        // continue cannot re-fire (the compact itself is
                        // abortable; a cancel returns Ok(None) first).
                        economy_fired_this_turn = true;
                        let progress = tokio::select! {
                            biased;
                            _ = token.cancelled() => return Ok(call),
                            _ = turn_token.cancelled() => return Ok(call),
                            p = self.compress(session) => p?,
                        };
                        if progress {
                            self.memory.recall(session).await?;
                            // A compaction folded the listing out of the
                            // assembled context; re-announce so the model is
                            // not skill-blind for the rest of this run. No-op
                            // when a listing still survives (dedup scan).
                            self.inject_skill_listing_and_body(session).await?;
                            continue 'outer;
                        }
                    }
                }
                if conservative_input_tokens > threshold {
                    if overflow_retries >= MAX_OVERFLOW_RETRIES {
                        return Err(RunError::ContextOverflowBounded {
                            retries: overflow_retries,
                            enforced_limit: None,
                        });
                    }
                    let progress = tokio::select! {
                        biased;
                        // compress is another LLM call on the run chain —
                        // abortable like stream.next(), not a bare await. A
                        // provider stall here otherwise hangs the run past
                        // the sandbox fence with no Esc path.
                        _ = token.cancelled() => return Ok(call),
                        _ = turn_token.cancelled() => return Ok(call),
                        p = self.compress(session) => p?,
                    };
                    if !progress {
                        // All-Verbatim and still over ceiling: a re-loop
                        // would not shrink. Sticky-suppress so the next
                        // turn does not retry pointlessly.
                        self.set_compaction_suppression(
                            SuppressionCause::NoProgress.suppression_state(),
                        );
                        return Err(RunError::ContextOverflowNoProgress);
                    }
                    overflow_retries += 1;
                    // Compact folded older memory-recall events out of the
                    // assembled context, where the Summarized disposition
                    // drops them from the model's input. Re-inject so the
                    // model is not memory-blind for the rest of this run:
                    // the surfaced scan now sees the
                    // folded set as empty, so recall re-surfaces entries the
                    // model still needs for the remaining tool turns.
                    self.memory.recall(session).await?;
                    // Re-announce skills too: the compact folded the
                    // listing out, so the scan resets and it re-surfaces.
                    self.inject_skill_listing_and_body(session).await?;
                    continue 'outer;
                }
            }

            let active_model = inference.model.clone();
            let mut settings = ModelSettings {
                max_output_tokens: Some(inference.max_output_tokens),
                ..Default::default()
            };
            // Per-request effort, resolved from the frozen snapshot (not the
            // live state) + the catalog + per-model default, then lowered to
            // the dialect's fields. Reads no stale construction-time value.
            // The same resolved level lands on the durable TurnUsage, so each
            // turn is attributed to the effort that actually ran.
            let applied_effort = resolve_applied_effort(
                &active_model,
                inference.effort,
                self.catalog_resolver.as_deref(),
            );
            apply_effort_settings(&mut settings, &active_model, applied_effort);
            // Observability at the effort inflection point: /debug raises the
            // level to surface this. Trajectory (durable log) does not collect
            // tracing — this is the runtime-only view of what the request sends.
            tracing::debug!(
                model = %active_model,
                applied_effort = ?applied_effort,
                reasoning_effort = ?settings.reasoning_effort,
                enable_thinking = ?settings.enable_thinking,
                "effort resolved for request"
            );
            // Fast lowers to the provider's fast service tier on models that
            // declare one. The apply path clamps an unsupported Fast to
            // Standard, so a Fast snapshot value means the model accepts it.
            if inference.speed == SpeedMode::Fast {
                settings.speed = Some(SpeedMode::Fast);
            }
            let request = self.build_request(&active_model, assembled, settings);
            let provider = self.provider.clone();
            let response_handler = self.events.response_stream_handler();
            let max_attempts = self.config.retry.max_attempts.max(1);

            // Retry-to-first-event: a retryable transport error before any event
            // retries the stream call; a non-retryable error is fatal; an empty
            // stream is a completed (empty) turn. After the first event, no retry.
            let mut attempts = 0u32;
            // Wall-clock for this round-trip, measured per 'outer iteration
            // (fresh each retry so attempt 2/3 do not accumulate 1's time).
            // First provider.stream call → stream end (Finish event); covers
            // transport retries inside the first-event loop (without_retries
            // would split those off — deferred, passed 0).
            let api_start = Instant::now();
            tracing::debug!(
                attempt = attempts,
                overflow_retries,
                "start stream round-trip"
            );
            let mut stream = provider.stream(request.clone());
            tracing::debug!("stream opened");
            let first = loop {
                tokio::select! {
                    biased;
                    _ = token.cancelled() => return Ok(call),
                    _ = turn_token.cancelled() => return Ok(call),
                    item = stream.next() => match item {
                        Some(Ok(ev)) => {
                            tracing::debug!(elapsed = ?api_start.elapsed(), "first event received");
                            break Some(ev);
                        }
                        Some(Err(e)) if e.retryable() && attempts + 1 < max_attempts => {
                            attempts += 1;
                            // Exponential backoff with jitter; a server Retry-After
                            // (429) bypasses the backoff ceiling so a rate-limited
                            // account is polled after the server's window, not every
                            // max-delay tick.
                            let wait = self.config.retry.delay_for(attempts, e.retry_after_delay());
                            tracing::debug!(attempt = attempts, wait = ?wait, "retrying after error");
                            sleep(wait).await;
                            stream = provider.stream(request.clone());
                            continue;
                        }
                        // Overflow handler: compress and retry (bounded).
                        // ContextOverflow is not retryable, so without this
                        // branch it would hit the fatal path below and brick
                        // the session. When the provider's error body named
                        // the real enforced limit, record it for the model so
                        // the catalog self-corrects: the next resolution trusts
                        // the provider's enforced value over the static table.
                        Some(Err(ProviderError::ContextOverflow { enforced_limit })) => {
                            // Attribute the overflow to the model the request
                            // used (the frozen snapshot), not the live state:
                            // a mid-run switch must not record the old
                            // request's enforced limit onto the new model.
                            let active = inference.model.clone();
                            record_learned_context_window(
                                &active,
                                enforced_limit,
                            );
                            // The one moment we know the window estimate is
                            // wrong AND cannot self-heal: the provider rejected
                            // an over-long request but its error body carried no
                            // parseable limit, so the learner learned nothing.
                            // Surface it to the user as a system line pointing at
                            // the catalog override (zero LLM cost, just text).
                            if enforced_limit.is_none() {
                                self.emit_unactionable_overflow(&active);
                            }
                            if overflow_retries >= MAX_OVERFLOW_RETRIES {
                                return Err(RunError::ContextOverflowBounded {
                                    retries: overflow_retries,
                                    enforced_limit,
                                });
                            }
                            let progress = tokio::select! {
                                biased;
                                // compress is another LLM call on the run chain —
                                // abortable like stream.next(), not a bare await. A
                                // provider stall here otherwise hangs the run past
                                // the sandbox fence with no Esc path.
                                _ = token.cancelled() => return Ok(call),
                                _ = turn_token.cancelled() => return Ok(call),
                                p = self.compress(session) => p?,
                            };
                            if !progress {
                                self.set_compaction_suppression(
                                    SuppressionCause::NoProgress.suppression_state(),
                                );
                                return Err(RunError::ContextOverflowNoProgress);
                            }
                            overflow_retries += 1;
                            // Compaction folded older memory-recall events out
                            // of the context; re-inject so the model is not
                            // memory-blind for the rest of this run.
                            self.memory.recall(session).await?;
                            // Re-announce skills: the compact folded the
                            // listing out; the scan resets and re-surfaces it.
                            self.inject_skill_listing_and_body(session).await?;
                            continue 'outer;
                        }
                        Some(Err(e)) => return Err(RunError::ProviderFatal(e)),
                        None => break None,
                    },
                    _ = sleep(stream_idle_timeout()) => {
                        tracing::debug!(elapsed = ?api_start.elapsed(), "stall retry (no chunk for idle timeout)");
                        if attempts + 1 < max_attempts {
                            attempts += 1;
                            let wait = self.config.retry.delay_for(attempts, None);
                            sleep(wait).await;
                            stream = provider.stream(request.clone());
                            continue;
                        }
                        return Err(RunError::ProviderFatal(ProviderError::Network));
                    }
                }
            };

            let mut state = StreamFold::default();
            let mut first_token_elapsed: Option<u64> = None;
            let mut blocks = BlockSpans::default();
            // Process events as they arrive so response handlers and the log
            // observe each delta without waiting for stream completion.
            if let Some(ev) = first {
                blocks.note(&ev);
                if is_first_output(&ev) {
                    first_token_elapsed = Some(api_start.elapsed().as_millis() as u64);
                }
                self.fold_event(ev, &mut state, session, response_handler.as_deref())
                    .await?;
            }
            loop {
                let ev = tokio::select! {
                    biased;
                    _ = token.cancelled() => {
                        let response = state.clone().into_response(inference.model.clone());
                        self.append_response_events(session, &response).await?;
                        return Ok(call);
                    }
                    _ = turn_token.cancelled() => {
                        // Per-turn abort mid-stream: flush partial; drive loop appends marker + next turn.
                        let response = state.clone().into_response(inference.model.clone());
                        self.append_response_events(session, &response).await?;
                        return Ok(call);
                    }
                    ev = stream.next() => match ev {
                        Some(Ok(e)) => e,
                        Some(Err(e)) => return Err(RunError::ProviderFatal(e)),
                        None => break,
                    },
                    _ = sleep(stream_idle_timeout()) => {
                        // Stall mid-stream: flush the partial so text is not lost, then fail (no retry — would replay emitted events).
                        let response = state.clone().into_response(inference.model.clone());
                        self.append_response_events(session, &response).await?;
                        return Err(RunError::ProviderFatal(ProviderError::Network));
                    }
                };
                blocks.note(&ev);
                if first_token_elapsed.is_none() && is_first_output(&ev) {
                    first_token_elapsed = Some(api_start.elapsed().as_millis() as u64);
                }
                self.fold_event(ev, &mut state, session, response_handler.as_deref())
                    .await?;
            }
            let (reasoning_ms, response_ms) = (blocks.reasoning_ms(), blocks.response_ms());
            // Missing provider usage falls back to the request estimate.
            fill_omitted_usage(&mut state.usage, estimated_input_tokens);
            let (raw_finish_reason, self_count_output_tokens, truncation_signal) =
                classify_stream_truncation(&mut state, inference.max_output_tokens);
            // Withhold the visible truncation notice until recovery is
            // exhausted; the next request continues from the saved partial.
            if state.finish_reason.as_deref() == Some("length")
                && length_retries < MAX_LENGTH_RETRIES
            {
                length_retries += 1;
                self.store
                    .append(new_event(
                        session,
                        SessionEvent::TruncationVerdict {
                            raw_finish_reason,
                            normalized_reason: state.finish_reason.clone(),
                            signal: truncation_signal,
                            server_output_tokens: state.usage.output_tokens,
                            self_count_output_tokens,
                            max_output_tokens: inference.max_output_tokens,
                            recovery_attempts: length_retries,
                            recovery_fired: true,
                        },
                    ))
                    .await?;
                let partial = state.clone().into_response(inference.model.clone());
                self.append_response_events(session, &partial).await?;
                // Record the partial call's cost before the nudge (the nudge
                // is the next call's input; the usage is this call's). A
                // length-recovery retry burns real tokens — and each retry's
                // input grows, since the partial reply + nudge are appended
                // before the re-call. Without this the worst offenders (long
                // replies that retry 1-3x) are the least visible in /cost +
                // /trajectory, and the self-evolution loop can't mine
                // truncation-retry waste. recovery=true marks this as a retry
                // continuation so the cost is queryable per-call.
                obs_wire::record_turn(
                    &self.observability,
                    &partial.model,
                    &partial.usage,
                    estimated_input_tokens,
                    api_start.elapsed().as_millis() as u64,
                    window,
                    inference.max_output_tokens,
                );
                self.record_turn_cache(&partial.usage);
                // The cut call is part of the turn the re-call continues, so it
                // joins the turn's total here and reaches the run figure and
                // the session tally with it.
                call.account(&partial.usage);
                let total_dur_ms = api_start.elapsed().as_millis() as u64;
                let decode_dur_ms =
                    first_token_elapsed.map(|ttft| total_dur_ms.saturating_sub(ttft));
                if let Err(e) = self
                    .append_model_step_timing(
                        session,
                        total_dur_ms,
                        first_token_elapsed,
                        decode_dur_ms,
                        reasoning_ms,
                        response_ms,
                    )
                    .await
                {
                    tracing::debug!("timing append failed: {e}");
                }
                self.append_turn_usage(
                    session,
                    &partial.model,
                    &partial.usage,
                    true,
                    applied_effort.map(EffortLevel::label),
                )
                .await?;
                self.append_resume_nudge(session).await?;
                continue 'outer;
            }
            note_exhausted_truncation(&mut state);
            // Terminal verdict: covers both recovery-exhausted (normalized
            // reason is length, recovery_attempts at the cap) and clean
            // success (normalized reason is stop, recovery_attempts is however
            // many recoveries fired before the clean stop). The data
            // distinguishes the two cases — no separate emission needed.
            let normalized_reason = state.finish_reason.clone();
            let server_output_tokens = state.usage.output_tokens;
            let response = state.into_response(inference.model.clone());
            self.append_response_events(session, &response).await?;
            self.store
                .append(new_event(
                    session,
                    SessionEvent::TruncationVerdict {
                        raw_finish_reason,
                        normalized_reason,
                        signal: truncation_signal,
                        server_output_tokens,
                        self_count_output_tokens,
                        max_output_tokens: inference.max_output_tokens,
                        recovery_attempts: length_retries,
                        recovery_fired: false,
                    },
                ))
                .await?;
            // Record per-turn usage: response.usage (provider-measured,
            // primary) and estimated_input_tokens (local tiktoken, fallback
            // when the provider omits usage). Co-located with the
            // TruncationVerdict so a replay sees them adjacent.
            obs_wire::record_turn(
                &self.observability,
                &response.model,
                &response.usage,
                estimated_input_tokens,
                api_start.elapsed().as_millis() as u64,
                window,
                inference.max_output_tokens,
            );
            self.record_turn_cache(&response.usage);
            let total_dur_ms = api_start.elapsed().as_millis() as u64;
            let decode_dur_ms = first_token_elapsed.map(|ttft| total_dur_ms.saturating_sub(ttft));
            if let Err(e) = self
                .append_model_step_timing(
                    session,
                    total_dur_ms,
                    first_token_elapsed,
                    decode_dur_ms,
                    reasoning_ms,
                    response_ms,
                )
                .await
            {
                tracing::debug!("timing append failed: {e}");
            }
            self.append_turn_usage(
                session,
                &response.model,
                &response.usage,
                false,
                applied_effort.map(EffortLevel::label),
            )
            .await?;
            call.account(&response.usage);
            call.response = Some(response);
            return Ok(call);
        }
    }
}

/// Sum usage across turns. The shared saturating_add folds every field and
/// AND-merges provenance, so a run whose any turn went unreported reads
/// unknown rather than a partial rate.
pub(crate) fn accumulate_usage(total: &mut Usage, turn: &Usage) {
    *total = total.saturating_add(turn);
}
