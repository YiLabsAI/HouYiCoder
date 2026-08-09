//! Fold summary + classification + slots tests. Split from fold.rs so that
//! file stays under the file-size gate; the cases live here, not the
//! production code.

use super::*;

#[test]
fn test_classify_bash_search() {
    assert_eq!(classify_bash("grep -r foo ."), BashKind::Search);
    assert_eq!(classify_bash("rg 'pattern' src/"), BashKind::Search);
}

#[test]
fn test_classify_bash_read() {
    assert_eq!(classify_bash("cat README.md"), BashKind::Read);
    assert_eq!(classify_bash("tail -n 50 log.txt"), BashKind::Read);
}

#[test]
fn test_classify_bash_list() {
    assert_eq!(classify_bash("ls -la"), BashKind::List);
    assert_eq!(classify_bash("find . -type f"), BashKind::List);
    assert_eq!(classify_bash("tree src/"), BashKind::List);
}

#[test]
fn test_classify_bash_plain() {
    assert_eq!(classify_bash("npm run build"), BashKind::Bash);
    assert_eq!(classify_bash("echo hello && make check"), BashKind::Bash);
}

#[test]
fn test_classify_bash_pipeline_first() {
    // find . -type f | wc -l: find is a list; wc is not a keyword, so the
    // first matching token (find) wins -> List.
    assert_eq!(classify_bash("find . -type f | wc -l"), BashKind::List);
    // grep ... | head: grep matches first -> Search.
    assert_eq!(classify_bash("grep foo bar | head"), BashKind::Search);
}

#[test]
fn test_accumulate_per_tool() {
    let mut s = ToolStats::default();
    accumulate(&mut s, "bash", &serde_json::json!({"command": "ls -la"}));
    accumulate(
        &mut s,
        "bash",
        &serde_json::json!({"command": "npm run build"}),
    );
    accumulate(&mut s, "read", &serde_json::json!({"file_path": "a.rs"}));
    accumulate(&mut s, "grep", &serde_json::json!({"pattern": "x"}));
    accumulate(&mut s, "edit", &serde_json::json!({"file_path": "b.rs"}));
    accumulate(
        &mut s,
        "todo_write",
        &serde_json::json!({"todos": [{"content": "x"}]}),
    );
    assert_eq!(
        s,
        ToolStats {
            list: 1,
            bash: 1,
            read_paths: ["a.rs"].into_iter().map(str::to_string).collect(),
            read_ops: 0,
            search: 1,
            edit: 1,
            write: 0,
            todo: 1,
            other: 0,
            ..Default::default()
        }
    );
}

#[test]
fn test_render_summary_todo() {
    let s = ToolStats {
        todo: 2,
        ..Default::default()
    };
    assert_eq!(render_summary(&s, &[], false).plain, "Updated 2 checklists");
    assert_eq!(render_summary(&s, &[], true).plain, "Updating 2 checklists");
    let s = ToolStats {
        todo: 1,
        ..Default::default()
    };
    assert_eq!(render_summary(&s, &[], false).plain, "Updated 1 checklist");
}

#[test]
fn test_render_past_tense() {
    let s = ToolStats {
        search: 2,
        read_paths: ["a.rs"].into_iter().map(str::to_string).collect(),
        bash: 3,
        ..Default::default()
    };
    let out = render_summary(&s, &[], false);
    assert!(
        out.plain.starts_with("Searched for 2 patterns"),
        "{:?}",
        out
    );
    assert!(out.plain.contains("read 1 file"), "{:?}", out);
    assert!(out.plain.contains("ran 3 shell commands"), "{:?}", out);
    // The "(ctrl+o to expand)" affordance lives in the styled line (UI chrome),
    // not in plain (plain is the selection/copy source — chrome must not copy).
    let line_text: String = out.line.spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(
        line_text.contains("(ctrl+o to expand)"),
        "line carries suffix: {:?}",
        out
    );
    let active = render_summary(&s, &[], true);
    let active_text: String = active
        .line
        .spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect();
    assert!(
        !active_text.contains("(ctrl+o to expand)"),
        "active bare: {:?}",
        active
    );
}

#[test]
fn test_render_summary_active_tense() {
    let s = ToolStats {
        bash: 1,
        ..Default::default()
    };
    let out = render_summary(&s, &[], true);
    assert_eq!(out.plain, "Running 1 shell command");
}

/// A Write call buckets separately from Edit so the turn summary reads
/// "Wrote N files", not "Edited N files" (a Write creates, not edits).
#[test]
fn test_render_summary_write() {
    let s = ToolStats {
        write: 1,
        ..Default::default()
    };
    assert_eq!(render_summary(&s, &[], false).plain, "Wrote 1 file");
    assert_eq!(render_summary(&s, &[], true).plain, "Writing 1 file");
    let s = ToolStats {
        write: 2,
        ..Default::default()
    };
    assert_eq!(render_summary(&s, &[], false).plain, "Wrote 2 files");
}

#[test]
fn test_render_summary_pluralization() {
    let s = ToolStats {
        list: 2,
        ..Default::default()
    };
    assert_eq!(render_summary(&s, &[], false).plain, "Listed 2 directories");
    let s = ToolStats {
        list: 1,
        ..Default::default()
    };
    assert_eq!(render_summary(&s, &[], false).plain, "Listed 1 directory");
}

#[test]
fn test_render_summary_empty() {
    assert_eq!(render_summary(&ToolStats::default(), &[], false).plain, "");
}

/// Re-reading the same file counts once (read_paths dedup by path) — a
/// read-path set. Three reads of a.rs → "Read 1 file".
#[test]
fn test_read_dedup_same_path() {
    let mut s = ToolStats::default();
    accumulate_brief(&mut s, "read", "a.rs");
    accumulate_brief(&mut s, "read", "a.rs");
    accumulate_brief(&mut s, "read", "a.rs");
    assert_eq!(s.read_count(), 1);
    assert_eq!(render_summary(&s, &[], false).plain, "Read 1 file");
}

/// Distinct paths count each; mixed with pathless bash cat calls, the cats
/// surface too (read_paths.len() + read_ops, not len-if-nonempty-else-ops).
#[test]
fn test_read_paths_plus_ops() {
    let mut s = ToolStats::default();
    accumulate_brief(&mut s, "read", "a.rs");
    accumulate_brief(&mut s, "read", "b.rs");
    // Two bash cats (no file_path to dedup on) → read_ops.
    accumulate_brief(&mut s, "bash", "cat x.txt");
    accumulate_brief(&mut s, "bash", "cat y.txt");
    assert_eq!(s.read_count(), 4, "2 paths + 2 ops = 4");
    let out = render_summary(&s, &[], false);
    assert!(out.plain.contains("Read 4 files"), "{:?}", out);
}

/// The ⎿ hint for a group's last bash call: $ <command> (capped), no
/// leading # comment to prefer.
#[test]
fn test_hint_bash_command() {
    assert_eq!(compute_hint("bash", "ls -la"), Some("$ ls -la".into()));
    assert_eq!(
        compute_hint("bash", "npm run build"),
        Some("$ npm run build".into())
    );
}

/// When the bash command carries a # label comment the model wrote for the
/// human, the hint prefers that label over the raw command — a
/// bash-comment-label extraction.
#[test]
fn test_hint_bash_comment_label() {
    assert_eq!(
        compute_hint("bash", "# list staged files\ngit diff --cached"),
        Some("list staged files".into())
    );
    // No comment → fall back to $ <command>.
    assert_eq!(
        compute_hint("bash", "git status"),
        Some("$ git status".into())
    );
}

/// Read hint is the file path; grep/glob hint quotes the pattern.
#[test]
fn test_hint_paths_and_patterns() {
    assert_eq!(
        compute_hint("read", "src/lib.rs"),
        Some("src/lib.rs".into())
    );
    assert_eq!(compute_hint("grep", "todo"), Some("\"todo\"".into()));
    assert_eq!(compute_hint("glob", "**/*.rs"), Some("\"**/*.rs\"".into()));
    assert_eq!(
        compute_hint("WebFetch", "https://x.io"),
        Some("https://x.io".into())
    );
    // Empty invocation → no hint.
    assert_eq!(compute_hint("read", ""), None);
}

/// A collapsed group with a hint carries the hint on the FoldGroup; the
/// render path pushes a second ⎿ row for it, and fold_aware_rows counts
/// 2 (summary + hint) so the count matches render (clicks route correctly).
#[test]
fn test_group_hint_last_call() {
    let t = vec![
        TranscriptLine::User("hi".into()),
        tool_call("c1", "bash", "ls -la", ToolOutcome::Success),
        tool_result("c1", ToolOutcome::Success),
        tool_call("c2", "read", "a.rs", ToolOutcome::Success),
        tool_result("c2", ToolOutcome::Success),
    ];
    let g = compute_fold_groups(&t, false);
    assert_eq!(g.len(), 1);
    // The last foldable call is the read → hint is its path.
    assert_eq!(g[0].hint.as_deref(), Some("a.rs"));
}

/// The count numbers render bold (so the eye latches onto them) while the
/// verb and noun stay dim. Pin so a future span reorder can't silently drop
/// the bold modifier from the count span.
#[test]
fn test_summary_count_bold() {
    use ratatui::style::Modifier;
    let s = ToolStats {
        search: 2,
        ..Default::default()
    };
    let out = render_summary(&s, &[], false);
    // The "2" is the count span; it must carry BOLD. Other spans are dim only.
    let count_span = out
        .line
        .spans
        .iter()
        .find(|sp| sp.content == "2")
        .expect("a count span for 2");
    assert!(
        count_span.style.add_modifier.contains(Modifier::BOLD),
        "count bold: {:?}",
        count_span
    );
}

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

/// A bash result row carrying a stdout body (git commit/push output lives in
/// stdout, surfaced via extract_body). Used by git-op fold tests.
fn tool_result_body(cid: &str, body: &str, oc: ToolOutcome) -> TranscriptLine {
    TranscriptLine::Tool {
        name: "result".to_string(),
        tool: "result".to_string(),
        status: String::new(),
        invocation: String::new(),
        outcome: oc,
        call_id: cid.to_string(),
        body: body.to_string(),
        is_diff: false,
    }
}

/// A git-commit bash call surfaces as "Committed abc123" in the summary,
/// and is NOT counted as a generic bash command (the load-bearing outcome
/// leads; the bash count excludes git-op calls). Pins the G7 fold wiring.
#[test]
fn test_git_commit_surfaces_summary() {
    let t = vec![
        TranscriptLine::User("hi".into()),
        tool_call("c1", "bash", "git commit -m x", ToolOutcome::Success),
        tool_result_body(
            "c1",
            "[main abc1234] x\n 1 file changed",
            ToolOutcome::Success,
        ),
        TranscriptLine::Agent("done".into()),
    ];
    let g = compute_fold_groups(&t, false);
    assert_eq!(g.len(), 1);
    assert_eq!(g[0].stats.bash, 0, "git-op bash not counted as bash");
    assert_eq!(g[0].git_ops.len(), 1, "one git op detected");
    let out = render_summary(&g[0].stats, &g[0].git_ops, false);
    assert!(
        out.plain.contains("Committed abc123"),
        "summary leads with commit: {}",
        out.plain
    );
    assert!(
        !out.plain.contains("shell command"),
        "no bash count: {}",
        out.plain
    );
}

/// A git-push bash call surfaces "Pushed to main"; the branch is parsed
/// from the ref-update line in the result body.
#[test]
fn test_git_push_surfaces_summary() {
    let t = vec![
        TranscriptLine::User("hi".into()),
        tool_call("c1", "bash", "git push", ToolOutcome::Success),
        tool_result_body(
            "c1",
            "To github.com:o/r.git\n   abc..def  main -> main",
            ToolOutcome::Success,
        ),
        TranscriptLine::Agent("done".into()),
    ];
    let g = compute_fold_groups(&t, false);
    let out = render_summary(&g[0].stats, &g[0].git_ops, false);
    assert!(
        out.plain.contains("Pushed to main"),
        "push surfaces: {}",
        out.plain
    );
}

/// A git log command (not a git-op to surface) still counts as a bash
/// command — the SHA in its output does not false-match as a commit.
#[test]
fn test_git_log_counts_bash() {
    let t = vec![
        TranscriptLine::User("hi".into()),
        tool_call("c1", "bash", "git log", ToolOutcome::Success),
        tool_result_body("c1", "[main abc1234] old commit", ToolOutcome::Success),
        TranscriptLine::Agent("done".into()),
    ];
    let g = compute_fold_groups(&t, false);
    assert!(g[0].git_ops.is_empty(), "git log is not a surfaced op");
    assert_eq!(g[0].stats.bash, 1, "git log counts as a bash command");
    let out = render_summary(&g[0].stats, &g[0].git_ops, false);
    assert!(
        out.plain.contains("Ran 1 shell command"),
        "git log is bash: {}",
        out.plain
    );
}

#[test]
fn test_compute_groups_single_turn() {
    let t = vec![
        TranscriptLine::User("hi".into()),
        tool_call("c1", "bash", "ls -la", ToolOutcome::Success),
        tool_result("c1", ToolOutcome::Success),
        tool_call("c2", "read", "a.rs", ToolOutcome::Success),
        tool_result("c2", ToolOutcome::Success),
    ];
    let g = compute_fold_groups(&t, false);
    assert_eq!(g.len(), 1);
    assert_eq!(g[0].start, 1);
    assert_eq!(g[0].end, 5);
    assert_eq!(g[0].stats.list, 1);
    assert_eq!(g[0].stats.read_count(), 1);
    assert!(!g[0].active);
}

#[test]
fn test_compute_groups_error_breaks() {
    let t = vec![
        TranscriptLine::User("hi".into()),
        tool_call("c1", "bash", "ls", ToolOutcome::Success),
        tool_result("c1", ToolOutcome::Success),
        tool_call("c2", "bash", "rm -rf", ToolOutcome::Error),
        tool_result("c2", ToolOutcome::Error),
        tool_call("c3", "read", "b.rs", ToolOutcome::Success),
        tool_result("c3", ToolOutcome::Success),
    ];
    let g = compute_fold_groups(&t, false);
    // Two groups: {c1} and {c3}, with the error call c2 breaking them.
    assert_eq!(g.len(), 2);
    assert_eq!(g[0].start, 1);
    assert_eq!(g[0].end, 3);
    assert_eq!(g[1].start, 5);
    assert_eq!(g[1].end, 7);
}

#[test]
fn test_compute_groups_agent_breaks() {
    let t = vec![
        TranscriptLine::User("hi".into()),
        tool_call("c1", "bash", "ls", ToolOutcome::Success),
        tool_result("c1", ToolOutcome::Success),
        TranscriptLine::Agent("found it".into()),
        tool_call("c2", "read", "b.rs", ToolOutcome::Success),
        tool_result("c2", ToolOutcome::Success),
    ];
    let g = compute_fold_groups(&t, false);
    // Two groups: the Agent line breaks the run.
    assert_eq!(g.len(), 2);
    assert_eq!(g[0].start, 1);
    assert_eq!(g[0].end, 3);
    assert_eq!(g[1].start, 4);
    assert_eq!(g[1].end, 6);
}

#[test]
fn test_compute_groups_active() {
    let t = vec![
        TranscriptLine::User("hi".into()),
        tool_call("c1", "bash", "ls", ToolOutcome::Success),
        tool_result("c1", ToolOutcome::Success),
    ];
    let g = compute_fold_groups(&t, true);
    assert_eq!(g.len(), 1);
    assert!(g[0].active);
    // When not busy, the same group is not active.
    let g2 = compute_fold_groups(&t, false);
    assert!(!g2[0].active);
}

#[test]
fn test_compute_groups_active_last() {
    let t = vec![
        TranscriptLine::User("first".into()),
        tool_call("c1", "bash", "ls", ToolOutcome::Success),
        tool_result("c1", ToolOutcome::Success),
        TranscriptLine::User("second".into()),
        tool_call("c2", "read", "b.rs", ToolOutcome::Success),
        tool_result("c2", ToolOutcome::Success),
    ];
    let g = compute_fold_groups(&t, true);
    assert_eq!(g.len(), 2);
    assert!(!g[0].active, "first turn is not active");
    assert!(g[1].active, "last turn is active");
}

#[test]
fn test_accumulate_brief_matches_accumulate() {
    let mut a = ToolStats::default();
    accumulate(&mut a, "bash", &serde_json::json!({"command": "grep foo"}));
    let mut b = ToolStats::default();
    accumulate_brief(&mut b, "bash", "grep foo");
    assert_eq!(a, b);
}

#[test]
fn test_slots_collapsed_single_turn() {
    let t = vec![
        TranscriptLine::User("hi".into()),
        tool_call("c1", "bash", "ls -la", ToolOutcome::Success),
        tool_result("c1", ToolOutcome::Success),
        tool_call("c2", "read", "a.rs", ToolOutcome::Success),
        tool_result("c2", ToolOutcome::Success),
        TranscriptLine::Agent("done".into()),
    ];
    let expanded = HashSet::new();
    let slots = display_slots(&t, false, &expanded, false);
    assert_eq!(slots.len(), 3);
    assert!(matches!(slots[0], DisplaySlot::Line(0, _)));
    assert!(matches!(slots[1], DisplaySlot::Summary(_)));
    assert!(matches!(slots[2], DisplaySlot::Line(5, _)));
}

#[test]
fn test_slots_expanded_summary_header() {
    // Expanded group: the Summary stays as a clickable collapse-handle
    // header, then each line.
    let t = vec![
        TranscriptLine::User("hi".into()),
        tool_call("c1", "bash", "ls", ToolOutcome::Success),
        tool_result("c1", ToolOutcome::Success),
        TranscriptLine::Agent("done".into()),
    ];
    let mut expanded = HashSet::new();
    expanded.insert("c1#0".to_string());
    let slots = display_slots(&t, false, &expanded, false);
    assert_eq!(slots.len(), 5);
    assert!(matches!(slots[0], DisplaySlot::Line(0, _)));
    assert!(matches!(slots[1], DisplaySlot::Summary(_)));
    assert!(matches!(slots[2], DisplaySlot::Line(1, _)));
    assert!(matches!(slots[3], DisplaySlot::Line(2, _)));
    assert!(matches!(slots[4], DisplaySlot::Line(3, _)));
}

#[test]
fn test_slots_active_single_call() {
    let t = vec![
        TranscriptLine::User("hi".into()),
        tool_call("c1", "bash", "ls", ToolOutcome::Success),
        tool_result("c1", ToolOutcome::Success),
    ];
    let expanded = HashSet::new();
    let slots = display_slots(&t, true, &expanded, false);
    assert_eq!(slots.len(), 3);
    assert!(matches!(slots[0], DisplaySlot::Line(0, _)));
    assert!(matches!(slots[1], DisplaySlot::Line(1, _)));
    assert!(matches!(slots[2], DisplaySlot::Line(2, _)));
}

#[test]
fn test_slots_active_multi_call() {
    // Active (in-flight) multi-call group: show each call directly — active
    // groups stay expanded, not buried behind a "Running N
    // commands" summary; we show each call + its folded result as it
    // lands, so the user tracks progress. No Summary header while
    // active; the group collapses to the summary only once completed.
    let t = vec![
        TranscriptLine::User("hi".into()),
        tool_call("c1", "bash", "ls", ToolOutcome::Success),
        tool_result("c1", ToolOutcome::Success),
        tool_call("c2", "bash", "find .", ToolOutcome::Success),
        tool_result("c2", ToolOutcome::Success),
        tool_call("c3", "read", "a.rs", ToolOutcome::Success),
        tool_result("c3", ToolOutcome::Success),
    ];
    let expanded = HashSet::new();
    let slots = display_slots(&t, true, &expanded, false);
    // Line(0) user + all 6 tool lines (no summary while active).
    assert_eq!(slots.len(), 7);
    assert!(matches!(slots[0], DisplaySlot::Line(0, _)));
    assert!(matches!(slots[1], DisplaySlot::Line(1, _)));
    assert!(matches!(slots[6], DisplaySlot::Line(6, _)));
    // Expanding is a no-op while active (already showing each line).
    let mut exp = HashSet::new();
    exp.insert("c1#0".to_string());
    let slots2 = display_slots(&t, true, &exp, false);
    assert_eq!(slots2.len(), 7);
}
