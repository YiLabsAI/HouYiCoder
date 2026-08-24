//! Transcript row-count path, split out of state.rs so the struct file stays
//! under the file-size gate. The count path matches the draw path row-for-row
//! (count==render): live trailing blocks (assistant text, the live thinking
//! block, the spinner) + per-line row counts that honor expand state + width
//! wrap. A drift here pins the scroll offset past real rows (blank space +
//! skipped tool calls when scrolled), so any render-side change lands here too.

use crate::records::TranscriptLine;
use crate::state::App;

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
        let w = self.last_transcript_width.get();
        match line {
            TranscriptLine::Agent(text) => self.agent_text_rows(text),
            TranscriptLine::ThoughtFor {
                reasoning, turn_id, ..
            } => {
                let mut n = 1; // header row + expanded reasoning (width-wrapped)
                if let Some(r) = reasoning
                    && (self.expanded_thinking.contains(turn_id) || self.verbose)
                {
                    n += self.render_cache.borrow_mut().thought_row_count(r, w);
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
            } if name == "result" => self.render_cache.borrow_mut().tool_row_count(
                body,
                call_id,
                Some(*outcome),
                self.expanded_results.contains(call_id) || self.verbose,
                *is_diff,
                w,
            ),
            // Plain selectable rows: count the exact set the draw path emits.
            TranscriptLine::ContextGrid(view) => {
                crate::view::context_view::render_as_rows(view).len()
            }
            TranscriptLine::Subagent {
                child_sid,
                folded_transcript,
                ..
            } => {
                let mut n = 1; // head row
                if self.expanded_subagents.contains(child_sid) || self.verbose {
                    if folded_transcript.is_empty() {
                        n += 1; // placeholder row
                    } else {
                        n += folded_transcript
                            .iter()
                            .map(|c| self.line_display_rows(c))
                            .sum::<usize>();
                    }
                }
                n
            }
            // count==render: a user prompt wraps + caps like the render path.
            TranscriptLine::User(text) => self.render_cache.borrow_mut().user_row_count(text, w),
            // count==render: the chip text is mode-dependent (verbose renders
            // the untruncated invocation, which can span many lines where the
            // truncated status spans at most two) — count the same form the
            // draw path emits, or the scroll offset drifts past real rows.
            _ => {
                let text = if self.verbose {
                    line.render_verbose()
                } else {
                    line.render()
                };
                text.split('\n').count()
            }
        }
    }

    /// Live trailing blocks rendered below the durable transcript while a
    /// reply is streaming.
    /// prefix_empty is true when the slot region produced zero rows, so the
    /// first trailing block skips its leading spacer (matching the render
    /// guard that only adds a spacer when something precedes it).
    pub(crate) fn live_trailing_row_count(&self, prefix_empty: bool) -> usize {
        let mut n = 0;
        let w = self.last_transcript_width.get();
        // No live thinking block during the turn (the live ∴ Thinking block
        // was removed; live reasoning does not echo
        // as a block). The thinking indicator is the spinner row.
        if self.live_active && !self.live_assistant_text.is_empty() {
            if n > 0 || !prefix_empty {
                n += 1;
            }
            // Same markdown renderer as the draw path (count == render), but
            // the single-slot live cache (not the LRU) — streaming text grows
            // every delta, so the LRU would never hit + pollute.
            n += self
                .render_cache
                .borrow_mut()
                .live_agent_row_count(&self.live_assistant_text, w);
        }
        if self.agent_busy && self.run_started.is_some() {
            if n > 0 || !prefix_empty {
                n += 1;
            }
            n += 1;
        }
        // Session checklist rows at the transcript tail, plus leading spacer.
        let todo_n = crate::view::todo_list::render_rows(self).len();
        if todo_n > 0 {
            if n > 0 || !prefix_empty {
                n += 1;
            }
            n += todo_n;
        }
        n
    }

    fn agent_text_rows(&self, text: &str) -> usize {
        self.render_cache
            .borrow_mut()
            .agent_row_count(text, self.last_transcript_width.get())
    }

    /// Walk the active transcript in FLAT order (no fold slots), summing
    /// display rows + the blank spacer before each emitted line. The flat
    /// window-render path's count==render pair: draw_flat_transcript emits
    /// exactly these rows, so flat_display_rows == the rendered total +
    /// flat_row_of_line(idx) == the row where idx starts. Matches fold_aware_rows
    /// minus the slot layer (fold grouping) -- the slot layer has no meaning
    /// when one screen is materialized at a time, so the window view skips it.
    /// Thinking is skipped (0 rows, no spacer), matching the draw path.
    pub(crate) fn flat_walk(&self, target: Option<usize>) -> usize {
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
            // Spacer before each emitted line except the first; Interrupted
            // is a child row of the message above (no blank between).
            if !first && !matches!(line, TranscriptLine::Interrupted) {
                total += 1;
            }
            if target.is_some_and(|t| t == i) {
                return total;
            }
            total += self.line_display_rows(line);
            first = false;
        }
        total
    }

    /// Total display rows the active transcript renders in FLAT (window) mode.
    /// The count==render single source for the window view: the flat draw path
    /// publishes the same value to window_scroll.total.
    pub(crate) fn flat_display_rows(&self) -> usize {
        self.flat_walk(None)
    }

    /// The display-row index where transcript line idx starts in FLAT (window)
    /// mode. Used to jump the window scroll to a search match.
    pub(crate) fn flat_row_of_line(&self, idx: usize) -> usize {
        self.flat_walk(Some(idx))
    }
}
