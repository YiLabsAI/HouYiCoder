//! The session-wide trajectory summary, folded once per durable event.
//!
//! The trajectory header and the status pane report figures about every turn
//! the session ran, while the pane loads only a window of turns, so those
//! figures cannot come from the window. This accumulator folds each event as
//! the store takes it, which keeps a read to a copy of small numbers rather
//! than a scan of the log.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use houyicoder_api::session::{
    ApprovalAudit, RecapSpend, SubagentUsage, TrajectorySummary, TrajectoryTimingSummary,
    TrajectoryUsageSummary, TurnAudit,
};
use houyicoder_context::{PermissionVerdict, RunCompletionStatus, SessionEvent, SessionLogEntry};
use houyicoder_protocol::frontend::run::ApprovalScope;
use houyicoder_protocol::llm::UsageProvenance;
use houyicoder_protocol::tool::{simple_command_word, tool_result_failed, tool_result_not_run};
use serde_json::Value;

use self::ttft::TtftHistogram;

mod ttft;

/// What a tool call carried, so a later result can be judged against it.
///
/// Only the command word is kept: the failure rule reduces a command to its
/// first word anyway, and a call can carry a heredoc or an inline script whose
/// body would cost far more to hold than the decision it serves.
struct ToolCallFacts {
    tool: String,
    command_word: Option<String>,
}

/// How many approval calls the fold remembers while counting each card and
/// each verdict once. A resumed turn re-asks a call right after the records
/// of the turn it was interrupted in, so a recent window covers every
/// duplicate a replay can produce; the bound keeps a long session's memory
/// flat. A session that draws more cards than the window holds and then
/// re-asks one from before it counts that card twice; reaching that needs a
/// session long enough to evict the record and a reconnect onto the old card.
const APPROVAL_CALLS_CAP: usize = 512;

/// The scope spelling older logs wrote for what the wire now calls a
/// workspace grant. Read when counting only; nothing writes it.
const LEGACY_ALWAYS_SCOPE: &str = "always";

/// Which of one call's approval records were counted. A reconnect re-emits
/// an open card and records that ask again, while a resume after a restart
/// asks the same call anew; without this ledger the same card or verdict
/// would be counted twice on the second path.
#[derive(Default)]
struct ApprovalCall {
    asked: bool,
    answered: bool,
}

/// The approval half of the summary: how many cards the session drew, how
/// each was answered, and the ledger that keeps a replay from counting one
/// card twice.
///
/// Cards and answers are counted apart so a card with no verdict stays an
/// unknown outcome rather than being read as a denial.
#[derive(Default)]
struct ApprovalFold {
    cards: usize,
    answers: usize,
    approved_once: usize,
    approved_session: usize,
    approved_workspace: usize,
    denied: usize,
    calls: HashMap<String, ApprovalCall>,
    order: VecDeque<String>,
}

impl ApprovalFold {
    /// Count one ask unless this call was already counted. A reconnect
    /// re-emits an open card and records that ask again, so the second one
    /// adds nothing.
    fn record_ask(&mut self, call_id: &str) {
        let entry = self.touch(call_id);
        if entry.asked {
            return;
        }
        entry.asked = true;
        self.cards = self.cards.saturating_add(1);
    }

    /// Fold one verdict unless this call's verdict was already counted.
    ///
    /// A verdict whose call has no recorded ask still counts that call as a
    /// card: the answer proves the card was shown, and a log written before
    /// asks were recorded holds exactly this shape. Counting it here keeps
    /// the card total equal to the distinct calls the log answers for, so an
    /// unanswered card elsewhere in the same log is still visible.
    fn record_verdict(&mut self, call_id: &str, verdict: PermissionVerdict, scope: &str) {
        let (first, had_ask) = {
            let entry = self.touch(call_id);
            let first = !entry.answered;
            let had_ask = entry.asked;
            entry.answered = true;
            (first, had_ask)
        };
        if !first {
            return;
        }
        if !had_ask {
            self.cards = self.cards.saturating_add(1);
        }
        self.answers = self.answers.saturating_add(1);
        if verdict == PermissionVerdict::Denied {
            self.denied = self.denied.saturating_add(1);
        } else if scope == ApprovalScope::Workspace.as_str() || scope == LEGACY_ALWAYS_SCOPE {
            // Older logs spelled the workspace grant "always". The words
            // differ but the grant does not, so the count lands where a
            // current log would put it; leaving it to fall through would
            // report every one of those answers as a once-only consent.
            self.approved_workspace = self.approved_workspace.saturating_add(1);
        } else if scope == ApprovalScope::Session.as_str() {
            self.approved_session = self.approved_session.saturating_add(1);
        } else {
            // The scope names the grant the answer actually bought, not the
            // one the client asked for. A spelling this build does not know
            // counts as the shortest-lived scope rather than reporting a
            // consent nobody granted.
            self.approved_once = self.approved_once.saturating_add(1);
        }
    }

    /// The ledger entry for one call, recording the call and releasing the
    /// oldest entry once the ledger is at its cap.
    fn touch(&mut self, call_id: &str) -> &mut ApprovalCall {
        if !self.calls.contains_key(call_id) {
            if self.calls.len() >= APPROVAL_CALLS_CAP
                && let Some(oldest) = self.order.pop_front()
            {
                self.calls.remove(&oldest);
            }
            self.order.push_back(call_id.to_string());
            self.calls
                .insert(call_id.to_string(), ApprovalCall::default());
        }
        // The insert above ran when the entry was absent and nothing removes
        // it in between, so the lookup cannot miss.
        self.calls
            .get_mut(call_id)
            .expect("ledger entry inserted when absent")
    }

    /// The audit as the summary reports it. Cards counts the distinct calls
    /// that were asked about or answered, so the cards without an answer are
    /// exactly the outcomes nobody recorded.
    fn audit(&self) -> ApprovalAudit {
        ApprovalAudit {
            cards: self.cards,
            approved_once: self.approved_once,
            approved_session: self.approved_session,
            approved_workspace: self.approved_workspace,
            denied: self.denied,
            unknown_outcome: self.cards.saturating_sub(self.answers),
        }
    }
}

/// The bucket one turn's last terminal record puts it in.
#[derive(Clone, Copy, PartialEq, Eq)]
enum TurnBucket {
    Completed,
    Interrupted,
    Unfinished,
    Unknown,
}

/// Which bucket one terminal record's stated outcome belongs to. The match
/// lists every state rather than sending the remainder to one bucket, so a
/// state added later is classified on purpose instead of inflating the
/// finished count.
///
/// A handoff counts as unfinished here: the turn was picked up by another
/// agent and did not finish in this session, so it is no more a completed
/// turn here than a failed one is.
fn turn_bucket(outcome: Option<RunCompletionStatus>) -> TurnBucket {
    match outcome {
        Some(RunCompletionStatus::Completed) => TurnBucket::Completed,
        Some(RunCompletionStatus::Interrupted) => TurnBucket::Interrupted,
        Some(
            RunCompletionStatus::HandedOff
            | RunCompletionStatus::Failed
            | RunCompletionStatus::VerificationFailed
            | RunCompletionStatus::TurnLimitReached,
        ) => TurnBucket::Unfinished,
        None => TurnBucket::Unknown,
    }
}

/// The settled tally of turns by bucket.
#[derive(Clone, Copy, Default)]
struct TurnCounts {
    completed: usize,
    interrupted: usize,
    unfinished: usize,
    unknown: usize,
}

impl TurnCounts {
    fn count(&mut self, bucket: TurnBucket) {
        let slot = match bucket {
            TurnBucket::Completed => &mut self.completed,
            TurnBucket::Interrupted => &mut self.interrupted,
            TurnBucket::Unfinished => &mut self.unfinished,
            TurnBucket::Unknown => &mut self.unknown,
        };
        *slot = slot.saturating_add(1);
    }

    fn audit(&self) -> TurnAudit {
        TurnAudit {
            completed: self.completed,
            interrupted: self.interrupted,
            unfinished: self.unfinished,
            unknown: self.unknown,
        }
    }
}

/// How the session's turns ended, one count per turn rather than per terminal
/// record. A recovery that re-drives an interrupted turn writes a second
/// terminal for the same turn, so the later record replaces the earlier one:
/// the turn counts by how it finally ended, not by an interruption it
/// recovered from, and the share a reader derives stays between zero and one.
#[derive(Default)]
struct TurnEnds {
    counts: TurnCounts,
    /// The newest turn's ending, held as a replacement target until that
    /// turn's boundary arrives.
    last: Option<TurnBucket>,
}

impl TurnEnds {
    /// One turn's terminal record. A later record for the same turn replaces
    /// this one, which is how a re-driven turn keeps a single ending.
    fn record(&mut self, outcome: Option<RunCompletionStatus>) {
        self.last = Some(turn_bucket(outcome));
    }

    /// A turn the log says was aborted rather than one whose result was
    /// recorded: the crash-recovery boundary and the per-turn cancel.
    fn record_aborted(&mut self) {
        self.last = Some(TurnBucket::Interrupted);
    }

    /// Settle the newest turn once a later one opens, or at a context clear.
    fn close(&mut self) {
        if let Some(bucket) = self.last.take() {
            self.counts.count(bucket);
        }
    }

    /// The tally including the newest turn, which stays a replacement target
    /// until its boundary arrives.
    fn audit(&self) -> TurnAudit {
        let mut counts = self.counts;
        if let Some(bucket) = self.last {
            counts.count(bucket);
        }
        counts.audit()
    }
}

/// How many call facts are held. A result normally lands before the turn that
/// issued its call ends, so a small window covers every real ordering while
/// keeping a session's memory flat. A result whose call has been evicted is
/// judged on its output alone, the same rule a windowed read applies.
const PENDING_CALLS_CAP: usize = 512;

/// The session-wide trajectory summary, folded one event at a time.
#[derive(Default)]
pub(crate) struct TrajectorySummaryState {
    user_inputs: usize,
    /// True when the log's first event is not a user input, which the fold
    /// numbers as a turn of its own.
    leading_partial_turn: bool,
    seen_any_event: bool,

    /// Turns that reached their end without reporting usage, so the token
    /// total is a lower bound.
    closed_turns_without_usage: usize,
    /// Whether a turn is open, and whether it has reported usage yet.
    turn_open: bool,
    open_turn_has_usage: bool,

    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    /// Which of the counts above the endpoints reported, folded across the
    /// turns this fold counted, or None before any turn reported usage. An
    /// option rather than the type's own default, because a fold that has
    /// counted nothing is complete (the identity is all-reported) while a
    /// record written before the flags existed reads as unknown. The snapshot
    /// joins it with the delegated half, so the account's completeness has one
    /// owner and no reader combines the two itself.
    turn_provenance: Option<UsageProvenance>,
    failures: usize,
    /// Results that came back without an error payload. Counted from the
    /// results rather than derived from the calls, because a call whose turn
    /// ended before its result arrived has no known outcome and must not be
    /// reported as one.
    successes: usize,
    tool_calls: usize,
    subagent: SubagentUsage,
    recap: RecapSpend,

    /// What the session asked the user and how much of the work finished.
    approvals: ApprovalFold,
    turn_ends: TurnEnds,

    first_ts: Option<u64>,
    last_ts: Option<u64>,

    model_ms: u64,
    tool_ms: u64,
    decode_samples: usize,
    decode_tokens: u64,
    decode_ms: u64,
    ttft: TtftHistogram,

    models: HashSet<Arc<str>>,
    /// Calls whose result has not arrived, and the order they were issued in,
    /// so the oldest can be retired once the map is at its cap.
    pending_calls: HashMap<String, ToolCallFacts>,
    pending_order: VecDeque<String>,
}

impl TrajectorySummaryState {
    /// Fold one event. Streaming deltas advance the span but no counter: they
    /// are not durable, so counting them would make a live session's figures
    /// differ from the same session read back.
    pub(crate) fn record(&mut self, entry: &SessionLogEntry) {
        // A boundary event is not turn content, so it does not decide whether
        // the log opens mid-run: the first content event does. A view that
        // starts at a context clear would otherwise be numbered one turn high.
        if !self.seen_any_event
            && !matches!(
                entry.event,
                SessionEvent::ContextCleared { .. }
                    | SessionEvent::CompactionBoundary { .. }
                    // A recap is written while no turn runs, so a log that
                    // opens with one opens between turns, not mid-run: taking
                    // it for content would open a turn that reported no usage
                    // and make the session total read incomplete for the rest
                    // of its life.
                    | SessionEvent::RecapAttempt { .. }
                    | SessionEvent::AwayRecap { .. }
                    // A delta is not durable and not turn content: a log state that
                    // saw one before any durable event must not take it for a
                    // turn that reported no usage, or the session total would
                    // read as incomplete for the rest of its life.
                    | SessionEvent::AssistantTextDelta { .. }
                    | SessionEvent::CacheDecisions { .. }
            )
        {
            self.seen_any_event = true;
            self.leading_partial_turn = !matches!(entry.event, SessionEvent::UserInput { .. });
            if self.leading_partial_turn {
                // The fold numbers this as a turn, so it must be open for the
                // usage check to see that it reported none.
                self.turn_open = true;
            }
        }
        if self.first_ts.is_none() {
            self.first_ts = Some(entry.ts);
        }
        self.last_ts = Some(entry.ts);
        self.fold(&entry.event);
    }

    /// Fold one durable event into the counters. Each kind owns its own facts,
    /// so a new event type adds a branch rather than a longer match.
    fn fold(&mut self, event: &SessionEvent) {
        match event {
            // Deltas are not durable, so they move no counter; the span they
            // extend is taken above.
            SessionEvent::AssistantTextDelta { .. } => {}
            SessionEvent::UserInput { .. } => self.open_turn(),
            SessionEvent::TurnUsage {
                input_tokens,
                output_tokens,
                cache_read_input_tokens,
                input_reported,
                cache_read_reported,
                cache_write_reported,
                model,
                ..
            } => self.fold_usage(
                *input_tokens,
                *output_tokens,
                *cache_read_input_tokens,
                UsageProvenance {
                    input_reported: *input_reported,
                    cache_read_reported: *cache_read_reported,
                    cache_write_reported: *cache_write_reported,
                },
                model,
            ),
            SessionEvent::ModelStepTiming {
                total_ms,
                ttft_ms,
                decode_ms,
                ..
            } => self.fold_timing(*total_ms, *ttft_ms, *decode_ms),
            SessionEvent::ToolCall {
                call_id,
                tool,
                input,
            } => {
                self.tool_calls = self.tool_calls.saturating_add(1);
                self.fold_tool_call(call_id, tool, input);
            }
            SessionEvent::ToolResult {
                call_id,
                output,
                duration_ms,
            } => self.fold_tool_result(call_id, output, *duration_ms),
            SessionEvent::SubagentReturn {
                tool_calls,
                tool_failures,
                ..
            } => {
                let calls = *tool_calls as usize;
                let failures = *tool_failures as usize;
                self.tool_calls = self.tool_calls.saturating_add(calls);
                self.failures = self.failures.saturating_add(failures);
                // The child's return reports its totals rather than its
                // individual results, so the remainder stands for the calls
                // that came back clean.
                self.successes = self
                    .successes
                    .saturating_add(calls.saturating_sub(failures));
                self.subagent.record(event);
            }
            // A recap runs between turns, so its spend joins the session's own
            // account while moving no turn counter, and the model it named is
            // not added to the models the session ran: the user never switched
            // to it.
            SessionEvent::AwayRecap { .. } => self.recap.record(event),
            SessionEvent::PermissionAsked { call_id, .. } => self.approvals.record_ask(call_id),
            SessionEvent::PermissionDecision {
                call_id,
                verdict,
                scope,
                ..
            } => self.approvals.record_verdict(call_id, *verdict, scope),
            SessionEvent::TurnAborted { .. } => {
                self.turn_ends.record_aborted();
                self.close_turn();
            }
            SessionEvent::RunCompleted { outcome, .. } => {
                self.turn_ends.record(*outcome);
                self.close_turn();
            }
            SessionEvent::ContextCleared { .. } => {
                // A clear ends whatever turn was newest: nothing after it can
                // be that turn's later terminal record.
                self.turn_ends.close();
                self.close_turn();
            }
            _ => {}
        }
    }

    /// A new user input ends the previous turn and opens the next.
    fn open_turn(&mut self) {
        self.turn_ends.close();
        self.close_turn();
        self.user_inputs = self.user_inputs.saturating_add(1);
        self.turn_open = true;
        self.open_turn_has_usage = false;
    }

    fn fold_usage(
        &mut self,
        input: u64,
        output: u64,
        cache_read: u64,
        provenance: UsageProvenance,
        model: &str,
    ) {
        self.open_turn_has_usage = true;
        self.input_tokens = self.input_tokens.saturating_add(input);
        self.output_tokens = self.output_tokens.saturating_add(output);
        self.cache_read_tokens = self.cache_read_tokens.saturating_add(cache_read);
        self.turn_provenance = Some(match self.turn_provenance {
            Some(folded) => folded.merge(provenance),
            None => provenance,
        });
        // Decode speed is measured against the tokens the model produced,
        // which is what the output count reports.
        self.decode_tokens = self.decode_tokens.saturating_add(output);
        if !model.is_empty() {
            self.models.insert(Arc::from(model));
        }
    }

    fn fold_timing(&mut self, total_ms: u64, ttft_ms: Option<u64>, decode_ms: Option<u64>) {
        if let Some(ttft) = ttft_ms {
            self.ttft.record(ttft);
        }
        if let Some(decode) = decode_ms {
            self.decode_ms = self.decode_ms.saturating_add(decode);
            self.decode_samples = self.decode_samples.saturating_add(1);
        }
        self.model_ms = self.model_ms.saturating_add(total_ms);
    }

    fn fold_tool_call(&mut self, call_id: &str, tool: &str, input: &Value) {
        // A repeated id replaces its facts, so its place in the order queue
        // must go with it: leaving the old entry would let the queue grow past
        // the cap while the map stays small.
        if self.pending_calls.contains_key(call_id)
            && let Some(index) = self.pending_order.iter().position(|id| id == call_id)
        {
            self.pending_order.remove(index);
        }
        if self.pending_calls.len() >= PENDING_CALLS_CAP
            && let Some(oldest) = self.pending_order.pop_front()
        {
            self.pending_calls.remove(&oldest);
        }
        self.pending_calls.insert(
            call_id.to_string(),
            ToolCallFacts {
                tool: tool.to_string(),
                command_word: input
                    .get("command")
                    .and_then(|c| c.as_str())
                    .and_then(simple_command_word)
                    .map(str::to_string),
            },
        );
        self.pending_order.push_back(call_id.to_string());
    }

    fn fold_tool_result(&mut self, call_id: &str, output: &Value, duration_ms: u64) {
        let facts = self.pending_calls.remove(call_id);
        if let Some(index) = self.pending_order.iter().position(|id| id == call_id) {
            self.pending_order.remove(index);
        }
        // Only the command word is rebuilt. It is already a simple command
        // word, so re-reading it yields the same word, and the rule sees the
        // shape it does everywhere else.
        let command_input;
        let (tool, input) = match &facts {
            Some(facts) => {
                command_input = facts
                    .command_word
                    .as_ref()
                    .map(|word| serde_json::json!({ "command": word }));
                (
                    facts.tool.as_str(),
                    command_input.as_ref().unwrap_or(&Value::Null),
                )
            }
            // A result whose call sits outside what this fold has seen: the
            // plain error-or-success rule still applies.
            None => ("", &Value::Null),
        };
        // A call the runner answered without executing it — the turn ended
        // while it was pending, the user rejected it — is neither outcome, so
        // it stays outside both counts and a reader sees it as a call with no
        // result.
        if tool_result_failed(output, tool, input) {
            self.failures = self.failures.saturating_add(1);
        } else if !tool_result_not_run(output) {
            self.successes = self.successes.saturating_add(1);
        }
        self.tool_ms = self.tool_ms.saturating_add(duration_ms);
    }

    /// End the open turn: a turn that reached its end without reporting usage
    /// leaves a hole in the session total. Its calls stay held, because a
    /// result can still arrive for one.
    fn close_turn(&mut self) {
        if self.turn_open && !self.open_turn_has_usage {
            self.closed_turns_without_usage = self.closed_turns_without_usage.saturating_add(1);
        }
        self.turn_open = false;
        self.open_turn_has_usage = false;
    }

    /// Turns as the pane numbers them. A log that opens mid-run carries one
    /// more than it has user inputs.
    pub(crate) fn numbered_turns(&self) -> usize {
        self.user_inputs
            .saturating_add(usize::from(self.leading_partial_turn))
    }

    /// Whether every turn reported usage, so the token total is the session's
    /// cost rather than a lower bound.
    fn totals_known(&self) -> bool {
        let open_turn_is_measured = !self.turn_open || self.open_turn_has_usage;
        self.numbered_turns() > 0 && self.closed_turns_without_usage == 0 && open_turn_is_measured
    }

    pub(crate) fn snapshot(&self) -> TrajectorySummary {
        let decode_tok_per_sec = if self.decode_ms > 0 && self.decode_tokens > 0 {
            Some(self.decode_tokens as f64 / (self.decode_ms as f64 / 1000.0))
        } else {
            None
        };
        let (ttft_p95_ms, p95_capped) = split(self.ttft.percentile(0.95));
        let (ttft_p99_ms, p99_capped) = split(self.ttft.percentile(0.99));
        // A child that reached a terminal without reporting usage leaves a
        // hole in the session total just as a turn does, and so does a recap
        // written without figures, so the one definition of completeness
        // covers all three. A reader that had to combine the fields itself
        // would eventually read the total as complete.
        let subagent_unmeasured = self.subagent.unmeasured_calls > 0;
        let turn_provenance = self
            .turn_provenance
            .unwrap_or(UsageProvenance::ALL_REPORTED);
        TrajectorySummary {
            total_turns: self.numbered_turns(),
            usage: TrajectoryUsageSummary {
                // The session's own account is the parent's calls plus every
                // child's and every recap's: what the whole session spent is
                // the figure the headline reports, and each background half is
                // carried beside it so a reader can see its contribution
                // rather than subtract for it.
                input_tokens: self
                    .input_tokens
                    .saturating_add(self.subagent.input_tokens)
                    .saturating_add(self.recap.input_tokens),
                output_tokens: self
                    .output_tokens
                    .saturating_add(self.subagent.output_tokens)
                    .saturating_add(self.recap.output_tokens),
                cache_read_tokens: self
                    .cache_read_tokens
                    .saturating_add(self.subagent.cache_read_input_tokens)
                    .saturating_add(self.recap.cache_read_input_tokens),
                totals_known: self.totals_known()
                    && !subagent_unmeasured
                    && self.recap.unmeasured_calls == 0,
                provenance: turn_provenance
                    .merge(self.subagent.provenance)
                    .merge(self.recap.provenance),
                failures: self.failures,
                successes: self.successes,
                tool_calls: self.tool_calls,
                subagent: self.subagent,
                subagent_unmeasured,
                recap: self.recap,
            },
            timing: TrajectoryTimingSummary {
                ttft_samples: self.ttft.samples as usize,
                ttft_avg_ms: (self.ttft.samples > 0).then(|| self.ttft.sum_ms / self.ttft.samples),
                ttft_p95_ms,
                ttft_p99_ms,
                ttft_percentile_capped: p95_capped || p99_capped,
                decode_samples: self.decode_samples,
                decode_tok_per_sec,
                model_ms: self.model_ms,
                tool_ms: self.tool_ms,
            },
            models_used: self.models.len(),
            single_model: if self.models.len() == 1 {
                self.models.iter().next().cloned()
            } else {
                None
            },
            duration_ms: match (self.first_ts, self.last_ts) {
                (Some(first), Some(last)) => last.saturating_sub(first),
                _ => 0,
            },
            approvals: self.approvals.audit(),
            turns: self.turn_ends.audit(),
        }
    }
}

/// Split a percentile into its value and whether it is a floor.
fn split(value: Option<(u64, bool)>) -> (Option<u64>, bool) {
    match value {
        Some((ms, capped)) => (Some(ms), capped),
        None => (None, false),
    }
}

#[cfg(test)]
mod tests;
