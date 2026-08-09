//! Render the collapsed turn-summary line: per-bucket counts (memory ops
//! lead, then search/read/list/edit/write/todo/bash/other) plus surfaced
//! git ops, with a "(ctrl+o to expand)" affordance on completed groups.
//! Present participle when the group is active, past tense when completed.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::fold::ToolStats;
use crate::git_op::{self, GitOp};

/// Rendered summary: a styled Line (bold counts + dim verb/noun + a
/// "(ctrl+o to expand)" affordance suffix on completed groups) plus the
/// plain text (no styling, no suffix) used for selection/copy and as the
/// search-highlight fallback. Both come from one walk so they never
/// disagree. Empty (no spans, empty plain) when no calls were counted.
#[derive(Debug)]
pub(crate) struct SummaryRender {
    pub line: Line<'static>,
    pub plain: String,
}

/// One countable part of the summary: a verb ("Searched for"/"searched for"),
/// a count, and a pluralized noun ("pattern"/"patterns"). The first part's
/// verb is capitalized at render time; the rest stay lowercase.
struct SummaryPart {
    verb: &'static str,
    count: u32,
    noun: &'static str,
}

/// Build the per-bucket summary parts in render order. Memory store ops
/// lead the file-op counts (after git ops): a memory save/delete is a
/// meta-operation on the agent's own state, so it reads before "read N
/// files". Memory writes lead search/read counts in the summary.
/// Verbs are present participle when active, past tense when completed;
/// the first part's verb is capitalized by the caller.
fn summary_count_parts(stats: &ToolStats, active: bool) -> Vec<SummaryPart> {
    let (
        search_v,
        read_v,
        list_v,
        bash_v,
        edit_v,
        write_v,
        todo_v,
        other_v,
        mem_write_v,
        mem_delete_v,
    ) = if active {
        (
            "searching for",
            "reading",
            "listing",
            "running",
            "editing",
            "writing",
            "updating",
            "running",
            "writing",
            "deleting",
        )
    } else {
        (
            "searched for",
            "read",
            "listed",
            "ran",
            "edited",
            "wrote",
            "updated",
            "ran",
            "wrote",
            "deleted",
        )
    };
    let read_count = stats.read_count();
    [
        (stats.mem_write > 0, mem_write_v, stats.mem_write, "memory"),
        (
            stats.mem_delete > 0,
            mem_delete_v,
            stats.mem_delete,
            "memory",
        ),
        (stats.search > 0, search_v, stats.search, "pattern"),
        (read_count > 0, read_v, read_count, "file"),
        (stats.list > 0, list_v, stats.list, "directory"),
        (stats.edit > 0, edit_v, stats.edit, "file"),
        (stats.write > 0, write_v, stats.write, "file"),
        (stats.todo > 0, todo_v, stats.todo, "checklist"),
        (stats.bash > 0, bash_v, stats.bash, "command"),
        (stats.other > 0, other_v, stats.other, "tool"),
    ]
    .into_iter()
    .filter_map(|(on, verb, count, noun)| on.then_some(SummaryPart { verb, count, noun }))
    .collect()
}

/// Render the summary line. Present participle when active, past tense when
/// completed. Counts bold within a dim line so the eye latches onto numbers;
/// a "(ctrl+o to expand)" affordance trails completed groups so the collapse
/// affordance is discoverable (a CtrlOToExpand hint trails the summary).
pub(crate) fn render_summary(stats: &ToolStats, git_ops: &[GitOp], active: bool) -> SummaryRender {
    let count_parts = summary_count_parts(stats, active);

    // Git ops lead the line (the load-bearing outcome reads first), then the
    // search/read/bash counts. The first part's verb is capitalized.
    let git_parts = git_op::git_op_parts(git_ops);
    let total_parts = git_parts.len() + count_parts.len();

    let dim = Style::new().fg(Color::DarkGray);
    let bold = Style::new()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::BOLD);
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut plain_parts: Vec<String> = Vec::with_capacity(total_parts);
    let mut first = true;
    for (verb, value) in &git_parts {
        if !first {
            spans.push(Span::styled(", ", dim));
        }
        let v = if first { cap_first(verb) } else { verb.clone() };
        spans.push(Span::styled(format!("{v} "), dim));
        spans.push(Span::styled(value.clone(), bold));
        plain_parts.push(format!("{v} {value}"));
        first = false;
    }
    for p in &count_parts {
        if !first {
            spans.push(Span::styled(", ", dim));
        }
        let v = if first {
            cap_first(p.verb)
        } else {
            p.verb.to_string()
        };
        let n = plural(p.count, p.noun);
        let (qualifier, noun_str): (&str, &str) = match p.verb {
            "ran" | "running" if p.noun == "command" => (" shell ", &n),
            "ran" | "running" if p.noun == "tool" => (" other ", &n),
            _ => (" ", &n),
        };
        spans.push(Span::styled(format!("{v} "), dim));
        spans.push(Span::styled(p.count.to_string(), bold));
        spans.push(Span::styled(format!("{qualifier}{noun_str}"), dim));
        // bash/other use a compound noun ("shell command" / "other tool"):
        // the count sits between verb and noun qualifier.
        match p.verb {
            "ran" | "running" if p.noun == "command" => {
                plain_parts.push(format!("{v} {} shell {n}", p.count));
            }
            "ran" | "running" if p.noun == "tool" => {
                plain_parts.push(format!("{v} {} other {n}", p.count));
            }
            _ => plain_parts.push(format!("{v} {} {n}", p.count)),
        }
        first = false;
    }
    let plain = plain_parts.join(", ");
    // Completed (non-active) groups get the affordance suffix so the collapse
    // is discoverable; active groups stay bare (they read as "in progress").
    if !active && total_parts > 0 {
        spans.push(Span::styled(" (ctrl+o to expand)", dim));
    }
    SummaryRender {
        line: Line::from(spans),
        plain,
    }
}

fn cap_first(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

fn plural(n: u32, singular: &str) -> String {
    if n == 1 {
        return singular.to_string();
    }
    match singular {
        "directory" => "directories".to_string(),
        "memory" => "memories".to_string(),
        s => format!("{s}s"),
    }
}
