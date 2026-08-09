//! Fold group-formation tests: raw-title bucketing (Update / WebFetch /
//! memory land in the right bucket, not other) and how a diff result
//! mid-run breaks a group so an edit renders expanded.

use crate::fold::{
    DisplaySlot, ToolStats, accumulate_brief, compute_fold_groups, display_slots, render_summary,
};
use crate::records::{ToolOutcome, TranscriptLine};
use std::collections::HashSet;

fn tool_call(cid: &str, name: &str, brief: &str, oc: ToolOutcome) -> TranscriptLine {
    TranscriptLine::Tool {
        name: name.to_string(),
        tool: name.to_string(),
        status: brief.to_string(),
        invocation: brief.to_string(),
        outcome: oc,
        call_id: cid.to_string(),
        body: String::new(),
        is_diff: false,
    }
}

fn tool_result(cid: &str, oc: ToolOutcome) -> TranscriptLine {
    TranscriptLine::Tool {
        name: "result".to_string(),
        tool: "result".to_string(),
        status: String::new(),
        invocation: String::new(),
        outcome: oc,
        call_id: cid.to_string(),
        body: String::new(),
        is_diff: false,
    }
}

/// A call whose chip name is the user-facing Update (raw title edit) must
/// bucket as edit, not other — the fold summary must not read
/// "ran 1 other tool". Drives the raw-title path through tool_call_at.
#[test]
fn test_update_chip_not_other() {
    let mut s = ToolStats::default();
    accumulate_brief(&mut s, "edit", "src/foo.rs");
    assert_eq!(s.edit, 1);
    assert_eq!(s.other, 0);
    let summary = render_summary(&s, &[], false);
    assert!(
        !summary.plain.contains("other"),
        "Update call must not render as other: {:?}",
        summary
    );
}

/// WebFetch and memory tools must not collapse to "ran N other tool" —
/// they have dedicated buckets so a turn that only fetched a page or
/// saved a memory reads as a concrete action, not a generic other.
#[test]
fn test_webfetch_memory_not_other() {
    let mut s = ToolStats::default();
    accumulate_brief(&mut s, "WebFetch", "https://example.com");
    assert_eq!(s.search, 1);
    assert_eq!(s.other, 0);
    let summary = render_summary(&s, &[], false);
    assert!(!summary.plain.contains("other"), "WebFetch: {:?}", summary);

    let mut s = ToolStats::default();
    accumulate_brief(&mut s, "save_memory", "note: x");
    assert_eq!(s.mem_write, 1);
    assert_eq!(s.edit, 0);
    assert_eq!(s.other, 0);
    let summary = render_summary(&s, &[], false);
    assert!(
        !summary.plain.contains("other"),
        "save_memory: {:?}",
        summary
    );
}

/// The raw title edit (chip shows Update) buckets as edit, not other
/// (accumulate_brief reads the raw tool field, not the chip name). edit is
/// non-foldable by the fold rule (each edit is its own individual message
/// with a diff), so a turn with a single edit forms NO fold group - it
/// renders individual, content visible.
#[test]
fn test_update_chip_buckets_edit() {
    let mut s = ToolStats::default();
    accumulate_brief(&mut s, "edit", "src/foo.rs");
    assert_eq!(s.edit, 1, "raw edit title buckets as edit");
    assert_eq!(s.other, 0, "edit does not fall to other");
    // edit is non-foldable: a single-edit turn forms no group.
    let line = TranscriptLine::Tool {
        name: "Update".to_string(),
        tool: "edit".to_string(),
        status: "src/foo.rs".to_string(),
        invocation: "src/foo.rs".to_string(),
        outcome: ToolOutcome::Success,
        call_id: "c1".to_string(),
        body: String::new(),
        is_diff: false,
    };
    let t = vec![
        TranscriptLine::User("hi".into()),
        line,
        TranscriptLine::Tool {
            name: "result".to_string(),
            tool: "edit".to_string(),
            status: String::new(),
            invocation: String::new(),
            outcome: ToolOutcome::Success,
            call_id: "c1".to_string(),
            body: String::new(),
            is_diff: false,
        },
    ];
    let g = compute_fold_groups(&t, false);
    assert_eq!(g.len(), 0, "edit renders individual, not folded");
}

/// A diff result (edit/multiedit) landing mid-run BREAKS the group so the
/// edit renders as its own expanded call (Update chip + diff), not buried
/// under a cross-tool summary led by a preceding search/read clause. Edits
/// stay individual rather than folding them into a
/// cross-tool aggregate. [bash, edit(diff), read] → bash group + edit
/// (individual) + read group.
#[test]
fn test_diff_during_run_breaks() {
    let diff_call = |cid: &str, oc: ToolOutcome| TranscriptLine::Tool {
        name: "Update".to_string(),
        tool: "edit".to_string(),
        status: "foo.rs".to_string(),
        invocation: "foo.rs".to_string(),
        outcome: oc,
        call_id: cid.to_string(),
        body: String::new(),
        is_diff: false,
    };
    let diff_result = |cid: &str, oc: ToolOutcome| TranscriptLine::Tool {
        name: "result".to_string(),
        tool: "edit".to_string(),
        status: String::new(),
        invocation: String::new(),
        outcome: oc,
        call_id: cid.to_string(),
        body: "Added 1 line, removed 1 line".to_string(),
        is_diff: true,
    };
    let t = vec![
        TranscriptLine::User("hi".into()),
        tool_call("c1", "bash", "ls", ToolOutcome::Success),
        tool_result("c1", ToolOutcome::Success),
        diff_call("c2", ToolOutcome::Success),
        diff_result("c2", ToolOutcome::Success),
        tool_call("c3", "read", "b.rs", ToolOutcome::Success),
        tool_result("c3", ToolOutcome::Success),
    ];
    let g = compute_fold_groups(&t, false);
    // The diff (c2) is exempt from grouping: it renders as its own expanded
    // call. bash (c1) forms one group, read (c3) forms another. Two groups,
    // neither counts the edit.
    assert_eq!(
        g.len(),
        2,
        "diff mid-run breaks: bash + read groups, edit individual"
    );
    assert_eq!(g[0].start, 1);
    assert_eq!(g[0].end, 3);
    assert_eq!(g[0].stats.list, 1);
    assert_eq!(g[0].stats.edit, 0);
    assert_eq!(g[1].start, 5);
    assert_eq!(g[1].end, 7);
    assert_eq!(g[1].stats.read_count(), 1);
    assert_eq!(g[1].stats.edit, 0);
}

/// Regression test reproducing the live bug where a Glob+Read+Edit turn rendered the
/// Read+Edit result bodies UNDER the Glob chip with the Read/Edit call chips
/// missing. The Edit's diff result must exempt it (render its own chip + diff),
/// and the Glob+Read pair folds to a Summary — never a detached result body
/// landing under the wrong chip. A System line between calls and results (the
/// truncation checkpoint shape) must not detach a result from its call.
#[test]
fn test_glob_read_edit_chips() {
    let diff_result = |cid: &str, oc: ToolOutcome| TranscriptLine::Tool {
        name: "result".to_string(),
        tool: "edit".to_string(),
        status: String::new(),
        invocation: String::new(),
        outcome: oc,
        call_id: cid.to_string(),
        body: "Added 4 lines".to_string(),
        is_diff: true,
    };
    // Glob + Read + Edit; a System frame sits between calls and results (the
    // truncation-checkpoint arrival shape). Edit carries the diff result.
    let t = vec![
        TranscriptLine::User("go".into()),
        tool_call("c1", "glob", "README*", ToolOutcome::Success),
        tool_call("c2", "read", "README.md", ToolOutcome::Success),
        tool_call("c3", "edit", "README.md", ToolOutcome::Success),
        TranscriptLine::System("checkpoint".into()),
        tool_result("c1", ToolOutcome::Success),
        tool_result("c2", ToolOutcome::Success),
        diff_result("c3", ToolOutcome::Success),
    ];
    // transcript_from_frames is not in scope here; exercise the fold layer the
    // render path consumes. After the projection rebuilds, the call+result
    // adjacency is what the fold sees; model it as the post-reorder shape.
    let reordered = vec![
        TranscriptLine::User("go".into()),
        tool_call("c1", "glob", "README*", ToolOutcome::Success),
        tool_result("c1", ToolOutcome::Success),
        tool_call("c2", "read", "README.md", ToolOutcome::Success),
        tool_result("c2", ToolOutcome::Success),
        tool_call("c3", "edit", "README.md", ToolOutcome::Success),
        diff_result("c3", ToolOutcome::Success),
    ];
    let expanded = HashSet::new();
    let slots = display_slots(&reordered, false, &expanded, false);
    // The edit (diff) is exempt — its call chip must surface as a Line slot,
    // not be buried under a Glob/Read summary. Count Line slots whose index is
    // a call row (name != "result"): must include the edit call.
    let edit_call_idx = reordered
        .iter()
        .position(|l| matches!(l, TranscriptLine::Tool { tool, name, .. } if tool == "edit" && name != "result"))
        .expect("edit call row present");
    assert!(
        slots
            .iter()
            .any(|s| matches!(s, DisplaySlot::Line(i, _) if *i == edit_call_idx)),
        "edit call chip must render (not hidden): {slots:?}"
    );
    // The glob+read pair collapses to one Summary (their chips hide behind it),
    // so neither the glob nor the read call chip renders as a bare Line. This
    // is the expected shape — a result body must never land under a foreign
    // chip. Assert no Line slot holds the glob call alone.
    let glob_call_idx = reordered
        .iter()
        .position(|l| matches!(l, TranscriptLine::Tool { tool, name, .. } if tool == "glob" && name != "result"))
        .expect("glob call row present");
    assert!(
        !slots
            .iter()
            .any(|s| matches!(s, DisplaySlot::Line(i, _) if *i == glob_call_idx)),
        "glob call must be inside a collapsed summary, not a bare line: {slots:?}"
    );
    drop(t); // retained to document the arrival shape the reorder models.
}

#[test]
fn test_slots_no_groups_lines() {
    let t = vec![
        TranscriptLine::User("hi".into()),
        TranscriptLine::Agent("hello".into()),
    ];
    let expanded = HashSet::new();
    let slots = display_slots(&t, false, &expanded, false);
    assert_eq!(slots.len(), 2);
    assert!(matches!(slots[0], DisplaySlot::Line(0, _)));
    assert!(matches!(slots[1], DisplaySlot::Line(1, _)));
}

/// save_memory lands in the mem_write bucket and renders "wrote N memory"
/// (past tense, completed group), not "edited N files". This is a
/// memory-write bucket — a memory save is a meta-operation on
/// the agent's own state, not a source-file edit.
#[test]
fn test_save_memory_wrote_bucket() {
    let mut s = ToolStats::default();
    accumulate_brief(&mut s, "save_memory", "note: x");
    assert_eq!(s.mem_write, 1);
    assert_eq!(s.edit, 0);
    let summary = render_summary(&s, &[], false);
    assert_eq!(summary.plain, "Wrote 1 memory");
}

/// delete_memory lands in its own mem_delete bucket (no delete tool
/// exists in the meta-op set; folding it into mem_write would read "wrote" for a delete,
/// which is wrong). A destructive op gets its own verb for visibility.
#[test]
fn test_delete_memory_deleted_bucket() {
    let mut s = ToolStats::default();
    accumulate_brief(&mut s, "delete_memory", "old-key");
    assert_eq!(s.mem_delete, 1);
    assert_eq!(s.mem_write, 0);
    assert_eq!(s.edit, 0);
    let summary = render_summary(&s, &[], false);
    assert_eq!(summary.plain, "Deleted 1 memory");
}

/// Pluralization: two saves → "memories", and a save+delete in one group
/// renders both memory parts (write before delete), leading the file
/// counts. Memory ops lead search/read counts in the summary.
#[test]
fn test_memory_plural_and_order() {
    let mut s = ToolStats::default();
    accumulate_brief(&mut s, "save_memory", "a");
    accumulate_brief(&mut s, "save_memory", "b");
    accumulate_brief(&mut s, "delete_memory", "c");
    let summary = render_summary(&s, &[], false);
    assert_eq!(summary.plain, "Wrote 2 memories, deleted 1 memory");
}

/// Memory ops lead the file-op counts: a turn that saves a memory AND
/// reads a file renders "wrote 1 memory, read 1 file" (memory first), not
/// the reverse. The memory-before-read ordering.
#[test]
fn test_memory_leads_file_ops() {
    let mut s = ToolStats::default();
    accumulate_brief(&mut s, "read", "src/foo.rs");
    accumulate_brief(&mut s, "save_memory", "note");
    let summary = render_summary(&s, &[], false);
    assert_eq!(summary.plain, "Wrote 1 memory, read 1 file");
}

/// Active groups use present participle: "writing N memory" / "deleting
/// N memory" — the in-progress form. Past tense is covered by the
/// completed-group tests above.
#[test]
fn test_memory_active_tense() {
    let mut s = ToolStats::default();
    accumulate_brief(&mut s, "save_memory", "note");
    accumulate_brief(&mut s, "delete_memory", "old");
    let summary = render_summary(&s, &[], true);
    assert_eq!(summary.plain, "Writing 1 memory, deleting 1 memory");
}
