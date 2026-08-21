//! Unified-diff plan parsing for the Edit/MultiEdit result body: turn a
//! one-line summary + a unified diff into the pre-collapse display plan
//! (summary row + line-numbered diff rows + dim gap rows between hunks),
//! with soft-wrap and word-level diff pairing. Pure (no App, no Frame) so
//! the layout is unit-testable in isolation. Extracted from markers.rs so
//! that file stays under the size gate.

use crate::view::word_diff::WordDiffPart;
use crate::view::word_diff::word_diff;

/// A unified-diff line kind (add / remove / nochange). The sigil is the
/// leading char of the raw hunk line (+ / - / space).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum DiffKind {
    Add,
    Remove,
    Context,
}

/// A parsed unified-diff hunk: the old-file start line number (from the
/// @@ -N,... header) + the classified lines (sigil stripped). The @@
/// header itself is consumed as metadata, never rendered — the line
/// numbers are derived from the @@ oldStart field, not parsed from the
/// @@ text, so the header never reaches the rendered rows.
struct DiffHunk {
    old_start: usize,
    lines: Vec<(DiffKind, String)>,
}

/// The kind of a fully-planned display row (before collapse + gutter
/// formatting). Summary is the result's one-line head; Diff is a numbered
/// add/remove/context line; Gap is the dim "..." interspersed
/// between hunks (the omitted unchanged context between two hunks).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum PlanKind {
    Summary,
    Diff(DiffKind),
    Gap,
}

/// A display row in the pre-collapse plan: its kind, the line number to show
/// in the gutter (None for the summary + the inter-hunk gap), the text, and
/// the word-level diff parts for an adjacent remove+add pair below the change
/// threshold (None otherwise). When present, the renderer splits the content
/// into word-spans so a small inline edit highlights just the changed words;
/// the row text still carries the full gutter+content for stable width + copy.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) struct PlanRow {
    pub(super) kind: PlanKind,
    pub(super) num: Option<usize>,
    pub(super) text: String,
    pub(super) word: Option<Vec<WordDiffPart>>,
}

/// Parse an Edit/MultiEdit result body (one-line summary + a unified diff)
/// into the pre-collapse display plan: the summary row, then for each hunk
/// its line-numbered diff rows, with a dim Gap row interspersed between
/// hunks. Line numbers follow the standard structured-patch scheme: context and
/// add lines advance the counter, a remove block numbers each line then
/// backs the counter up so the following add block restarts from the hunk's
/// old-file start (removals show old-file numbers, additions show new-file
/// numbers, context shows the running new-file number). Returns None when
/// the body carries no @@ hunk header (not a structured diff body) so the
/// caller falls back to plain line-by-line rendering.
pub(super) fn plan_diff_body(body: &str, width: u16) -> Option<Vec<PlanRow>> {
    let mut lines = body.split('\n');
    let summary = lines.next()?.to_string();
    let mut hunks: Vec<DiffHunk> = Vec::new();
    let mut cur: Option<DiffHunk> = None;
    for line in lines {
        if let Some(rest) = line.strip_prefix("@@ ") {
            if let Some(h) = cur.take() {
                hunks.push(h);
            }
            // "@@ -N,... +M,... @@" — the old-file start is the digits after
            // the first '-'. A new-file hunk emits "-0" (empty old); parse
            // falls back to 1 on a malformed header (fail safe, never panic).
            let after_minus = rest.strip_prefix('-').unwrap_or(rest);
            let digits: String = after_minus
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect();
            let old_start = digits.parse::<usize>().unwrap_or(1);
            cur = Some(DiffHunk {
                old_start,
                lines: Vec::new(),
            });
            continue;
        }
        let hunk = cur.as_mut()?;
        let (kind, text) = if let Some(t) = line.strip_prefix('+') {
            (DiffKind::Add, t.to_string())
        } else if let Some(t) = line.strip_prefix('-') {
            (DiffKind::Remove, t.to_string())
        } else {
            // Context line (leading space) and the "\ No newline at end of
            // file" marker: strip the leading sigil char; a bare-empty context
            // line stays "".
            (DiffKind::Context, line.get(1..).unwrap_or("").to_string())
        };
        hunk.lines.push((kind, text));
    }
    if let Some(h) = cur.take() {
        hunks.push(h);
    }
    if hunks.is_empty() {
        return None;
    }

    let mut plan =
        Vec::with_capacity(1 + hunks.len() + hunks.iter().map(|h| h.lines.len()).sum::<usize>());
    plan.push(PlanRow {
        kind: PlanKind::Summary,
        num: None,
        text: summary,
        word: None,
    });
    for (hi, hunk) in hunks.iter().enumerate() {
        if hi > 0 {
            plan.push(PlanRow {
                kind: PlanKind::Gap,
                num: None,
                text: "...".to_string(),
                word: None,
            });
        }
        plan.extend(per_hunk_rows(hunk));
    }
    wrap_diff_plan(&mut plan, width);
    Some(plan)
}

/// Soft-wrap each diff content row to the pane width (the wrapText
/// behavior). The line-number + sigil gutter (4-space indent + the
/// right-aligned number + space + sigil + space) is reserved; the content
/// wraps in the remaining columns. A wrapped content line becomes N rows: the
/// first keeps its line number, continuation rows carry a blank gutter (number
/// suppressed, sigil preserved). Word-level diff is dropped on a wrapped line
/// (the word parts cover the full line, not the per-row segments) — a wrapped
/// line renders as whole-line bars per segment; unwrapped lines keep their
/// word-diff. avail == 0 (unknown / too-narrow pane) skips wrapping so the
/// caller falls back to the terminal's truncation, never panics.
fn wrap_diff_plan(plan: &mut Vec<PlanRow>, width: u16) {
    let avail = match width_width_avail(plan, width) {
        Some(a) if a > 0 => a,
        _ => return,
    };
    let mut out: Vec<PlanRow> = Vec::with_capacity(plan.len());
    for row in plan.drain(..) {
        let PlanRow {
            kind,
            num,
            text,
            word,
        } = row;
        if !matches!(kind, PlanKind::Diff(_)) {
            out.push(PlanRow {
                kind,
                num,
                text,
                word,
            });
            continue;
        }
        let segments = crate::view::line_wrap::wrap_line(&text, avail);
        if segments.len() <= 1 {
            out.push(PlanRow {
                kind,
                num,
                text,
                word,
            });
            continue;
        }
        // Wrapped: first segment keeps the number + (dropped) word; the rest
        // carry a blank gutter (num=None) + no word.
        let mut iter = segments.into_iter();
        out.push(PlanRow {
            kind,
            num,
            text: iter.next().unwrap_or_default(),
            word: None,
        });
        for seg in iter {
            out.push(PlanRow {
                kind,
                num: None,
                text: seg,
                word: None,
            });
        }
    }
    *plan = out;
}

/// The available content width for wrapping a diff content row = pane width
/// minus the 4-space indent minus the gutter (max line-number width + space +
/// sigil + space). None when there are no numbered rows (no gutter to reserve)
/// or the width is too small to reserve a gutter.
fn width_width_avail(plan: &[PlanRow], width: u16) -> Option<usize> {
    let max_w = plan
        .iter()
        .filter_map(|r| r.num)
        .map(|n| n.to_string().len())
        .max()?
        .max(1);
    // marker(1) + space(1) + max_w(number) + space(1) — a gutter width of
    // maxDigits + 3. No indent lead (the diff is framed in a bordered box
    // rather than indented under ⎿). Keeps the wrap budget equal to the
    // rendered gutter so content does not overflow the pane by one column
    // (the old budget mismatched the render).
    let gutter = max_w + 3;
    let width = usize::from(width);
    width.checked_sub(gutter)
}

/// Number + pair the lines of one hunk into planned rows. Line numbers follow
/// the standard structured-patch scheme: context and add lines advance the counter,
/// a remove block numbers each line then backs the counter up so the following
/// add block restarts from the hunk's old-file start (removals show old-file
/// numbers, additions show new-file numbers). After numbering, adjacent
/// remove+add blocks are paired for inline word-level diff (pair_word_diffs).
fn per_hunk_rows(hunk: &DiffHunk) -> Vec<PlanRow> {
    let mut rows = Vec::with_capacity(hunk.lines.len());
    let mut i = hunk.old_start;
    let mut idx = 0;
    while idx < hunk.lines.len() {
        let (kind, text) = &hunk.lines[idx];
        match kind {
            DiffKind::Context | DiffKind::Add => {
                rows.push(PlanRow {
                    kind: PlanKind::Diff(*kind),
                    num: Some(i),
                    text: text.clone(),
                    word: None,
                });
                i += 1;
                idx += 1;
            }
            DiffKind::Remove => {
                // First removal takes the current counter (not yet advanced);
                // each subsequent removal advances first, so a K-line removal
                // block is numbered old_start..old_start + K - 1. Then the
                // counter backs up by (K - 1) so the following add block
                // restarts at the same old start — the counter is decremented
                // by the number of removed lines.
                rows.push(PlanRow {
                    kind: PlanKind::Diff(DiffKind::Remove),
                    num: Some(i),
                    text: text.clone(),
                    word: None,
                });
                idx += 1;
                let mut subsequent = 0usize;
                while idx < hunk.lines.len() && hunk.lines[idx].0 == DiffKind::Remove {
                    i += 1;
                    rows.push(PlanRow {
                        kind: PlanKind::Diff(DiffKind::Remove),
                        num: Some(i),
                        text: hunk.lines[idx].1.clone(),
                        word: None,
                    });
                    subsequent += 1;
                    idx += 1;
                }
                i -= subsequent;
            }
        }
    }
    pair_word_diffs(&mut rows);
    rows
}

/// Pair adjacent remove+add blocks within a hunk + attach word-level diff
/// parts to the paired rows — an adjacent-line pairing +
/// word-diff + change-threshold gate. A remove block followed by an
/// add block pairs min(remove_len, add_len) lines; for each pair the change
/// ratio (changed chars / total chars) is computed, and only when it is at
/// or below the threshold are the word-parts attached — a near-total rewrite
/// (ratio > threshold) falls back to whole-line bars so a messy word-diff
/// does not surface. The same parts are attached to both the remove and the
/// add row; the renderer filters by tag (remove shows equal+removed, add
/// shows equal+added).
const WORD_DIFF_CHANGE_THRESHOLD: f64 = 0.4;
fn pair_word_diffs(rows: &mut [PlanRow]) {
    let mut i = 0;
    while i < rows.len() {
        if !matches!(rows[i].kind, PlanKind::Diff(DiffKind::Remove)) {
            i += 1;
            continue;
        }
        // Collect the remove block [i..r_end).
        let r_start = i;
        while i < rows.len() && matches!(rows[i].kind, PlanKind::Diff(DiffKind::Remove)) {
            i += 1;
        }
        let r_end = i;
        // Collect the following add block [i..a_end).
        let a_start = i;
        while i < rows.len() && matches!(rows[i].kind, PlanKind::Diff(DiffKind::Add)) {
            i += 1;
        }
        let a_end = i;
        if a_start == a_end {
            // No add block follows → no pairing.
            continue;
        }
        let pair_count = (r_end - r_start).min(a_end - a_start);
        for k in 0..pair_count {
            let r_idx = r_start + k;
            let a_idx = a_start + k;
            let old_text = rows[r_idx].text.clone();
            let new_text = rows[a_idx].text.clone();
            let parts = word_diff(&old_text, &new_text);
            let total = old_text.chars().count() + new_text.chars().count();
            let changed: usize = parts
                .iter()
                .filter(|p| p.added || p.removed)
                .map(|p| p.value.chars().count())
                .sum();
            let ratio = if total == 0 {
                0.0
            } else {
                changed as f64 / total as f64
            };
            if ratio <= WORD_DIFF_CHANGE_THRESHOLD {
                rows[r_idx].word = Some(parts.clone());
                rows[a_idx].word = Some(parts);
            }
        }
    }
}
