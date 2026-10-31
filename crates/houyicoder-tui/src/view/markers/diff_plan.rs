//! Unified-diff plan parsing for the Edit/MultiEdit result body: turn a
//! one-line summary + a unified diff into the pre-collapse display plan
//! (summary row + line-numbered diff rows + dim gap rows between hunks),
//! with soft-wrap and word-level diff pairing. Pure (no App, no Frame) so
//! the layout is unit-testable in isolation. Extracted from markers.rs so
//! that file stays under the size gate.

use crate::view::line_wrap::wrap_line;
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

/// A parsed unified-diff hunk: both file start line numbers from the
/// @@ -N,... +M,... header plus the classified lines (sigil stripped). The @@
/// header itself is consumed as metadata, never rendered.
///
/// Both starts are kept because a hunk can grow the file. The old and new
/// line numbers diverge by however much the earlier hunks added or removed, so
/// a hunk numbered from one counter alone repeats numbers across the omitted
/// region between hunks.
struct DiffHunk {
    old_start: usize,
    new_start: usize,
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
/// hunks. Line numbers follow the standard structured-patch scheme with one
/// counter per file: a removal advances only the old-file counter, an addition
/// only the new-file counter, and a context line advances both and shows the
/// new-file number. Each hunk starts from the two positions its own header
/// states, so a file that grew in an earlier hunk does not repeat numbers.
/// Returns None when
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
            // "@@ -N,... +M,... @@" — the old-file start follows the first
            // '-', the new-file start follows the first '+'. A new-file hunk
            // emits "-0" (empty old); parse falls back to 1 on a malformed
            // header (fail safe, never panic). A header naming no plus side
            // falls back to the old start, which is what a one-sided header
            // means.
            let after_minus = rest.strip_prefix('-').unwrap_or(rest);
            let old_start = leading_number(after_minus).unwrap_or(1);
            let new_start = rest
                .split_once('+')
                .and_then(|(_, after_plus)| leading_number(after_plus))
                .unwrap_or(old_start);
            cur = Some(DiffHunk {
                old_start,
                new_start,
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
    let avail = match content_wrap_width(plan, width) {
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
        let segments = wrap_line(&text, avail);
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

/// The content width available for wrapping a diff row: the pane width less
/// the gutter (widest line number, plus the sigil and the two spaces framing
/// it). None when no row is numbered, so there is no gutter to reserve, or
/// when the pane is narrower than the gutter itself.
fn content_wrap_width(plan: &[PlanRow], width: u16) -> Option<usize> {
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

/// Number + pair the lines of one hunk into planned rows.
///
/// Two counters run side by side, one per file, because the two files stop
/// agreeing at the first change. A removed line consumes a line of the old
/// file only; an added line consumes one of the new file only; a context line
/// consumes one of each. Both start from the hunk's own header, so a hunk that
/// follows an earlier one which grew or shrank the file continues from the
/// numbers the header states instead of repeating the old side's.
///
/// After numbering, adjacent remove+add blocks are paired for inline
/// word-level diff (pair_word_diffs).
fn per_hunk_rows(hunk: &DiffHunk) -> Vec<PlanRow> {
    let mut rows = Vec::with_capacity(hunk.lines.len());
    let mut old_line = hunk.old_start;
    let mut new_line = hunk.new_start;
    for (kind, text) in &hunk.lines {
        let num = match kind {
            DiffKind::Remove => {
                let num = old_line;
                old_line += 1;
                num
            }
            DiffKind::Add => {
                let num = new_line;
                new_line += 1;
                num
            }
            DiffKind::Context => {
                let num = new_line;
                old_line += 1;
                new_line += 1;
                num
            }
        };
        rows.push(PlanRow {
            kind: PlanKind::Diff(*kind),
            num: Some(num),
            text: text.clone(),
            word: None,
        });
    }
    pair_word_diffs(&mut rows);
    rows
}

/// The leading run of digits in text, as a number.
fn leading_number(text: &str) -> Option<usize> {
    let digits: String = text.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
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

/// The least similarity two lines need before their inline words are drawn
/// against each other. Below this they are different lines that happen to sit
/// in one remove/add block, and pairing them annotates a rewrite that never
/// happened.
const PAIR_MIN_SIMILARITY: f64 = 0.4;

/// How alike two lines are, as the share of their words the two hold in
/// common. A cheap multiset measure is enough here: the question is only which
/// added line a removed line became, and the answer is almost always the only
/// candidate that shares most of its words.
fn line_similarity(left: &str, right: &str) -> f64 {
    let words = |s: &str| -> Vec<String> {
        s.split(|c: char| !c.is_alphanumeric() && c != '_')
            .filter(|w| !w.is_empty())
            .map(str::to_string)
            .collect()
    };
    let a = words(left);
    let b = words(right);
    if a.is_empty() && b.is_empty() {
        return 0.0;
    }
    let mut pool = b.clone();
    let mut shared = 0usize;
    for word in &a {
        if let Some(pos) = pool.iter().position(|w| w == word) {
            pool.remove(pos);
            shared += 1;
        }
    }
    let union = a.len() + b.len() - shared;
    if union == 0 {
        return 0.0;
    }
    shared as f64 / union as f64
}
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
        // Pair by content, not by position. Index-for-index pairing assumes
        // the k-th removed line became the k-th added line, which holds only
        // when a block rewrites every line in place. A block that deletes one
        // line and adds five puts the rewrite at an arbitrary offset, and
        // pairing del[0] with add[0] then highlights two unrelated lines
        // against each other. Each removed line takes the most similar added
        // line, which is the one it actually became.
        let mut used_add = vec![false; a_end - a_start];
        for r_idx in r_start..r_end {
            let old_text = rows[r_idx].text.clone();
            let mut best: Option<(usize, f64)> = None;
            for offset in 0..(a_end - a_start) {
                if used_add[offset] {
                    continue;
                }
                let score = line_similarity(&old_text, &rows[a_start + offset].text);
                if best.is_none_or(|(_, top)| score > top) {
                    best = Some((offset, score));
                }
            }
            let Some((offset, score)) = best else {
                break;
            };
            // Below this the two lines share nothing worth drawing: pairing
            // them annotates a rewrite that never happened.
            if score < PAIR_MIN_SIMILARITY {
                continue;
            }
            used_add[offset] = true;
            let a_idx = a_start + offset;
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
