//! Expand and collapse actions for the transcript's foldable blocks. A
//! target is resolved from the selection anchor, a click row, or the
//! transcript tail, and that block's expansion flips in the view state.
//! The block kinds differ in what they hold (a tool result, a fold group, a
//! delegation, a reasoning summary, a memory notice), so each resolution
//! reads its own rows rather than sharing one flat index.

use crate::agent_message::ClientCommand;
use crate::fold::is_memory_notice;
use crate::fold::{DisplaySlot, display_slots};
use crate::records::TranscriptLine;
use crate::state::App;
use crate::ui::selection::anchor_row;
use crate::view::markers::COLLAPSE_SHOW;
use crate::view::todo_list::TODO_FOLD_KEY;
use houyicoder_protocol::frontend::SessionId;

/// The child a transcript row names, as a key target. The two forms answer
/// different keys, and the difference is the point of the type: a live
/// delegation is a fold block the expand chord opens in place, while the
/// trace of a finished child holds no rows to expand, so the drill-in key
/// takes it and the expand chord declines it. Flattened into one shape, the
/// expand chord claimed the trace row it could not open, reported the chord
/// handled, and left the screen unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SubagentTarget {
    /// The live delegation line, with whether its child transcript still
    /// needs fetching.
    Delegation {
        child_sid: String,
        needs_fetch: bool,
    },
    /// The trace a finished child left in the timeline.
    Trace { child_sid: String },
}

impl SubagentTarget {
    /// The child session id either form names, and whether opening it must
    /// fetch the transcript. A delegation knows from its fold block; a trace
    /// never holds rows, so it always asks.
    pub(crate) fn into_parts(self) -> (String, bool) {
        match self {
            Self::Delegation {
                child_sid,
                needs_fetch,
            } => (child_sid, needs_fetch),
            Self::Trace { child_sid } => (child_sid, true),
        }
    }
}

impl App {
    /// The visible-row index (into the per-row metadata vecs published by
    /// the last draw) of the selection anchor, mapping its content row
    /// through the current scroll offset. None when no anchor is set or the
    /// anchor row is scrolled out of the viewport.
    pub(crate) fn anchor_visible_row(&self) -> Option<usize> {
        let ids = self.ui.frame.layout.last_all_ids.borrow();
        let content_row = anchor_row(&self.ui.selection.transcript, &ids)?;
        let top = self.active_scroll().top_offset();
        let ri = content_row.checked_sub(top)?;
        (ri < self.ui.frame.layout.transcript_rect.get().height as usize).then_some(ri)
    }

    /// Toggle the tool result the selection anchor sits on, without falling
    /// back to the last result. Returns false when the anchor is not on a
    /// result row, so the chord can fall through to the tail-most block and
    /// the checklist. This is the early path that keeps a cursor on a result
    /// row from being outranked by the checklist or a newer tail block.
    pub(crate) fn toggle_result_at_anchor(&mut self) -> bool {
        let Some(ri) = self.anchor_visible_row() else {
            return false;
        };
        let Some(id) = self.ui.frame.keys.borrow().callid_at(ri) else {
            return false;
        };
        self.ui.transcript_view.toggle_result(id);
        true
    }

    /// Toggle expand/collapse of a tool result row (Ctrl+O). If the selection
    /// anchor sits on a result row, toggle that one; otherwise toggle the last
    /// result in the transcript. No-op when no result is present. Keyed by
    /// call_id so the choice survives the wholesale transcript rebuild on each
    /// event batch.
    pub(crate) fn toggle_focused_result_expand(&mut self) {
        let lines = self.transcript().lines();
        let target = self
            .anchor_visible_row()
            .and_then(|ri| self.ui.frame.keys.borrow().callid_at(ri))
            .or_else(|| {
                lines.iter().rev().find_map(|l| match l {
                    TranscriptLine::Tool { name, call_id, .. } if name.as_str() == "result" => {
                        Some(call_id.clone())
                    }
                    _ => None,
                })
            });
        if let Some(id) = target {
            self.ui.transcript_view.toggle_result(id);
        }
    }

    /// Toggle expand/collapse of the fold group under the selection anchor
    /// (Ctrl+O on a summary or collapse-hint row). Returns false when the
    /// anchor is not on a fold row, so the caller can try the next target.
    pub(crate) fn toggle_focused_fold_expand(&mut self) -> bool {
        let key = self
            .anchor_visible_row()
            .and_then(|ri| self.ui.frame.keys.borrow().fold_key_at(ri));
        let Some(key) = key else {
            return false;
        };
        // The checklist's summary publishes a fold key so the chord and a click
        // both reach it, but it is not a transcript fold. Passing its key to
        // the transcript view would store a fold group that no block owns: the
        // key would be consumed, nothing would open, and the checklist branch
        // further down would never run.
        if key == TODO_FOLD_KEY {
            self.sessions.current.todos.expanded = !self.sessions.current.todos.expanded;
            return true;
        }
        self.ui.transcript_view.toggle_fold(key);
        true
    }

    /// Toggle the inline expansion of the delegation the cursor is on. False
    /// when the cursor is elsewhere or on a finished child's trace, so the
    /// caller can offer the row to the other expandable blocks. A first expand
    /// of an unloaded line fires a one-shot fetch; a re-expand reuses the
    /// cached rows.
    pub(crate) fn toggle_subagent_expand(&mut self) -> bool {
        let Some(SubagentTarget::Delegation {
            child_sid,
            needs_fetch,
        }) = self.subagent_target_at_cursor()
        else {
            return false;
        };
        self.apply_subagent_toggle(child_sid, needs_fetch);
        true
    }

    /// Toggle the expandable block the transcript holds latest: a delegation,
    /// a reasoning summary, a completed tool group, or a memory-change notice.
    /// All four compete by position: answering for each block from its own
    /// rows is what keeps a delegation from outranking a newer reasoning
    /// block, and omitting one left the hint it prints with no key that could
    /// answer it. False when the transcript holds none of the four.
    pub(crate) fn toggle_tail_expand(&mut self) -> bool {
        enum TailTarget {
            Group(String),
            Delegation(String, bool),
            Thinking,
            Notice(String),
            Result(String),
        }
        // The tail is taken from the transcript the view renders. Reading the
        // live transcript while a search or delegation view is open picks a
        // line the user cannot see, so the key toggles an off-screen block.
        let mut candidates: Vec<(usize, TailTarget)> = Vec::new();
        {
            let lines = self.active_transcript();
            if let Some(idx) = lines
                .iter()
                .rposition(|l| matches!(l, TranscriptLine::Subagent { .. }))
                && let TranscriptLine::Subagent {
                    child_sid,
                    folded_transcript,
                    ..
                } = &lines[idx]
            {
                candidates.push((
                    idx,
                    TailTarget::Delegation(child_sid.clone(), folded_transcript.is_empty()),
                ));
            }
            if let Some(idx) = lines.iter().rposition(|l| {
                matches!(
                    l,
                    TranscriptLine::ThoughtFor {
                        reasoning: Some(_),
                        ..
                    }
                )
            }) {
                candidates.push((idx, TailTarget::Thinking));
            }
            // A group still in the active turn never folds, so it is not a
            // candidate; the position is the group's last line, where its
            // summary row renders.
            if let Some(group) = self.active_fold_groups().iter().rev().find(|g| !g.active) {
                candidates.push((
                    group.end.saturating_sub(1),
                    TailTarget::Group(group.key.clone()),
                ));
            }
            // A notice folds under the same chord, so it competes by position
            // too: newest wins over an older tool group, and a newer group
            // wins over an older notice. The key carries the occurrence
            // ordinal display_slots assigns over this same transcript.
            let mut notice_ordinal = 0usize;
            let mut latest_notice: Option<(usize, String)> = None;
            for (idx, line) in lines.iter().enumerate() {
                if is_memory_notice(line) {
                    latest_notice = Some((idx, format!("mg#{notice_ordinal}")));
                    notice_ordinal += 1;
                }
            }
            if let Some((idx, key)) = latest_notice {
                candidates.push((idx, TailTarget::Notice(key)));
            }
            if let Some(idx) = lines.iter().rposition(|l| {
                matches!(
                    l,
                    TranscriptLine::Compaction { key, summary, .. }
                    if !key.is_empty() && !summary.is_empty()
                )
            }) && let TranscriptLine::Compaction { key, .. } = &lines[idx]
            {
                candidates.push((idx, TailTarget::Notice(key.clone())));
            }
            // A result row whose body folds (more than the always-shown head)
            // competes by position with the other tail blocks, so a chord with
            // the cursor elsewhere still reaches the newest collapsible result.
            if let Some(idx) = lines.iter().rposition(|l| {
                matches!(
                    l,
                    TranscriptLine::Tool { name, body, call_id, .. }
                    if name.as_str() == "result"
                        && !call_id.is_empty()
                        && body.lines().count() > COLLAPSE_SHOW
                )
            }) && let TranscriptLine::Tool { call_id, .. } = &lines[idx]
            {
                candidates.push((idx, TailTarget::Result(call_id.clone())));
            }
        }
        let Some((_, target)) = candidates.into_iter().max_by_key(|(at, _)| *at) else {
            return false;
        };
        match target {
            TailTarget::Group(key) => {
                self.ui.transcript_view.toggle_fold(key);
                self.pin_transcript_top();
                true
            }
            TailTarget::Delegation(sid, needs_fetch) => {
                self.apply_subagent_toggle(sid, needs_fetch);
                true
            }
            TailTarget::Thinking => self.toggle_thinking_expand(),
            TailTarget::Notice(key) => {
                self.ui.transcript_view.toggle_fold(key);
                true
            }
            TailTarget::Result(id) => {
                self.ui.transcript_view.toggle_result(id);
                self.pin_transcript_top();
                true
            }
        }
    }

    /// Toggle a Subagent delegation's inline expansion by visible row index
    /// (the click handler). The row's fold key carries the child session id,
    /// so a click on the head row resolves the target delegation directly —
    /// no cursor walk, no fallback. Returns true when the row was a
    /// delegation head.
    pub(crate) fn toggle_subagent_expand_at_row(&mut self, ri: usize) -> bool {
        let Some(child_sid) = self.ui.frame.keys.borrow().fold_key_at(ri) else {
            return false;
        };
        // The row stash carries only the fold key, not the loaded flag, so
        // look up the matching Subagent line to decide first-expand fetch.
        // A missing match (stale stash) defaults to fetch — the backend's
        // empty-case guard surfaces "unavailable" rather than refetching.
        let lines = self.transcript().lines();
        let needs_fetch = lines
            .iter()
            .find_map(|line| match line {
                TranscriptLine::Subagent {
                    child_sid: sid,
                    folded_transcript,
                    ..
                } if sid == &child_sid => Some(folded_transcript.is_empty()),
                _ => None,
            })
            .unwrap_or(true);
        self.apply_subagent_toggle(child_sid, needs_fetch);
        true
    }

    /// Shared post-resolution body: flip the expand state for a child session
    /// id and, on a first expand of an unloaded line, fire the one-shot
    /// transcript fetch. The fetch lands asynchronously and fills
    /// folded_transcript in place; a re-expand reuses the cached rows.
    fn apply_subagent_toggle(&mut self, child_sid: String, needs_fetch: bool) {
        let expanding = !self
            .ui
            .transcript_view
            .expanded
            .subagents
            .contains(&child_sid);
        self.ui.transcript_view.toggle_subagent(child_sid.clone());
        self.pin_transcript_top();
        if expanding
            && needs_fetch
            && let Some(s) = self.sessions.current.connection.as_ref()
        {
            match s.next_request_id() {
                Ok(req_id) => {
                    self.enqueue_refresh(ClientCommand::ChildTranscriptQuery {
                        req_id,
                        child_sid: SessionId(child_sid),
                    });
                }
                Err(_) => self.note_request_id_exhausted(),
            }
        }
    }

    /// Resolve the child row under the selection cursor. None when the cursor
    /// is unset or on another kind of line; a caller wanting a default names
    /// its own.
    ///
    /// The walk mirrors the render path (build_slots_rows), not the flat
    /// per-line sum: a collapsed run of tool calls renders as one summary
    /// slot, so the cursor's content-row lives in the fold-aware slot
    /// space. A flat walk diverges whenever a folded group precedes the
    /// target line and mis-resolves to the wrong Subagent.
    pub(crate) fn subagent_target_at_cursor(&self) -> Option<SubagentTarget> {
        let transcript = self.active_transcript();
        let groups = self.active_fold_groups();
        let slots = display_slots(
            transcript,
            groups.as_ref(),
            &self.ui.transcript_view.expanded.fold_groups,
            self.ui.verbose,
        );
        let ids = self.ui.frame.layout.last_all_ids.borrow();
        let content_row = anchor_row(&self.ui.selection.transcript, &ids)?;
        let mut row = 0usize;
        let mut first = true;
        for slot in &slots {
            let (rows, opens) = self.slot_rows_and_opens(slot, transcript);
            if !first && opens {
                row += 1;
            }
            if row + rows > content_row {
                // Cursor lands in this slot. A Summary slot or a line that is
                // not about a child is not a delegation, so the answer is None
                // -- this walk does not look elsewhere.
                let DisplaySlot::Line(i, _) = slot else {
                    return None;
                };
                return match &transcript[*i] {
                    TranscriptLine::Subagent {
                        child_sid,
                        folded_transcript,
                        ..
                    } => Some(SubagentTarget::Delegation {
                        child_sid: child_sid.clone(),
                        needs_fetch: folded_transcript.is_empty(),
                    }),
                    // The trace a finished child left behind opens the same
                    // view. It holds no rows of its own, so the expand chord
                    // declines it and only the drill-in key takes it.
                    TranscriptLine::SubagentFinished(child) => Some(SubagentTarget::Trace {
                        child_sid: child.child_sid.clone(),
                    }),
                    _ => None,
                };
            }
            row += rows;
            if rows > 0 {
                first = false;
            }
        }
        None
    }

    /// The child the cursor is on, else the most recent. The drill-in key means
    /// "open a child", so with no line named it opens the newest; the expand
    /// key has other blocks to weigh and defaults differently.
    pub(crate) fn subagent_target_or_last(&self) -> Option<SubagentTarget> {
        let lines = self.transcript().lines();
        self.subagent_target_at_cursor().or_else(|| {
            lines.iter().rev().find_map(|line| match line {
                TranscriptLine::Subagent {
                    child_sid,
                    folded_transcript,
                    ..
                } => Some(SubagentTarget::Delegation {
                    child_sid: child_sid.clone(),
                    needs_fetch: folded_transcript.is_empty(),
                }),
                // The trace names the same child as the fold head does, so
                // an idle enter takes whichever of the two sits nearest the
                // tail.
                TranscriptLine::SubagentFinished(child) => Some(SubagentTarget::Trace {
                    child_sid: child.child_sid.clone(),
                }),
                _ => None,
            })
        })
    }

    /// Toggle the LAST ThoughtFor line's inline reasoning expansion (Ctrl+O).
    /// Targets the most recent "Thought for Ns" line (near the tail) — no
    /// ambiguity for keyboard users (the latest is unique). For older
    /// ThoughtFor lines the user scrolls + clicks. Returns true when a
    /// ThoughtFor with reasoning was toggled.
    pub(crate) fn toggle_thinking_expand(&mut self) -> bool {
        // The rendered rows come from the active transcript, so the last
        // reasoning block is read from the same slice. Keyed by turn_id
        // rather than reasoning text so two turns with identical reasoning
        // do not collide.
        let target = self
            .active_transcript()
            .iter()
            .rev()
            .find_map(|line| match line {
                TranscriptLine::ThoughtFor {
                    reasoning: Some(_),
                    turn_id,
                    ..
                } => Some(turn_id.clone()),
                _ => None,
            });
        let Some(turn_id) = target else {
            return false;
        };
        self.ui.transcript_view.toggle_thinking(turn_id);
        self.pin_transcript_top();
        true
    }

    /// Toggle thinking expansion by display row index (used by the click
    /// handler). Click on a "Thought for Ns" row toggles inline reasoning
    /// expand/collapse — direct target, no cursor drift (the ThoughtFor row
    /// stays at its screen position; expansion adds rows BELOW). Returns true
    /// when the row is a ThoughtFor row with reasoning.
    pub(crate) fn toggle_thinking_expand_at_row(&mut self, ri: usize) -> bool {
        // Resolve the click row straight to the ThoughtFor turn_id published
        // by the draw pass (parallel to the visible fold keys). The old path
        // counted "Nth visible expandable ThoughtFor" then matched it to
        // "Nth in the FULL transcript" — when ThoughtFor rows scrolled out
        // of the viewport the two counts diverged (the visible count skipped
        // off-screen rows, the full count did not), so clicking a visible
        // row toggled an off-screen turn ("point at bottom, expand at top").
        let turn_id = self.ui.frame.keys.borrow().turn_id_at(ri);
        let Some(turn_id) = turn_id else {
            return false;
        };
        // Only a ThoughtFor carrying reasoning is expandable. The published
        // turn_id is set only on the header row; confirm the transcript line
        // is a reasoning-bearing ThoughtFor before toggling (defensive — the
        // render path already gates the hint on reasoning: Some).
        let has_reasoning = self.active_transcript().iter().any(|line| {
            matches!(
                line,
                TranscriptLine::ThoughtFor {
                    reasoning: Some(_),
                    turn_id: tid,
                    ..
                } if tid == &turn_id
            )
        });
        if !has_reasoning {
            return false;
        }
        self.ui.transcript_view.toggle_thinking(turn_id);
        self.pin_transcript_top();
        true
    }

    /// Toggle expand/collapse of a fold group at a display row index (used by
    /// the click handler, which knows the row from the mouse y coordinate).
    /// No-op when the row is not a fold row.
    pub(crate) fn toggle_fold_at_row(&mut self, ri: usize) {
        let Some(key) = self.ui.frame.keys.borrow().fold_key_at(ri) else {
            return;
        };
        // The checklist's summary publishes its own key so the shared chord and
        // a click both reach it. It is not a transcript fold, so it toggles the
        // checklist state rather than a fold group.
        if key == TODO_FOLD_KEY {
            self.sessions.current.todos.expanded = !self.sessions.current.todos.expanded;
            return;
        }
        self.ui.transcript_view.toggle_fold(key);
    }

    /// Collapse the expanded fold group under the selection anchor, if the
    /// anchor row sits inside an expanded block. Used by the mouse-up handler:
    /// a clean click (no drag motion, no word/line span) anywhere in an
    /// expanded block collapses that block. Returns true when a group was
    /// collapsed (the caller skips the drag-finish/copy path); false otherwise
    /// (the caller falls through to finish_drag). Drag-select inside an
    /// expanded block is preserved because a real drag never reaches here
    /// (is_click_only gates the call site).
    pub(crate) fn collapse_expanded_under_anchor(&mut self) -> bool {
        let Some(ri) = self.anchor_visible_row() else {
            return false;
        };
        let key = self
            .ui
            .frame
            .keys
            .borrow()
            .expanded_group_at(ri)
            .filter(|k| self.ui.transcript_view.fold_is_open(k));
        let Some(key) = key else {
            return false;
        };
        self.ui.transcript_view.close_fold(&key);
        self.ui.selection.transcript.clear();
        true
    }
}
