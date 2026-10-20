//! The delegation records: a spawn opens one, the return closes it, and the
//! child's spend folds into the turn it belongs to.

use super::*;

impl TurnBuilder {
    /// Open a delegation record, completed by the matching return.
    pub(super) fn begin_agent(
        &mut self,
        child_session_id: &str,
        subagent_type: &str,
        prompt_summary: &str,
        ts: u64,
    ) {
        self.touch(ts);
        let summary = if prompt_summary.is_empty() {
            format!("{subagent_type} delegated")
        } else {
            preview(prompt_summary)
        };
        let index = self.push(self.record(
            TrajectoryRecordKind::Agent,
            Some(subagent_type.to_string()),
            summary,
            ts,
        ));
        self.open_agents.insert(child_session_id.to_string(), index);
    }

    /// Close the delegation: its outcome, its summary, and what it spent.
    pub(super) fn finish_agent(
        &mut self,
        child_session_id: &str,
        status: &str,
        summary: &str,
        usage: Option<EventUsage>,
        child_provenance: UsageProvenance,
        ts: u64,
    ) {
        self.touch(ts);
        let index = match self.open_agents.remove(child_session_id) {
            Some(index) => index,
            // The spawn sits outside the loaded window: show the return on its
            // own record rather than dropping it silently.
            None => {
                let record = self.record(
                    TrajectoryRecordKind::Agent,
                    None,
                    format!("{child_session_id} returned"),
                    ts,
                );
                self.push(record)
            }
        };
        let end_ms = self.offset(ts);
        let record = &mut self.records[index];
        if record.start_ms < end_ms {
            record.duration_ms = end_ms - record.start_ms;
        }
        // An unrecognised status is unknown, not a success.
        record.outcome = match status {
            "completed" | "ok" => RecordOutcome::Ok,
            "" => RecordOutcome::Pending,
            _ => RecordOutcome::Failed,
        };
        // A child's usage also accumulates into the turn's own tokens, so the
        // turn row reports what this user request spent overall.
        if let Some(child_u) = usage {
            if let Some(tin) = child_u.input {
                self.tokens_in = Some(self.tokens_in.unwrap_or(0) + tin);
            }
            if let Some(tout) = child_u.output {
                self.tokens_out = Some(self.tokens_out.unwrap_or(0) + tout);
            }
            if let Some(cread) = child_u.cache_read {
                self.cache_read = Some(self.cache_read.unwrap_or(0) + cread);
            }
            if let Some(cwrite) = child_u.cache_write {
                self.cache_write = Some(self.cache_write.unwrap_or(0) + cwrite);
            }
        }
        // The turn's totals now hold the child's counts, so the child's
        // completeness governs them: a field it did not report cannot be read as
        // a measured zero in the turn's sum. This folds whether or not the child
        // recorded any count, so a child nobody measured leaves the turn's
        // account partial instead of reading as one that spent nothing.
        self.usage_provenance = self.usage_provenance.merge(child_provenance);
        record.usage = usage;
        if !summary.is_empty() && self.mode == FoldMode::Records {
            record.output = Some(summary.to_string());
        }
    }

    /// Close a delegation on its return: the record's outcome, the child's
    /// summary, and what the work spent.
    ///
    /// A return that recorded no tokens states no usage at all: it tells the
    /// parent nothing about the spend, which is not the same as a child that
    /// spent nothing. Its completeness still folds into the turn, so the turn
    /// and the session's own aggregation read alike rather than one claiming an
    /// account the other calls unknown.
    pub(super) fn apply_subagent_return(&mut self, ev: &SessionLogEntry) {
        let SessionEvent::SubagentReturn {
            child_session_id,
            status,
            summary,
            input_tokens,
            output_tokens,
            cache_read_input_tokens,
            cache_write_input_tokens,
            reasoning_tokens,
            input_reported,
            cache_read_reported,
            cache_write_reported,
            ..
        } = &ev.event
        else {
            return;
        };
        let provenance = UsageProvenance {
            input_reported: *input_reported,
            cache_read_reported: *cache_read_reported,
            cache_write_reported: *cache_write_reported,
        };
        // A child counts as measured when any of its five counts is above zero,
        // the same bar the session's own aggregation holds it to: a child that
        // reported cache tokens without input or output still spent a real
        // amount, and dropping it would hide a recorded count.
        let measured = *input_tokens > 0
            || *output_tokens > 0
            || *cache_read_input_tokens > 0
            || *cache_write_input_tokens > 0
            || *reasoning_tokens > 0;
        let usage = measured.then_some(EventUsage {
            input: Some(*input_tokens),
            output: Some(*output_tokens),
            cache_read: Some(*cache_read_input_tokens),
            cache_write: Some(*cache_write_input_tokens),
            reasoning: Some(*reasoning_tokens),
            provenance,
        });
        self.finish_agent(child_session_id, status, summary, usage, provenance, ev.ts);
    }
}
