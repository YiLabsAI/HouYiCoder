use super::*;
use crate::records::ToolOutcome;
use crate::selection::diff_gutter_skip;
use ratatui::style::Color;
use unicode_width::UnicodeWidthStr;

#[test]
fn test_row_call_styled() {
    let line = styled_row("⏺ Bash(echo hi)", None).expect("chip");
    // glyph + space + name + args = 4 spans.
    assert_eq!(line.spans.len(), 4);
}

#[test]
fn test_row_call_success_green() {
    // Call chip colored by outcome: Success ⇒ green glyph.
    let line = styled_row("⏺ Bash(ls)", Some(ToolOutcome::Success)).expect("chip");
    assert_eq!(line.spans.len(), 4);
}

#[test]
fn test_row_call_error_red() {
    let line = styled_row("⏺ Bash(rm)", Some(ToolOutcome::Error)).expect("chip");
    assert_eq!(line.spans.len(), 4);
}

#[test]
fn test_row_result_success_default() {
    // A successful result body uses the default foreground (Reset), not
    // green — stdout is plain text, not a semantic signal. Green-washing
    // it drowns the color channel reserved for errors.
    let line = styled_row("  ⎿  {\"success\":true}", Some(ToolOutcome::Success)).expect("chip");
    assert_eq!(line.spans.len(), 2);
    // The body span (index 1) must be Reset, not green.
    assert_eq!(line.spans[1].style.fg, Some(Color::Reset));
}

#[test]
fn test_row_result_red() {
    let line = styled_row("  ⎿  {\"error\":\"boom\"}", Some(ToolOutcome::Error)).expect("chip");
    assert_eq!(line.spans.len(), 2);
}

#[test]
fn test_row_result_read_neutral() {
    // A Read row carries outcome None ⇒ default body (not green/red), so a
    // file whose content has "error" in it does not miscolor.
    let line = styled_row("  ⎿  read config.json", None).expect("chip");
    assert_eq!(line.spans.len(), 2);
}

#[test]
fn test_edit_summary_bolds_counts() {
    // An Edit diff summary bolds the count numbers (bold
    // emphasis on the Added/Removed counts); a non-edit summary does not.
    let line = styled_row(
        "  ⎿  Added 2 lines, removed 1 line",
        Some(ToolOutcome::Success),
    )
    .expect("chip");
    // gutter + "Added " + "2"(bold) + " lines, removed " + "1"(bold) + " line".
    let bold_count = line
        .spans
        .iter()
        .filter(|s| s.style.add_modifier.contains(Modifier::BOLD))
        .count();
    assert_eq!(bold_count, 2, "both counts bold: {:?}", line.spans);
    let joined: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    assert_eq!(joined, "  ⎿  Added 2 lines, removed 1 line");
    // A Read summary does not start with Added/Removed ⇒ single body span.
    let read = styled_row("  ⎿  Read 3 lines", None).expect("chip");
    assert_eq!(read.spans.len(), 2);
}

#[test]
fn test_row_nonmatch_none() {
    assert!(styled_row("● hello", None).is_none());
    assert!(styled_row("> user", None).is_none());
}

#[test]
fn test_short_result_not_folded() {
    let rows = result_body_rows(
        "a\nb\nc",
        "c1",
        Some(ToolOutcome::Success),
        false,
        false,
        80,
    );
    // summary + 2 continuation, no hint.
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].1, "  ⎿  a");
    assert_eq!(rows[0].2, Some(ToolOutcome::Success));
    assert_eq!(rows[0].3.as_deref(), Some("c1"));
    assert_eq!(rows[1].1, "     b");
    assert_eq!(rows[1].2, None);
    assert_eq!(rows[1].3, None);
}

#[test]
fn test_result_body_long_collapses() {
    // 10 lines > threshold 3 -> head (3) + hint, 7 hidden.
    let body = (0..10)
        .map(|i| format!("line{i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let rows = result_body_rows(&body, "c1", Some(ToolOutcome::Success), false, false, 80);
    assert_eq!(rows.len(), 4);
    assert_eq!(rows[0].1, "  ⎿  line0");
    assert_eq!(rows[1].1, "     line1");
    assert_eq!(rows[2].1, "     line2");
    assert!(rows[3].1.contains("+7 lines"));
    assert_eq!(rows[3].3, None);
}

#[test]
fn test_result_body_expanded_all() {
    let body = (0..10)
        .map(|i| format!("line{i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let rows = result_body_rows(&body, "c1", None, true, false, 80);
    assert_eq!(rows.len(), 10);
    assert!(!rows.last().unwrap().1.contains("Ctrl+O"));
}

#[test]
fn test_result_body_at_threshold() {
    // Exactly threshold lines -> not collapsed (n <= threshold).
    let body = (0..COLLAPSE_SHOW)
        .map(|i| format!("line{i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let rows = result_body_rows(&body, "c1", None, false, false, 80);
    assert_eq!(rows.len(), COLLAPSE_SHOW);
    assert!(!rows.last().unwrap().1.contains("Ctrl+O"));
}

#[test]
fn test_result_body_empty() {
    // An empty body must not render a bare "⎿ " marker (the empty-after-
    // marker bug); it shows a "(no output)" placeholder instead.
    let rows = result_body_rows("", "c1", Some(ToolOutcome::Error), false, false, 80);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].1, "  ⎿  (no output)");
    assert_eq!(rows[0].2, Some(ToolOutcome::Error));
    assert_eq!(rows[0].3.as_deref(), Some("c1"));
}

/// The done label a silent command produces reaches the screen as its own
/// row, rather than being read as empty and replaced by the no-output
/// placeholder. The transcript-level test pins that the body says done;
/// this pins what the user actually sees, which is the claim that matters.
#[test]
fn test_result_body_done_row() {
    let rows = result_body_rows("done", "c1", Some(ToolOutcome::Success), false, false, 80);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].1, "  ⎿  done");
    assert_eq!(rows[0].2, Some(ToolOutcome::Success));
}

/// A long non-diff body line (bash stdout / read content) soft-wraps to the
/// pane width: the full content is preserved across rows (not truncated to
/// the pane edge), and count == render holds (the single-source invariant
/// survives wrapping for the plain path too).
#[test]
fn test_stdout_wraps_long_line() {
    let body = "this is a very long stdout line that must soft-wrap at a narrow pane width and not lose its tail";
    let rows = result_body_rows(body, "c1", None, true, false, 30);
    assert!(
        rows.len() > 2,
        "long stdout line must wrap to >2 rows at 30 cols, got {}",
        rows.len()
    );
    // The wrapped content's tail ("tail") survives (not truncated away).
    let joined: String = rows.iter().map(|r| r.1.as_str()).collect::<String>();
    assert!(
        joined.contains("tail"),
        "tail must survive the wrap: {joined}"
    );
    assert_eq!(
        result_row_count(body, true, false, 30),
        rows.len(),
        "count/render drift at the wrapped stdout width"
    );
    // At a wide width the same line does not wrap (1 row + the marker).
    let wide = result_body_rows(body, "c1", None, true, false, 200);
    assert_eq!(wide.len(), 1);
}

/// A pathological body (a single minified-style line of many K chars) is
/// pre-truncated before wrap on the COLLAPSED path (a
/// truncated-content maxChars = SHOW*avail*4), so the count path does
/// not wrap the whole thing; the summary shows 3 rows + a "… +N lines"
/// estimate computed from the ORIGINAL length (pre-truncation does not lie
/// about how much was dropped).
#[test]
fn test_stdout_caps_pathological_body() {
    // A 22k-char single line. At width 24, avail = 14 (24 - 10 column
    // reserve), so a full wrap would be ~1572 rows; the pre-truncate caps
    // the wrapped work at max_chars = 3*14*4 = 168, and the estimate still
    // reports ~1569 hidden rows.
    let huge = "x".repeat(1000 * 22);
    let rows = result_body_rows(&huge, "c1", None, false, false, 24);
    // Collapsed: summary + 2 continuation + 1 hint = 4 rows (COLLAPSE_SHOW=3
    // + the +N lines hint), NOT ~1572 wrapped rows.
    assert!(
        rows.len() <= COLLAPSE_SHOW + 1,
        "collapsed pathological body must pre-truncate before wrap, got {}",
        rows.len()
    );
    let joined: String = rows.iter().map(|r| r.1.as_str()).collect::<String>();
    assert!(
        joined.contains("… +"),
        "the capped tail must carry the +N lines estimate (from the original length): {joined}"
    );
    // The estimate reflects the ORIGINAL length, not the pre-truncated
    // prefix: ~22k chars / 14 ≈ 1572 rows - 3 shown ≈ 1569 hidden.
    assert!(
        joined.contains("1568") || joined.contains("1569") || joined.contains("1570"),
        "the estimate should be ~1569 (original 22k / avail 14 - 3): {joined}"
    );
}

// --- diff parsing + numbering ------------------------------------------

/// A single-hunk diff: the @@ header is consumed, not rendered; the add
/// and remove lines carry line numbers; removals show the old-file
/// number, additions the new-file number (numberDiffLines rollback).
#[test]
fn test_plan_single_hunk_numbers() {
    let body = "Added 1 line, removed 1 line\n@@ -2,3 +2,3 @@\n ctx\n-old\n+new\n tail";
    let plan = plan_diff_body(body, 80).expect("parses");
    assert_eq!(plan.len(), 5); // summary + 4 diff lines, no gap (single hunk)
    assert_eq!(plan[0].kind, PlanKind::Summary);
    // Context at old_start=2, then removal at 3, then add at 3 (rollback),
    // then trailing context at 4.
    assert_eq!(
        plan[1],
        PlanRow {
            kind: PlanKind::Diff(DiffKind::Context),
            num: Some(2),
            text: "ctx".into(),
            word: None
        }
    );
    assert_eq!(
        plan[2],
        PlanRow {
            kind: PlanKind::Diff(DiffKind::Remove),
            num: Some(3),
            text: "old".into(),
            word: None
        }
    );
    assert_eq!(
        plan[3],
        PlanRow {
            kind: PlanKind::Diff(DiffKind::Add),
            num: Some(3),
            text: "new".into(),
            word: None
        }
    );
    assert_eq!(
        plan[4],
        PlanRow {
            kind: PlanKind::Diff(DiffKind::Context),
            num: Some(4),
            text: "tail".into(),
            word: None
        }
    );
}

/// An adjacent remove+add pair below the change threshold attaches word-level
/// diff parts to BOTH rows (so the renderer highlights just the changed
/// words); a pair above the threshold falls back to whole-line bars (None).
#[test]
fn test_plan_pairs_word_diff() {
    // "let x = 1" → "let x = 2": changed 2 / total 18 = 0.11 ≤ 0.4 → word-diff.
    let body = "Added 1 line, removed 1 line\n@@ -1 +1 @@\n-let x = 1\n+let x = 2";
    let plan = plan_diff_body(body, 80).expect("parses");
    // summary + remove + add.
    assert_eq!(plan.len(), 3);
    assert!(plan[1].word.is_some(), "remove row gets word parts");
    assert!(plan[2].word.is_some(), "add row gets word parts");
    // The SAME parts attach to both; the remove row's parts include the
    // removed "1", the add row's parts include the added "2".
    let rem_parts = plan[1].word.as_ref().unwrap();
    let add_parts = plan[2].word.as_ref().unwrap();
    assert_eq!(rem_parts, add_parts, "both rows share the word parts");
    assert!(rem_parts.iter().any(|p| p.removed && p.value.contains('1')));
    assert!(add_parts.iter().any(|p| p.added && p.value.contains('2')));
}

/// A near-total rewrite (change ratio > 0.4) does NOT attach word parts — the
/// renderer falls back to whole-line bars so a messy word-diff does not
/// surface on a substantially-changed line.
#[test]
fn test_plan_skips_word_diff() {
    // "old_val" → "new_val": changed 6 / total 14 ≈ 0.43 > 0.4 → no word-diff.
    let body = "Added 1 line, removed 1 line\n@@ -1 +1 @@\n-old_val\n+new_val";
    let plan = plan_diff_body(body, 80).expect("parses");
    assert_eq!(plan.len(), 3);
    assert!(
        plan[1].word.is_none(),
        "remove row falls back to whole-line"
    );
    assert!(plan[2].word.is_none(), "add row falls back to whole-line");
}

/// A K-line removal block is numbered old_start..old_start+K-1, then the
/// counter backs up so the following add block restarts at old_start.
#[test]
fn test_plan_remove_block_rollback() {
    let body = "Removed 3 lines\n@@ -5,3 +5,1 @@\n-a\n-b\n-c\n+kept";
    let plan = plan_diff_body(body, 80).expect("parses");
    // summary + 3 removes + 1 add, no gap.
    assert_eq!(plan.len(), 5);
    assert_eq!(plan[1].num, Some(5)); // first remove: old_start, not yet advanced
    assert_eq!(plan[2].num, Some(6)); // second: advanced
    assert_eq!(plan[3].num, Some(7)); // third: advanced again
    // After the 3-remove block the counter backs up by 2 (K-1) to 5, so
    // the add restarts at 5.
    assert_eq!(plan[4].num, Some(5));
    assert_eq!(plan[4].kind, PlanKind::Diff(DiffKind::Add));
}

/// Two hunks: a dim Gap row is interspersed BETWEEN them (not before the
/// first, not after the last) — the intersperse gap.
#[test]
fn test_plan_multi_hunk_gap() {
    let body = "Added 2 lines\n@@ -1 +1 @@\n+a\n@@ -10 +10 @@\n+b";
    let plan = plan_diff_body(body, 80).expect("parses");
    // summary + add(a) + gap + add(b).
    assert_eq!(plan.len(), 4);
    assert_eq!(plan[0].kind, PlanKind::Summary);
    assert_eq!(plan[1].kind, PlanKind::Diff(DiffKind::Add));
    assert_eq!(plan[2].kind, PlanKind::Gap);
    assert_eq!(plan[2].text, "...");
    assert_eq!(plan[3].kind, PlanKind::Diff(DiffKind::Add));
}

/// A body with no @@ hunk header is not a structured diff: plan_diff_body
/// returns None so the caller falls back to plain line-by-line rendering.
#[test]
fn test_plan_non_diff_none() {
    assert!(plan_diff_body("just\nsome\nstdout", 80).is_none());
    assert!(plan_diff_body("single line", 80).is_none());
}

// --- diff render rows ---------------------------------------------------

/// A short diff body renders the summary + the numbered, sigil-prefixed
/// diff rows; the @@ header never appears as a row.
#[test]
fn test_diff_rows_numbered() {
    let body = "Added 1 line, removed 1 line\n@@ -2,3 +2,3 @@\n ctx\n-old\n+new";
    let rows = result_body_rows(body, "c1", Some(ToolOutcome::Success), true, true, 80);
    // expanded: summary + top border + 3 diff rows + bottom border.
    assert_eq!(rows.len(), 6);
    assert_eq!(rows[0].1, "  ⎿  Added 1 line, removed 1 line");
    assert_eq!(rows[0].0, TAG_PLAIN);
    assert_eq!(rows[1].0, TAG_DIFF_BORDER);
    // Context row: marker(space) + sep + num + sep, TAG_DIFF_CTX so the
    // copy path strips the gutter (clean-code copy, like add/remove).
    assert_eq!(rows[2].0, TAG_DIFF_CTX);
    assert_eq!(rows[2].1, "  2 ctx");
    // Removal: num 3, '-' sigil.
    assert_eq!(rows[3].0, TAG_DIFF_DEL);
    assert_eq!(rows[3].1, "- 3 old");
    // Addition: num 3, '+' sigil.
    assert_eq!(rows[4].0, TAG_DIFF_ADD);
    assert_eq!(rows[4].1, "+ 3 new");
    assert_eq!(rows[5].0, TAG_DIFF_BORDER);
}

/// A diff content line that fits the wrap budget renders on ONE row — the
/// wrap budget must equal the rendered gutter (marker + sep + num + sep),
/// not overshoot or undershoot. The old budget mismatched the render by one
/// column, so a line that fit the budget overflowed the pane by one char on
/// every row. Pins the budget==render invariant for a single-hunk diff.
#[test]
fn test_diff_wrap_budget_matches() {
    // max_w = 1 (line numbers 1..2). gutter = 1 + 3 = 4. At width 40, avail = 36.
    // A 36-char content line fills the row exactly — it must NOT wrap to a 2nd row.
    let content = "x".repeat(36);
    let body = format!("Added 1 line\n@@ -1 +1 @@\n+{content}");
    let rows = result_body_rows(&body, "c1", Some(ToolOutcome::Success), true, true, 40);
    // summary + top border + 1 add row + bottom border (no wrapped continuation).
    assert_eq!(
        rows.len(),
        4,
        "a line that fits the budget must not wrap: {rows:?}"
    );
    let add_row = &rows[2].1;
    assert_eq!(add_row.as_str(), format!("+ 1 {content}"));
    // Display width of the rendered row must not exceed the pane width.
    let w = UnicodeWidthStr::width(add_row.as_str());
    assert!(w <= 40, "row overflowed the pane: {w} > 40 ({add_row:?})");
}

/// A multi-hunk diff renders a dim "..." gap row between hunks.
#[test]
fn test_diff_gap_between_hunks() {
    let body = "Added 2 lines\n@@ -1 +1 @@\n+a\n@@ -10 +10 @@\n+b";
    let rows = result_body_rows(body, "c1", None, true, true, 80);
    // summary + top border + add(a) + gap + add(b) + bottom border.
    assert_eq!(rows.len(), 6);
    assert_eq!(rows[3].0, TAG_DIFF_HUNK);
    assert_eq!(rows[3].1, "..."); // dim gap, not the @@ header
}

/// A diff body renders in full (no line cap — a
/// structured-diff list has none, unlike stdout's truncated-content cap),
/// and result_row_count (is_diff=true) matches the rendered row count (the
/// @@ header does not count — it is metadata). Also pins the count==render
/// invariant for an is_diff=false body that HAPPENS to contain @@ (bash
/// stdout of a patch fragment): count must NOT consume @@ as a diff gap,
/// it must match the plain render that splits on \n.
#[test]
fn test_diff_collapses_and_counts() {
    let body = "Added 4 lines\n@@ -1 +1 @@\n+a\n@@ -2 +2 @@\n+b\n@@ -3 +3 @@\n+c\n@@ -4 +4 @@\n+d";
    // plan (is_diff=true): summary + top border + add + gap + add + gap + add
    // + gap + add + bottom border = 10. Diffs render full (no COLLAPSE_SHOW),
    // so expanded=false == expanded=true.
    let rows = result_body_rows(body, "c1", None, false, true, 80);
    assert_eq!(rows.len(), 10); // full diff, no collapse hint
    assert!(
        !rows.iter().any(|r| r.1.contains("… +")),
        "no collapse hint for a diff: {:?}",
        rows.iter().map(|r| &r.1).collect::<Vec<_>>()
    );
    assert_eq!(result_row_count(body, false, true, 80), 10);
    assert_eq!(result_row_count(body, true, true, 80), 10);
    // Count==render invariant, both is_diff branches, both expand states.
    for expanded in [false, true] {
        for is_diff in [false, true] {
            let rendered = result_body_rows(body, "c1", None, expanded, is_diff, 80).len();
            assert_eq!(
                result_row_count(body, expanded, is_diff, 80),
                rendered,
                "count/render drift at expanded={expanded} is_diff={is_diff}"
            );
        }
    }
}

/// The latent drift guard: an is_diff=false body whose text contains @@
/// (bash printing a patch fragment, sed of a .patch, model git stdout).
/// The count path must take the SAME branch as render (plain, @@ lines
/// counted verbatim) — never the diff plan (which would consume @@ as
/// metadata + insert a gap, diverging by the gap count). Expanded so the
/// full row set is compared.
#[test]
fn test_count_render_no_drift() {
    let body = "stdout line\n@@ -1 +1 @@\n+a\n@@ -2 +2 @@\n+b";
    // is_diff=false → plain: 5 lines (no @@ consumption, no gap).
    assert_eq!(result_row_count(body, true, false, 80), 5);
    assert_eq!(
        result_row_count(body, true, false, 80),
        result_body_rows(body, "c1", None, true, false, 80).len(),
        "is_diff=false + @@ body must not drift count vs render"
    );
    // is_diff=true → diff plan: summary + top border + add + gap + add +
    // bottom border = 6 (@@ consumed).
    assert_eq!(result_row_count(body, true, true, 80), 6);
    assert_eq!(
        result_row_count(body, true, true, 80),
        result_body_rows(body, "c1", None, true, true, 80).len(),
        "is_diff=true copy must not drift"
    );
}

/// A long diff line soft-wraps to the pane width: the first wrapped row keeps
/// its line number, continuation rows carry a blank gutter (num=None) with the
/// sigil preserved. count==render is pinned at the narrow width too — the
/// single-source invariant survives wrapping (no drift-by-duplication).
#[test]
fn test_diff_wraps_long_line() {
    let body = "Added 1 line\n@@ -1 +1 @@\n+this is a very long added line that must wrap at a narrow pane width";
    let plan = plan_diff_body(body, 20).expect("parses");
    // summary + the add row split into N wrapped rows (N > 2 at width 20).
    assert!(
        plan.len() > 3,
        "expected the long add line to wrap, got {} rows",
        plan.len()
    );
    // First add row keeps the number; continuations are num=None.
    assert_eq!(plan[1].num, Some(1));
    assert!(
        plan[2..].iter().all(|r| r.num.is_none()),
        "continuations are blank-gutter"
    );
    // The sigil kind is preserved across the wrapped continuation rows.
    assert!(
        plan[1..]
            .iter()
            .all(|r| matches!(r.kind, PlanKind::Diff(DiffKind::Add)))
    );
    // A wrapped line drops word-diff (word parts cover the full line, not
    // per-segment); unwrapped lines keep it.
    assert!(plan[1..].iter().all(|r| r.word.is_none()));
    // count == render at the narrow width (post-wrap single source).
    assert_eq!(
        result_row_count(body, true, true, 20),
        result_body_rows(body, "c1", None, true, true, 20).len(),
        "count/render drift at the wrapped width"
    );
    // At a wide width the same body does NOT wrap (1 add row).
    let wide = plan_diff_body(body, 200).expect("parses");
    assert_eq!(wide.len(), 2); // summary + 1 add row, no wrap
}

#[test]
fn test_diff_row_tag_colors() {
    // The rendered text is what the reader sees; how many spans it takes to
    // draw is an implementation detail, so the assertions below are on the
    // text and on the background that fills the bar.
    let line = diff_row("    1 + new", TAG_DIFF_ADD, 11, None, None);
    let joined: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    assert_eq!(joined, "    1 + new");
    assert!(
        line.spans
            .iter()
            .all(|s| s.style.bg == Some(Color::Rgb(28, 38, 32))),
        "every span carries the add bar: {:?}",
        line.spans
    );
    let padded = diff_row("    1 + new", TAG_DIFF_ADD, 20, None, None);
    let joined: String = padded.spans.iter().map(|s| s.content.as_ref()).collect();
    assert_eq!(joined.len(), 20);
    assert_eq!(
        padded.spans.last().expect("a padding span").style.bg,
        Some(Color::Rgb(28, 38, 32))
    );
    let del = diff_row("    2 - old", TAG_DIFF_DEL, 11, None, None);
    let joined: String = del.spans.iter().map(|s| s.content.as_ref()).collect();
    assert_eq!(joined, "    2 - old");
    assert!(
        del.spans
            .iter()
            .all(|s| s.style.fg == Some(Color::Rgb(255, 107, 128))),
        "every span carries the remove colour when no grammar applies: {:?}",
        del.spans
    );
}

/// diff_gutter_skip lands at the content start across number widths + all
/// three line kinds (add / remove / context) so fullscreen copy strips
/// the line-number + sigil gutter uniformly.
#[test]
fn test_diff_gutter_skip_widths() {
    // Layout: marker(sigil) + sep + right-aligned num + sep + content.
    // diff_gutter_skip auto-counts so it tracks any number width.
    // w=1 (single-digit num): sigil + sep + 1 num + sep = col 4.
    assert_eq!(diff_gutter_skip("- 3 old"), 4);
    assert_eq!(diff_gutter_skip("+ 3 new"), 4);
    // context sigil is a space — same structure, same skip.
    assert_eq!(diff_gutter_skip("  2 ctx"), 4);
    // w=2 (right-aligned number): content shifts by the extra digit width.
    assert_eq!(diff_gutter_skip("+ 12 let x"), 5);
    // content with leading spaces (code indent) is preserved past the gutter.
    assert_eq!(diff_gutter_skip("+ 3     let x = 1;"), 4);
    // a non-diff plain row does not match the gutter shape → 0 (copy verbatim).
    assert_eq!(diff_gutter_skip("just some stdout"), 0);
}

/// The inter-hunk gap row renders dim (no cyan @@ text, no background):
/// the tag has no bg so no padding span is appended.
#[test]
fn test_diff_row_gap_dim() {
    let line = diff_row("...", TAG_DIFF_HUNK, 20, None, None);
    let joined: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    assert_eq!(joined, "...");
    assert_eq!(line.spans.len(), 1); // body only, no padding (no bg)
    assert!(line.spans[0].style.bg.is_none());
}

/// A hunk header states both file positions. Numbering a hunk from the old
/// side alone repeats numbers across the omitted region whenever an earlier
/// hunk grew or shrank the file: the first hunk ends on new-file line 204, the
/// omitted region is skipped, and the next hunk then restarts its context at
/// the old-file line 204 again. The user reads two 204s around one "...".
#[test]
fn test_diff_hunks_number_sides() {
    let body = "Edited 3 lines\n\
@@ -200,5 +200,7 @@
 line 200
 line 201
-removed 202
+added 202
+added 203
 line 203
 line 204
@@ -204,3 +206,3 @@
 line 206
-changed 207
+changed 207 now
 line 208
";
    let plan = plan_diff_body(body, 80).expect("parses");
    // A removal carries the old-file number, which is lower than the context
    // above it once the file has grown, so the whole column is not monotonic.
    // What must hold is the seam: the omitted region is skipped, and the next
    // hunk continues from the number its own header states.
    let first_gap = plan
        .iter()
        .position(|r| r.kind == PlanKind::Gap)
        .expect("the omitted region is drawn as a gap");
    let before = plan[..first_gap]
        .iter()
        .rev()
        .find_map(|r| r.num)
        .expect("a numbered row before the gap");
    let after = plan[first_gap + 1..]
        .iter()
        .find_map(|r| r.num)
        .expect("a numbered row after the gap");
    assert_eq!(before, 205, "the hunk ends on its last new-file number");
    assert_eq!(
        after,
        before + 1,
        "the next hunk continues from there, so no number repeats across the gap"
    );
}

/// One line removed, five added, the real rewrite at the end. Pairing by
/// position ties the removed line to the first added line, which is unrelated,
/// and highlights two different statements against each other. Pairing by
/// content finds the line it actually became.
#[test]
fn test_pairing_finds_rewrite() {
    let body = "\n@@ -10,1 +10,5 @@\n-let total = a + b;\n+// explain\n+// more\n+// detail\n+// context\n+let total = a + b + c;\n";
    let plan = plan_diff_body(body, 80).expect("parses");
    let paired: Vec<&PlanRow> = plan.iter().filter(|r| r.word.is_some()).collect();
    assert_eq!(
        paired.len(),
        2,
        "exactly the removed line and the one it became carry word marks: {plan:?}"
    );
    // The removed line's partner is the last added row, not the first.
    let removed = paired
        .iter()
        .find(|r| r.kind == PlanKind::Diff(DiffKind::Remove))
        .expect("removed row");
    let partner = plan
        .iter()
        .find(|r| r.kind == PlanKind::Diff(DiffKind::Add) && r.word.is_some())
        .expect("partner row");
    assert!(
        partner.text.contains("a + b + c"),
        "the rewrite is the partner, not the comment above it: {:?}",
        partner.text
    );
    assert!(removed.text.contains("a + b"), "{:?}", removed.text);
}

/// A remove/add block whose lines share nothing gets no word marks at all.
/// Drawing them annotates a rewrite that never happened.
#[test]
fn test_unrelated_lines_stay_plain() {
    let body = "\n@@ -10,1 +10,1 @@\n-import os\n+return 42;\n";
    let plan = plan_diff_body(body, 80).expect("parses");
    assert!(
        plan.iter().all(|r| r.word.is_none()),
        "unrelated lines carry no word marks: {plan:?}"
    );
}

/// A diff row whose file has a grammar colours its code by what the tokens
/// are, not only by whether the line was added. Without it a reader cannot
/// tell a new keyword from a new identifier in one green bar.
#[test]
fn test_diff_row_colours_syntax() {
    let plain = diff_row("    1 + let x = 1;", TAG_DIFF_ADD, 40, None, None);
    let coloured = diff_row("    1 + let x = 1;", TAG_DIFF_ADD, 40, None, Some("rs"));
    let fg = |line: &Line<'static>| -> Vec<String> {
        line.spans
            .iter()
            .filter_map(|s| s.style.fg)
            .map(|c| format!("{c:?}"))
            .collect()
    };
    assert!(
        fg(&coloured).len() > fg(&plain).len(),
        "the grammar splits the line into more than one colour"
    );
    // The bar survives under the syntax colours: every span keeps the add
    // background, so the row still reads as an addition.
    let bg = |line: &Line<'static>| -> Vec<Option<Color>> {
        line.spans.iter().map(|s| s.style.bg).collect()
    };
    assert!(
        bg(&coloured).iter().any(|b| *b == bg(&plain)[0]),
        "the change background is preserved under the syntax colour"
    );
}

/// A file with no grammar renders the line plain rather than wrong. An
/// invented colour would be worse than none, and the bar already says what
/// changed.
#[test]
fn test_diff_row_unknown_lang() {
    let plain = diff_row("    1 + let x = 1;", TAG_DIFF_ADD, 40, None, None);
    let unknown = diff_row(
        "    1 + let x = 1;",
        TAG_DIFF_ADD,
        40,
        None,
        Some("notalang"),
    );
    assert_eq!(
        unknown.spans.len(),
        plain.spans.len(),
        "an unknown grammar adds no spans of its own"
    );
}

/// The change sigil keeps the row's colour even when the code beside it is
/// grammar-coloured. An added line's plus sign handed to the grammar renders
/// as an operator in the text colour, so the one glyph saying what happened to
/// the line stops saying it.
#[test]
fn test_diff_sigil_keeps_color() {
    let line = diff_row("    1 + let x = 1;", TAG_DIFF_ADD, 40, None, Some("rs"));
    let sigil = line
        .spans
        .iter()
        .find(|s| s.content.trim() == "+")
        .unwrap_or_else(|| {
            panic!("no sigil span among {:?}", line.spans);
        });
    assert_eq!(
        sigil.style.fg,
        Some(Color::Rgb(78, 186, 101)),
        "the plus stays the add colour"
    );
    assert_eq!(
        sigil.style.bg,
        Some(Color::Rgb(28, 38, 32)),
        "and keeps the bar it sits on"
    );
    // The code after it is still grammar-coloured, so the sigil is what was
    // carved out rather than the whole line going flat.
    let colours: Vec<_> = line
        .spans
        .iter()
        .filter(|s| !s.content.trim().is_empty())
        .filter_map(|s| s.style.fg)
        .collect();
    assert!(
        colours.len() > 2,
        "the code keeps several colours: {colours:?}"
    );
}

/// Splitting the sigil off the content never loses or reorders a character.
/// The content can itself begin with a space, hold a plus sign inside a string
/// literal, or be written in a wide-glyph script, and each of those is a place
/// where a one-character split could eat something the reader needs.
#[test]
fn test_diff_sigil_split() {
    for (text, tag, lang) in [
        ("    1 +     indented();", TAG_DIFF_ADD, Some("rs")),
        ("    1 - let s = \"a + b\";", TAG_DIFF_DEL, Some("rs")),
        ("    1 + 中文注释", TAG_DIFF_ADD, Some("rs")),
        ("    1 + x", TAG_DIFF_ADD, Some("nosuchlang")),
    ] {
        let row = diff_row(text, tag, 40, None, lang);
        let joined: String = row.spans.iter().map(|s| s.content.as_ref()).collect();
        let trimmed = joined.trim_end();
        assert_eq!(
            trimmed, text,
            "the split must not lose or reorder a character, tag={tag} lang={lang:?}"
        );
    }
}

/// A diff block is coloured as one document, so a grammar sees a comment
/// opened on one line and closed on another. Colouring each row on its own
/// loses that state, and every line after the opener then paints as code while
/// the text still says it is a comment.
#[test]
fn test_diff_comment_spans_lines() {
    let rows = vec![
        (TAG_DIFF_ADD, "    1 + /* the opener".to_string(), None),
        (TAG_DIFF_ADD, "    2 + still inside".to_string(), None),
        (TAG_DIFF_ADD, "    3 + and still */".to_string(), None),
    ];
    let lines = diff_rows(&rows, 40, Some("rs"));
    // A comment colour is the same on every line of the comment and differs
    // from the colour of code, so a line that reads as code inside the block
    // is the defect this guards.
    let body_colour = |line: &Line<'static>| -> Option<Color> {
        line.spans
            .iter()
            .rev()
            .find(|s| !s.content.trim().is_empty())
            .and_then(|s| s.style.fg)
    };
    let opener = body_colour(&lines[0]);
    let middle = body_colour(&lines[1]);
    assert_eq!(
        middle, opener,
        "a line inside the comment keeps the comment's colour: {lines:?}"
    );
    // The proof that this is the fix and not a coincidence: the same middle
    // row, coloured on its own, reads as code.
    let alone = diff_row("    2 + still inside", TAG_DIFF_ADD, 40, None, Some("rs"));
    assert_ne!(
        body_colour(&alone),
        middle,
        "colouring the row by itself loses the comment state"
    );
}

/// The same state carries across a multi-line string, which is the other shape
/// that spans rows. A raw string opened on one line makes every row up to the
/// closing quote one string, and colouring rows separately breaks it the same
/// way it breaks a comment.
#[test]
fn test_diff_string_spans_lines() {
    let rows = vec![
        (TAG_DIFF_ADD, "    1 + let q = r#\"".to_string(), None),
        (TAG_DIFF_ADD, "    2 + SELECT * FROM t".to_string(), None),
        (TAG_DIFF_ADD, "    3 + \"#;".to_string(), None),
    ];
    let lines = diff_rows(&rows, 40, Some("rs"));
    let body_colour = |line: &Line<'static>| -> Option<Color> {
        line.spans
            .iter()
            .find(|s| s.content.contains("SELECT"))
            .and_then(|s| s.style.fg)
    };
    let quoted = body_colour(&lines[1]).expect("the query is coloured");
    let alone = body_colour(&diff_row(
        "    2 + SELECT * FROM t",
        TAG_DIFF_ADD,
        40,
        None,
        Some("rs"),
    ))
    .expect("the row coloured alone is still coloured");
    assert_ne!(
        quoted, alone,
        "inside the raw string the query is not read as code"
    );
}
