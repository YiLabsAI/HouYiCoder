//! Transcript row-count path, split out of state.rs so the struct file stays
//! under the file-size gate. The count path matches the draw path row-for-row
//! (count==render): live trailing blocks (assistant text, the live thinking
//! block, the spinner) + per-line row counts that honor expand state + width
//! wrap. A drift here pins the scroll offset past real rows (blank space +
//! skipped tool calls when scrolled), so any render-side change lands here too.

use crate::records::TranscriptLine;
use crate::state::App;
use crate::view::context_view::render_as_rows;
use crate::view::line_wrap::wrap_line;
use crate::view::todo_list::render_rows;
use crate::view::working::command_state;
use crate::view::working::compaction_rows::visible_digest_lines;

impl App {
    /// The row count a single transcript line renders to. A tool result uses
    /// the result-body row count (summary + continuations + optional hint,
    /// honoring per-result expand state) so multi-line bodies no longer
    /// undercount as one; Agent text is counted via the same markdown renderer
    /// the draw path uses (blank separators and code fences collapse to zero
    /// rows, which naive split('\n') overcounts — that drift pinned the scroll
    /// offset past real rows and surfaced as blank space + skipped tool calls
    /// when scrolled); anything else falls back to its rendered lines.
    pub(crate) fn line_display_rows(&self, line: &TranscriptLine) -> usize {
        self.line_display_rows_mode(line, self.ui.verbose)
    }

    pub(crate) fn line_display_rows_mode(
        &self,
        line: &TranscriptLine,
        full_tool_call: bool,
    ) -> usize {
        let w = self.ui.frame.feedback.transcript_width.get();
        match line {
            TranscriptLine::Agent(text) => self.agent_text_rows(text),
            TranscriptLine::ThoughtFor {
                reasoning, turn_id, ..
            } => {
                let mut n = 1; // header row + expanded reasoning (width-wrapped)
                if let Some(r) = reasoning
                    && (self.ui.transcript_view.expanded.thinking.contains(turn_id)
                        || self.ui.verbose)
                {
                    n += self.ui.renderer.lines.borrow_mut().thought_row_count(r, w);
                }
                n
            }
            // Thinking is not a row above the answer (folded into the
            // thought-for line below); zero rows, content stays for /search.
            TranscriptLine::Thinking { .. } => 0,
            TranscriptLine::Tool {
                name,
                call_id,
                body,
                is_diff,
                outcome,
                ..
            } if name == "result" => {
                let ended = command_state::ended_body(self, line);
                self.ui.renderer.lines.borrow_mut().tool_row_count(
                    ended.as_deref().unwrap_or(body),
                    call_id,
                    Some(*outcome),
                    self.ui.transcript_view.expanded.results.contains(call_id) || self.ui.verbose,
                    *is_diff,
                    w,
                )
            }
            // Plain selectable rows: count the exact set the draw path emits.
            // A compaction draws its head row plus one wrapped row per digest
            // line while expanded, so a flat count of the label undershoots by
            // exactly the digest and every offset after it drifts.
            TranscriptLine::Compaction { key, summary, .. } => {
                let mut n = 1; // head row
                if self.ui.transcript_view.expanded.fold_groups.contains(key) || self.ui.verbose {
                    let digest_rows: usize = visible_digest_lines(summary)
                        .into_iter()
                        .map(|logical| wrap_line(logical, w as usize).len())
                        .sum();
                    n += if digest_rows > 40 { 41 } else { digest_rows };
                }
                n
            }
            TranscriptLine::ContextGrid(view) => render_as_rows(view, w as usize).len(),
            TranscriptLine::Subagent {
                child_sid,
                folded_transcript,
                ..
            } => self.subagent_display_rows(child_sid, folded_transcript),
            // The trace draws as one row at every width: the name takes the
            // room that is left and the figures give way before the row
            // does. Its text is one line even when the task summary carried
            // a newline, so the fallback's line split would count the same
            // row twice.
            TranscriptLine::SubagentFinished(_) => 1,
            // count==render: a user prompt wraps + caps like the render path.
            TranscriptLine::User(text) => {
                self.ui.renderer.lines.borrow_mut().user_row_count(text, w)
            }
            TranscriptLine::Tool { name, .. } if name != "result" => {
                let call_rows = line
                    .tool_call_rows(w, full_tool_call)
                    .map_or(1, |rows| rows.len());
                call_rows + command_state::running_rows(self, line, w).len()
            }
            TranscriptLine::Recap(_) => line.recap_rows(w).map_or(0, |rows| rows.len()),
            TranscriptLine::System(_) => {
                let text = if self.ui.verbose {
                    line.render_verbose()
                } else {
                    line.render()
                };
                text.split('\n')
                    .map(|logical| wrap_line(logical, w as usize).len())
                    .sum()
            }
            // count==render: the chip text is mode-dependent (verbose renders
            // the untruncated invocation, which can span many lines where the
            // truncated status occupies one) — count the same form the
            // draw path emits, or the scroll offset drifts past real rows.
            _ => {
                let text = if self.ui.verbose {
                    line.render_verbose()
                } else {
                    line.render()
                };
                text.split('\n').count()
            }
        }
    }

    /// The rows one subagent card takes: the head row, plus the child's
    /// folded transcript when it is open. Each child is drawn below the head,
    /// so a child that opens a block takes its spacer even when it is the
    /// first: the draw path already has the head in its rows and charges the
    /// blank line.
    fn subagent_display_rows(&self, child_sid: &str, folded: &[TranscriptLine]) -> usize {
        let mut n = 1;
        if self
            .ui
            .transcript_view
            .expanded
            .subagents
            .contains(child_sid)
            || self.ui.verbose
        {
            if folded.is_empty() {
                n += 1;
            } else {
                n += folded
                    .iter()
                    .map(|child| {
                        let rows = self.line_display_rows(child);
                        rows + usize::from(rows > 0 && child.opens_block())
                    })
                    .sum::<usize>();
            }
        }
        n
    }

    /// Live trailing blocks rendered below the durable transcript while a
    /// reply is streaming.
    /// prefix_empty is true when the slot region produced zero rows, so the
    /// first trailing block skips its leading spacer (matching the render
    /// guard that only adds a spacer when something precedes it).
    pub(crate) fn live_trailing_row_count(&self, prefix_empty: bool) -> usize {
        // The parent's live rows (streaming text + spinner + todos) belong
        // to the parent view. Suppress them while a teammate view is open so
        // the count matches the render path (build_tail_rows early-returns
        // empty on the same guard). Without this, count == render breaks
        // — the count path inflates by the parent's live rows while the
        // render path shows zero, desyncing scroll + fold-aware offsets.
        if self.ui.teammate_view.is_some() {
            return 0;
        }
        let mut n = 0;
        let w = self.ui.frame.feedback.transcript_width.get();
        // No live thinking block during the turn (the live ∴ Thinking block
        // was removed; live reasoning does not echo
        // as a block). The thinking indicator is the spinner row.
        if let Some(p) = self.run_progress()
            && p.live_active
            && !p.live_assistant_text.is_empty()
        {
            if n > 0 || !prefix_empty {
                n += 1;
            }
            // Same markdown renderer as the draw path (count == render), but
            // the single-slot live cache (not the LRU) — streaming text grows
            // every delta, so the LRU would never hit + pollute.
            n += self
                .ui
                .renderer
                .lines
                .borrow_mut()
                .live_agent_row_count(p.live_assistant_text.as_str(), w);
        }
        if self.agent_busy() && self.run_started().is_some() {
            if n > 0 || !prefix_empty {
                n += 1;
            }
            n += 1;
        }
        // Session checklist rows at the transcript tail, plus leading spacer.
        let todo_n = render_rows(self).len();
        if todo_n > 0 {
            if n > 0 || !prefix_empty {
                n += 1;
            }
            n += todo_n;
        }
        n
    }

    fn agent_text_rows(&self, text: &str) -> usize {
        self.ui
            .renderer
            .lines
            .borrow_mut()
            .agent_row_count(text, self.ui.frame.feedback.transcript_width.get())
    }

    /// Walk the active transcript in FLAT order (no fold slots), summing
    /// display rows plus the blank spacer before each line that opens a block.
    /// The history window's count and render paths emit exactly these rows, so
    /// history_display_rows is the rendered total and history_row_of_line(idx)
    /// is the row where idx starts. The slot layer has no meaning there, since
    /// one screen is materialized at a time. The draw puts no spacer above the
    /// first row it emits, so a leading line that renders nothing leaves the
    /// next line at the top without one.
    pub(crate) fn history_walk(&self, target: Option<usize>) -> usize {
        let transcript = self.active_transcript();
        let mut total = 0;
        let mut first = true;
        for (i, line) in transcript.iter().enumerate() {
            if matches!(line, TranscriptLine::Thinking { .. }) {
                if target.is_some_and(|t| t == i) {
                    return total;
                }
                continue;
            }
            // Spacer before each line that opens a block; a child row such as
            // the interrupt notice or a tool result stays against its parent.
            if !first && line.opens_block() {
                total += 1;
            }
            if target.is_some_and(|t| t == i) {
                return total;
            }
            let rows = self.line_display_rows(line);
            total += rows;
            // First means "no rows emitted yet", matching the draw's is_empty
            // check. A line that renders nothing must not enable a spacer for
            // the line below it.
            if rows > 0 {
                first = false;
            }
        }
        total
    }

    /// Total display rows the active transcript renders in FLAT (window) mode.
    /// The count==render single source for the window view: the flat draw path
    /// publishes the same value to window_scroll.total.
    #[cfg(test)]
    pub(crate) fn history_display_rows(&self) -> usize {
        self.history_walk(None)
    }

    /// The display-row index where transcript line idx starts in FLAT (window)
    /// mode. Used to jump the window scroll to a search match.
    pub(crate) fn history_row_of_line(&self, idx: usize) -> usize {
        self.history_walk(Some(idx))
    }
}
