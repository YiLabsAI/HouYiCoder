//! agent::call — the streaming model-call path with pre-flight and overflow.
//!
//! Extracted from the runner module to keep file sizes under the gate. The
//! model_call_stream method is the hot path: re-project the event log, build
//! the request, drive the provider stream, and fold events into a
//! CompletionResponse. Pre-flight (fail-closed at the absolute reserve —
/// ! window minus the model response room and an estimation margin) and ! overflow handling (compress → retry, bounded 2) live here so the ! session never bricks on ContextOverflow.
use futures::StreamExt;
use tokio_util::sync::CancellationToken;

use houyicoder_context::{SessionId, TruncationSignal, TurnEventKind};
use houyicoder_protocol::llm::{
    CompletionRequest, CompletionResponse, InputItem, ModelSettings, OutputItem, ProviderError,
};
use houyicoder_protocol::llm::{LlmEvent, Usage};

use super::{RunError, Runner, new_event, obs_wire};
use houyicoder_api::live::{LiveEvent, LiveSink};

#[path = "stream_fold.rs"]
mod stream_fold;
use stream_fold::StreamFold;

/// Max gap between stream chunks before the run aborts the call as a stall
/// (dead socket / half-open gateway). On by default; without it a dead socket
/// hangs the run forever unless the user presses Esc. Tests use a tiny value
/// so the stall path runs instantly — a normal test stream emits chunks far faster than 50ms, so only a truly pending stream (no chunks at all) trips it.
#[cfg(not(test))]
fn stream_idle_timeout() -> std::time::Duration {
    std::time::Duration::from_secs(90)
}
#[cfg(test)]
fn stream_idle_timeout() -> std::time::Duration {
    std::time::Duration::from_millis(50)
}

/// Pre-flight compress threshold: the context window minus the room the model
/// needs to respond (capped so a huge max_output does not erase the buffer)
/// minus an estimation margin for tiktoken drift on non-tiktoken-native
/// models. Saturates to 0 for windows smaller than the reserve (tiny-window
/// stubs trip compress immediately, which is correct — there is no room to
/// serve anything).
///
/// An absolute buffer beats a 95% ratio: on a 200k window the ratio left
/// only 10k headroom, too thin for a model that needs 8-16k to respond. The
/// absolute reserves real output room and scales correctly to 1M-class
/// windows (the buffer derives from the resolved window; a 1M-capable model
/// whose limit is mis-resolved to 200k still anchors a 200k buffer — that is
/// a window-resolution gap, not a miscompute in this formula).
fn pre_flight_threshold(window: u32, max_output_tokens: u32) -> u32 {
    const MAX_OUTPUT_RESERVE_CAP: u32 = 20_000;
    const ESTIMATION_MARGIN: u32 = 13_000;
    let reserve = max_output_tokens.min(MAX_OUTPUT_RESERVE_CAP) + ESTIMATION_MARGIN;
    window.saturating_sub(reserve)
}

#[cfg(test)]
#[path = "pre_flight_threshold_tests.rs"]
mod pre_flight_threshold_tests;

/// One streaming model call: re-project the event log into input, build the
/// request, drive the provider stream, and fold it into a CompletionResponse
/// while appending each text delta to the session log and notifying the live
/// sink. Retry wraps only stream establishment (a retryable error before the
/// first real event retries the whole call); once an event lands the turn is
/// committed — a mid-stream error is terminal; there is no partial-recovery
/// path here yet.
///
/// Pre-flight (fail-closed): if the served view exceeds the absolute reserve
/// (window minus the model response room and an estimation margin), compress
/// before sending to the provider. Overflow handler: when
/// the stream returns ContextOverflow, compress and retry (bounded 2). If
/// compress makes no progress (all Verbatim), fail-closed. Still overflow
/// after retries → fail-closed bounded (no infinite retry avalanche).
#[expect(clippy::too_many_lines, reason = "long by design, kept whole")]
impl Runner {
    #[expect(clippy::cognitive_complexity, reason = "inherent dispatch complexity")]
    pub(crate) async fn model_call_stream(
        &self,
        session: SessionId,
        turn: u32,
        max_turns: u32,
        token: &CancellationToken,
    ) -> Result<Option<CompletionResponse>, RunError> {
        const MAX_OVERFLOW_RETRIES: u32 = 2;
        const CONVERGE_REMINDER_TURNS: u32 = 5;

        // One model_call_stream entry = one drive-loop iteration = one logical
        // turn. Start the turn here so length/overflow retries inside 'outer
        // stay on the same turn number (the turn boundary is this entry).
        // Append a durable TurnStarted boundary alongside the in-memory
        // start_turn so the projection groups turns on a marker that exists
        // even for
        // cancelled / errored turns (which never reach a TurnUsage).
        obs_wire::start_turn(&self.observability);
        self.heal_turn_start_suppress();
        self.append_turn_started(session).await?;

        let mut overflow_retries = 0u32;
        // Economy gate fires at most once per turn: a proactive compact that
        // does not bring the view under the ceiling would otherwise re-fire
        // every 'outer re-entry (turn is a caller param, not incremented
        // here), looping. The ceiling gate is bounded by overflow_retries;
        // the economy gate is bounded by this flag (one proactive compact,
        // then the ceiling owns further recovery).
        let mut economy_fired_this_turn = false;
        // Length-recovery: a finish_reason "length" cut at the token cap is
        // recovered by appending the partial assistant message + a resume
        // nudge, then re-calling so the model continues. Bounded so a model
        // that always over-runs cannot loop forever. Live streaming UX has
        // no clean retract for an already-shown partial, so this path skips
        // the discard-and-retry and continues from the partial — which also
        // yields more total room across the N continuations than one big cap.
        const MAX_LENGTH_RETRIES: u32 = 3;
        let mut length_retries = 0u32;
        'outer: loop {
            let snapshot = self.store.current_view(session).await?;
            let tool_defs = self.tools.tool_defs();
            // Format the MEMORY.md index from the memory provider so it sits
            // in the byte-stable cache prefix. None when no provider is wired
            // (tests, stub). Capped at 200 entries.
            let memory_index = self.format_memory_index();
            let mut served = self.context_builder.build_with_manifest(
                &snapshot.events,
                snapshot.manifest.as_ref(),
                Some(self.store.backend()),
                &tool_defs,
                memory_index.as_deref(),
            );
            // Capture the served token count up front: served.messages is
            // moved into the request below, so token_count() (a &self method)
            // can't run after that. The count is the provider-omits-usage
            // fallback for context_pct, passed to record_turn at turn end.
            let served_tokens = served.token_count();

            // Convergence reminder: when the turn count is within the
            // reminder window of the hard cap, inject a user message that
            // tells the model to synthesize and answer. Without this, an
            // analysis task (read-heavy, no single definitive result) can
            // loop tool calls until MaxTurnsReached, producing no answer.
            // The turn>1 guard skips the very first call: reminding before
            // the model has acted once is absurd, and it keeps tiny max_turns
            // configs (tests, short subagent forks) from firing the reminder
            // on turn 1 and polluting the served view.
            let remaining = max_turns.saturating_sub(turn);
            if turn > 1 && remaining <= CONVERGE_REMINDER_TURNS {
                // Convergence nudge: encourage the model to synthesize if it
                // has enough information, without revealing the turn budget
                // (budget pressure is a control-plane concern; injecting it
                // into the model input causes premature convergence).
                let reminder = "If you have gathered enough information to \
                     answer, synthesize your findings and produce the final \
                     answer now. Prioritize answering over further exploration.";
                served.messages.push(InputItem::User {
                    content: reminder.into(),
                });
            }

            // Pre-flight: served tokens exceed the absolute reserve (model
            // response room, capped, + estimation margin) → compress first,
            // fail-closed so an oversized request never reaches the provider.
            // See pre_flight_threshold: an absolute buffer beats a 95% ratio,
            // which on a 200k window left only 10k headroom — too thin for a
            // model that needs 8-16k to respond. The window resolves from the
            // active model id ([1m] suffix / per-provider catalog / conservative
            // default), not the provider's static capabilities, so a
            // long-context model gets its real window and an unknown model
            // never over-reports.
            let caps = super::model_window::resolve_capabilities(
                &self.active_model(),
                self.provider.capabilities(),
            );
            let window = caps.context_window;
            if window > 0 {
                // The pre-flight reserve uses the same max_output_tokens the
                // request body sends (catalog override else config), not the
                // provider caps' static max_output — so the room the gate
                // reserves matches the room the request asks for.
                let threshold = pre_flight_threshold(window, self.resolve_max_output_tokens());
                // Floor the served estimate to the last observed input tokens
                // so a tiktoken undercount on a non-native model cannot
                // false-trip the gate. The observed is the provider's ground
                // truth for the prefix; the estimate covers messages added
                // since. The max is the conservative floor.
                let last_observed = self
                    .observability
                    .lock()
                    .ok()
                    .and_then(|ol| ol.last_turn_delta().map(|d| d.input));
                let served_tokens =
                    super::model_window::effective_served_tokens(served_tokens, last_observed);
                // Economy gate (cost-saving): compact proactively when the
                // remaining turns make the rewrite + summarizer cost pay back
                // in cache-read savings over the horizon. Runs BEFORE the
                // ceiling (overflow-guard) so a view that is expensive to
                // compacts early. Guarded by served_tokens > window/2 so a
                // small test window does not false-fire; the decision layer
                // also skips on NoShrink/NoHorizon/BelowBreakeven. No-progress
                // here is not an error (the view may already be compacted):
                // fall through to the ceiling check.
                let cost = self.cost_model.cost();
                if served_tokens > window / 2
                    && remaining > 0
                    && !economy_fired_this_turn
                    && self.compact_suppress() == super::compact::CompactSuppress::None
                {
                    let projection =
                        super::economy::economy_projection(served_tokens, remaining as u64);
                    let decision = super::economy::economy_decision(projection, &cost);
                    if decision.compact {
                        // Mark fired before the await so a re-entry after
                        // continue cannot re-fire (the compact itself is
                        // abortable; a cancel returns Ok(None) first).
                        economy_fired_this_turn = true;
                        let progress = tokio::select! {
                            _ = token.cancelled() => return Ok(None),
                            p = self.compress(session) => p?,
                        };
                        if progress {
                            self.inject_memory_recall(session).await?;
                            // A compaction folded the listing out of the
                            // served view; re-announce so the model is not
                            // skill-blind for the rest of this run. No-op
                            // when a listing still survives (dedup scan).
                            self.inject_skill_listing(session).await?;
                            continue 'outer;
                        }
                    }
                }
                if served_tokens > threshold {
                    if overflow_retries >= MAX_OVERFLOW_RETRIES {
                        return Err(RunError::ContextOverflowBounded {
                            retries: overflow_retries,
                            enforced_limit: None,
                        });
                    }
                    let progress = tokio::select! {
                        // compress is another LLM call on the run chain —
                        // abortable like stream.next(), not a bare await. A
                        // provider stall here otherwise hangs the run past
                        // the sandbox fence with no Esc path.
                        _ = token.cancelled() => return Ok(None),
                        p = self.compress(session) => p?,
                    };
                    if !progress {
                        // The compact made no progress (all-Verbatim) AND the
                        // view is still over the ceiling — a re-loop would not
                        // shrink. Sticky-suppress the auto path so the next
                        // turn does not retry pointlessly; the overflow guard
                        // stays fail-closed here.
                        self.set_compact_suppress(
                            super::compact::SuppressReason::StillOver.suppress_state(),
                        );
                        return Err(RunError::ContextOverflowNoProgress);
                    }
                    overflow_retries += 1;
                    // Compact folded older memory-recall events out of the
                    // served view (Summarized disposition drops them from the
                    // projection). Re-inject so the model is not memory-blind
                    // for the rest of this run: the surfaced scan now sees the
                    // folded set as empty, so recall re-surfaces entries the
                    // model still needs for the remaining tool turns.
                    self.inject_memory_recall(session).await?;
                    // Re-announce skills too: the compact folded the
                    // listing out, so the scan resets and it re-surfaces.
                    self.inject_skill_listing(session).await?;
                    continue 'outer;
                }
            }

            // Append the configured instructions to the served system prompt
            // (do not replace it) so the static identity/framework prefix
            // stays byte-stable for prompt-cache. An empty configured
            // instructions field (the production path) uses the served system
            // verbatim. Custom text lands at the end; the default prompt is
            // kept.
            let instructions = assemble_instructions(&served.system, &self.config.instructions);
            let active_model = self.active_model();
            let mut settings = ModelSettings {
                max_output_tokens: Some(self.resolve_max_output_tokens()),
                ..Default::default()
            };
            // Per-request effort, resolved fresh each call from the active pick
            // + the catalog + per-model default, then lowered to the dialect's
            // fields. Reads no stale construction-time value (I4) and no env.
            super::apply_effort_settings(
                &mut settings,
                &active_model,
                self.resolve_applied_effort(),
            );
            let mut request = CompletionRequest {
                model: super::model_window::normalize_model_for_api(&active_model),
                instructions,
                input: served.messages,
                tools: self.tools.tool_defs(),
                settings,
                cache_breakpoints: Vec::new(),
            };
            // Apply the cache policy: place prompt-cache breakpoints so the
            // provider can carve a stable prefix. The Auto three-breakpoint set
            // (system static prefix, last tool def, latest user message) is the
            // default; the provider lowers each kind to its wire format.
            let policy = self.cache_policy.policy();
            self.cache_policy
                .apply(&mut request.cache_breakpoints, &policy);
            let provider = self.provider.clone();
            let live = self.live.clone();
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
            let api_start = std::time::Instant::now();
            tracing::debug!(
                attempt = attempts,
                overflow_retries,
                "start stream round-trip"
            );
            let mut stream = provider.stream(request.clone());
            tracing::debug!("stream opened");
            let first = loop {
                tokio::select! {
                    _ = token.cancelled() => {
                        tracing::debug!("cancelled by esc");
                        return Ok(None);
                    }
                    _ = tokio::time::sleep(stream_idle_timeout()) => {
                        tracing::debug!(elapsed = ?api_start.elapsed(), "stall retry (no chunk for idle timeout)");
                        if attempts + 1 < max_attempts {
                            attempts += 1;
                            let wait = self.config.retry.delay_for(attempts, None);
                            tokio::time::sleep(wait).await;
                            stream = provider.stream(request.clone());
                            continue;
                        }
                        return Err(RunError::ProviderFatal(ProviderError::Network));
                    }
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
                            tokio::time::sleep(wait).await;
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
                            let active = self.active_model();
                            super::model_window::record_learned_context_window(
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
                        // compress is another LLM call on the run chain —
                        // abortable like stream.next(), not a bare await. A
                        // provider stall here otherwise hangs the run past
                        // the sandbox fence with no Esc path.
                        _ = token.cancelled() => return Ok(None),
                        p = self.compress(session) => p?,
                    };
                            if !progress {
                                self.set_compact_suppress(
                                    super::compact::SuppressReason::StillOver.suppress_state(),
                                );
                                return Err(RunError::ContextOverflowNoProgress);
                            }
                            overflow_retries += 1;
                            // Compact folded older memory-recall events out
                            // of the served view; re-inject so the model is
                            // not memory-blind for the rest of this run.
                            self.inject_memory_recall(session).await?;
                            // Re-announce skills: the compact folded the
                            // listing out; the scan resets and re-surfaces it.
                            self.inject_skill_listing(session).await?;
                            continue 'outer;
                        }
                        Some(Err(e)) => return Err(RunError::ProviderFatal(e)),
                        None => break None,
                    },
                }
            };

            let mut state = StreamFold::default();
            // Process events INLINE as they arrive — not buffered. The live sink
            // must fire during the stream (so the host sees tokens form in real
            // time) and each delta must hit the durable log as it lands. Buffering
            // the whole stream first would defeat the streaming UX (the host would
            // see a spinner for the whole turn, then a burst).
            if let Some(ev) = first {
                self.fold_event(ev, &mut state, session, &live).await?;
            }
            loop {
                let ev = tokio::select! {
                    _ = token.cancelled() => {
                        let response = state.clone().into_response(self.config.model.clone());
                        self.append_response_events(session, &response).await?;
                        return Ok(None);
                    }
                    _ = tokio::time::sleep(stream_idle_timeout()) => {
                        // Stall mid-stream: flush the partial response so the
                        // turn's text is not lost, then fail. No retry here
                        // (a retry would replay already-emitted events).
                        let response = state.clone().into_response(self.config.model.clone());
                        self.append_response_events(session, &response).await?;
                        return Err(RunError::ProviderFatal(ProviderError::Network));
                    }
                    ev = stream.next() => match ev {
                        Some(Ok(e)) => e,
                        Some(Err(e)) => return Err(RunError::ProviderFatal(e)),
                        None => break,
                    },
                };
                self.fold_event(ev, &mut state, session, &live).await?;
            }
            // Provider-omits-usage fallback: some OpenAI-compat streams
            // ignore stream_options.include_usage; substitute the served
            // estimate so the status gauge + tally read the real footprint.
            super::model_window::fill_omitted_usage(&mut state.usage, served_tokens);
            // Capture the raw provider finish_reason BEFORE dialect
            // normalization so the verdict carries the original dialect
            // (max_tokens / MAX_TOKENS / length / stop). Without this the
            // normalize below flattens the dialect to "length" and the raw
            // spelling the gateway used is lost to trajectory analysis.
            let raw_finish_reason = state.finish_reason.clone();
            // Normalize provider dialects of the cap-cut finish reason before
            // any comparison: OpenAI-compatible gateways say "length", but an
            // Anthropic-shaped reply says "max_tokens" and Gemini says
            // "MAX_TOKENS". The recovery gate below compares against "length"
            // exactly, so an unnormalized alias silently disabled recovery —
            // the reply rendered cut mid-sentence with no notice (bug-log #29).
            if let Some(reason) = &state.finish_reason
                && reason != "length"
                && is_length_reason(reason)
            {
                state.finish_reason = Some("length".into());
            }
            // Silent-truncation heuristic: trusts the provider finish_reason
            // exclusively and recovers only on "length" or the context-window
            // signal. A proxy that cuts the stream at the token
            // cap but signals "stop" passes through as a complete reply, and a
            // cut mid-code-block leaves an open fence the caller cannot
            // distinguish from a finished one. Synthesize "length" when the
            // provider claimed a natural stop but the output looks cut, so the
            // existing resume loop picks it up. Two signals, either suffices:
            // 1. output reached the token cap — output_tokens within SLACK of
            //    the cap. The server-reported usage is accurate when present;
            //    many streaming proxies omit it, so a cheap self-count estimate
            //    (the shared Tokenizer) is the fallback — catching a proxy
            //    that honored max_tokens but mislabeled the stop.
            // 2. an unclosed code block — an odd triple-backtick count means a
            //    fence opened but never closed, so the cut landed mid-block.
            //
            // Classified once into a TruncationSignal the verdict records, so
            // the heuristic is not re-run for the verdict. When the provider
            // already signaled the cap-cut, the heuristic is skipped (nothing
            // to synthesize) and the signal stays None.
            let self_count_output_tokens = super::Tokenizer::new().count(&state.assistant_text);
            let mut truncation_signal = TruncationSignal::None;
            if state.finish_reason.as_deref() != Some("length") {
                truncation_signal = classify_truncation_signal(
                    &state.assistant_text,
                    &state.usage,
                    self_count_output_tokens,
                    self.config.max_output_tokens,
                );
                if truncation_signal != TruncationSignal::None {
                    state.finish_reason = Some("length".into());
                }
            }
            // Length-recovery: the provider cut the reply at the max_tokens
            // cap (finish_reason "length"). Withhold the truncation notice
            // while recovery can still succeed — append the partial assistant
            // message + a resume-direct nudge, then re-call so the model
            // picks up mid-thought. The nudge is a MetaUser event (served to
            // the model, hidden from the readable transcript). Bounded by
            // MAX_LENGTH_RETRIES; on exhaustion a visible notice lands.
            if state.finish_reason.as_deref() == Some("length")
                && length_retries < MAX_LENGTH_RETRIES
            {
                length_retries += 1;
                self.store
                    .append(new_event(
                        session,
                        TurnEventKind::TruncationVerdict {
                            raw_finish_reason,
                            normalized_reason: state.finish_reason.clone(),
                            signal: truncation_signal,
                            server_output_tokens: state.usage.output_tokens,
                            self_count_output_tokens,
                            max_output_tokens: self.config.max_output_tokens,
                            recovery_attempts: length_retries,
                            recovery_fired: true,
                        },
                    ))
                    .await?;
                let partial = state.clone().into_response(self.config.model.clone());
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
                    served_tokens,
                    api_start.elapsed().as_millis() as u64,
                    caps.context_window,
                    self.resolve_max_output_tokens(),
                );
                self.record_turn_cache(&partial.usage);
                self.append_turn_usage(session, &partial.model, &partial.usage, true, None)
                    .await?;
                self.append_resume_nudge(session).await?;
                continue 'outer;
            }
            if state.finish_reason.as_deref() == Some("length") {
                // Recovery exhausted: surface the cut so an empty/truncated
                // reply is not a silent mystery.
                if state.assistant_text.is_empty() {
                    state.assistant_text = if state.reasoning.is_empty() {
                        "*(output truncated at the token cap — no reply produced after recovery)*"
                            .to_string()
                    } else {
                        "*(the model reasoned but produced no answer before the token cap — recovery exhausted)*"
                            .to_string()
                    };
                } else {
                    state
                        .assistant_text
                        .push_str("\n\n*…output truncated at the token cap — recovery exhausted.*");
                }
            }
            // Terminal verdict: covers both recovery-exhausted (normalized
            // reason is length, recovery_attempts at the cap) and clean
            // success (normalized reason is stop, recovery_attempts is however
            // many recoveries fired before the clean stop). The data
            // distinguishes the two cases — no separate emission needed.
            let normalized_reason = state.finish_reason.clone();
            let server_output_tokens = state.usage.output_tokens;
            let response = state.into_response(self.config.model.clone());
            self.append_response_events(session, &response).await?;
            self.store
                .append(new_event(
                    session,
                    TurnEventKind::TruncationVerdict {
                        raw_finish_reason,
                        normalized_reason,
                        signal: truncation_signal,
                        server_output_tokens,
                        self_count_output_tokens,
                        max_output_tokens: self.config.max_output_tokens,
                        recovery_attempts: length_retries,
                        recovery_fired: false,
                    },
                ))
                .await?;
            // Record the per-turn usage now that both values live in this
            // scope: response.usage (provider-measured, primary) and
            // served_tokens (local tiktoken, fallback when the provider
            // omits usage). The durable TurnUsage event + the in-memory OL
            // delta fire together, once per model call that returns to the
            // drive loop. Co-located with the TruncationVerdict (the other
            // per-turn durable metadata) so a replay sees them adjacent.
            obs_wire::record_turn(
                &self.observability,
                &response.model,
                &response.usage,
                served_tokens,
                api_start.elapsed().as_millis() as u64,
                caps.context_window,
                self.resolve_max_output_tokens(),
            );
            self.record_turn_cache(&response.usage);
            self.append_turn_usage(session, &response.model, &response.usage, false, None)
                .await?;
            return Ok(Some(response));
        }
    }

    /// Append the resume-direct nudge as a persisted MetaUser event so the
    /// next serve picks it up and the model continues the cut reply. The
    /// nudge: no apology, no recap, pick up mid-thought, break remaining
    /// work into smaller pieces. Served to the model, hidden from the
    /// readable transcript.
    async fn append_resume_nudge(&self, session: SessionId) -> Result<(), RunError> {
        const NUDGE: &str = "Output token limit hit. Resume directly — no apology, no recap of what you were doing. Pick up mid-thought if that is where the cut happened. Break remaining work into smaller pieces.";
        self.store
            .append(new_event(
                session,
                houyicoder_context::TurnEventKind::MetaUser {
                    text: NUDGE.to_string(),
                },
            ))
            .await?;
        Ok(())
    }

    /// Fold one streamed LlmEvent into the turn state: append a text delta to
    /// the durable log + notify the live sink + accumulate; collect reasoning,
    /// tool calls, and usage. A ProviderError event is terminal for the turn.
    /// Boundary events (TextStart/End, ToolInput*, Step*) are ignored —
    /// tool-input streaming is not surfaced here yet.
    async fn fold_event(
        &self,
        ev: LlmEvent,
        state: &mut StreamFold,
        session: SessionId,
        live: &Option<LiveSink>,
    ) -> Result<(), RunError> {
        match ev {
            LlmEvent::TextDelta { text, .. } => {
                if let Some(sink) = live {
                    sink(&LiveEvent::AssistantDelta { text: text.clone() });
                }
                self.store
                    .append(new_event(
                        session,
                        TurnEventKind::AssistantTextDelta { text: text.clone() },
                    ))
                    .await?;
                state.assistant_text.push_str(&text);
            }
            LlmEvent::ReasoningDelta { text, .. } => {
                if let Some(sink) = live {
                    sink(&LiveEvent::ReasoningDelta { text: text.clone() });
                }
                state.reasoning.push(text);
            }
            LlmEvent::ToolCall { id, name, input } => {
                state
                    .tool_calls
                    .push(OutputItem::ToolCall { id, name, input });
            }
            LlmEvent::Finish { reason, usage: u } => {
                if let Some(u) = u {
                    state.usage = u;
                }
                // Capture the provider's finish reason so the drive loop can
                // react: a "length" finish means the reply was cut at the
                // max_tokens cap mid-generation, and the loop continues with a
                // resume-direct nudge (recovery). The marker surfacing lives
                // in the drive loop, withheld while recovery can still
                // succeed — not here, where it would fire on every length hit
                // even when recovery is about to produce a complete reply.
                state.finish_reason = Some(reason);
            }
            LlmEvent::ProviderError { .. } => {
                return Err(RunError::ProviderFatal(ProviderError::Unknown(
                    "provider error event mid-stream".into(),
                )));
            }
            LlmEvent::TextStart { .. }
            | LlmEvent::TextEnd { .. }
            | LlmEvent::ReasoningStart { .. }
            | LlmEvent::ReasoningEnd { .. }
            | LlmEvent::ToolInputStart { .. }
            | LlmEvent::ToolInputDelta { .. }
            | LlmEvent::ToolInputEnd { .. }
            | LlmEvent::ToolResult { .. }
            | LlmEvent::ToolError { .. }
            | LlmEvent::StepStart { .. }
            | LlmEvent::StepFinish { .. } => {}
        }
        Ok(())
    }
}

/// Whether a provider finish reason means the reply was cut at the output
/// token cap. Providers spell it differently: OpenAI-compatible endpoints
/// say length, Anthropic-shaped replies say max_tokens, Gemini says
/// MAX_TOKENS (with variants like max_output_tokens seen from gateways).
/// Matched case-insensitively so the recovery loop keys on the meaning, not
/// one provider's spelling.
fn is_length_reason(reason: &str) -> bool {
    matches!(
        reason.to_ascii_lowercase().as_str(),
        "length" | "max_tokens" | "max_output_tokens" | "model_length"
    )
}

/// Which silent-truncation signal fired for the folded turn state, or None
/// when no signal fired. Computed once so the drive loop both synthesizes
/// the cap-cut finish_reason and records the cause in the verdict without
/// re-running the heuristic. Priority: server-reported near-cap (most
/// reliable) > self-counted near-cap (fallback when the proxy omits usage) >
/// unclosed code block. The server count is checked only when the server
/// reported one — matches the original heuristic's prefer-server behavior.
fn classify_truncation_signal(
    assistant_text: &str,
    usage: &Usage,
    self_count: u32,
    max_output_tokens: u32,
) -> TruncationSignal {
    const SILENT_TRUNCATION_SLACK: u32 = 64;
    let cap = max_output_tokens.saturating_sub(SILENT_TRUNCATION_SLACK);
    if usage.output_tokens != 0 {
        if usage.output_tokens >= cap {
            return TruncationSignal::ServerUsageNearCap;
        }
    } else if self_count >= cap {
        return TruncationSignal::SelfCountNearCap;
    }
    if !assistant_text.is_empty() && count_triple_backticks(assistant_text) % 2 == 1 {
        return TruncationSignal::UnclosedCodeBlock;
    }
    TruncationSignal::None
}

/// Count triple-backtick occurrences in a string. An odd count means an open
/// code fence never closed — a signal the stream was cut mid-block. Used by
/// the silent-truncation heuristic to catch a proxy that cut the reply but
/// signaled a natural stop.
fn count_triple_backticks(s: &str) -> usize {
    let mut count = 0usize;
    let bytes = s.as_bytes();
    let mut i = 0usize;
    while i + 2 < bytes.len() {
        if bytes[i] == b'`' && bytes[i + 1] == b'`' && bytes[i + 2] == b'`' {
            count += 1;
            i += 3;
        } else {
            i += 1;
        }
    }
    count
}

/// Sum usage across turns (inclusive totals add up; breakdown fields add too).
pub(crate) fn accumulate_usage(total: &mut Usage, turn: &Usage) {
    total.input_tokens += turn.input_tokens;
    total.output_tokens += turn.output_tokens;
    total.total_tokens += turn.total_tokens;
    total.non_cached_input_tokens += turn.non_cached_input_tokens;
    total.cache_read_input_tokens += turn.cache_read_input_tokens;
    total.cache_write_input_tokens += turn.cache_write_input_tokens;
    total.reasoning_tokens += turn.reasoning_tokens;
}

/// Assemble the provider-facing instruction string: the served system prompt
/// with the configured instructions appended. An empty configured field
/// returns the served system verbatim (the production path). Appending keeps
/// the static identity/framework prefix byte-stable so the prompt-cache
/// prefix survives; replacing would discard the served prompt + thrash the
/// cache on every config change.
fn assemble_instructions(served_system: &str, configured: &str) -> String {
    if configured.is_empty() {
        served_system.to_string()
    } else {
        format!("{served_system}\n\n{configured}")
    }
}

#[cfg(test)]
#[path = "assemble_instructions_tests.rs"]
mod assemble_instructions_tests;
