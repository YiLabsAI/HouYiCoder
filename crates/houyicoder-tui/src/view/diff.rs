//! A before-and-after pair compared line by line. Only the lines that left
//! and the lines that arrived are marked, with a few unchanged lines around
//! each change for orientation and a count standing in for the stretches that
//! did not move. Marking every earlier line as removed and every later one as
//! added would turn one changed line in a long file into twice the file, which
//! says nothing about what changed.

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

use crate::view::{ERROR_RED, SUCCESS_GREEN};

/// The unchanged lines shown on each side of a change, and the point past
/// which the two middles are compared as one block instead of line by line.
const CONTEXT: usize = 2;
const MAX_MIDDLE_CELLS: usize = 250_000;

/// The two versions as marked lines. An absent side is None rather than an
/// empty string: nothing was there, which is not the same as an empty file,
/// and both render as the other side's lines alone.
pub(crate) fn changed_lines(before: Option<&str>, after: Option<&str>) -> Vec<Line<'static>> {
    marked_lines(before, after, true)
}

/// The two versions as marked lines, with every kept line shown. An approval
/// states the bytes a write will leave, so a count standing in for kept lines
/// would ask the user to consent to lines that are not on the page.
pub(crate) fn full_lines(before: Option<&str>, after: Option<&str>) -> Vec<Line<'static>> {
    marked_lines(before, after, false)
}

fn marked_lines(before: Option<&str>, after: Option<&str>, collapse: bool) -> Vec<Line<'static>> {
    let old: Vec<&str> = before
        .map(|text| text.lines().collect())
        .unwrap_or_default();
    let new: Vec<&str> = after.map(|text| text.lines().collect()).unwrap_or_default();
    let steps = diff_steps(&old, &new);
    render(&steps, collapse)
}

/// One line of the comparison: kept, removed, or added.
enum Step<'a> {
    Same(&'a str),
    Gone(&'a str),
    Came(&'a str),
}

/// The two sides as a sequence of kept, removed, and added lines. The matching
/// prefix and suffix are taken off first, so the quadratic comparison only sees
/// the part that actually differs; when even that part is large the two middles
/// are reported as one removed block and one added block, which overstates the
/// change but never understates it.
fn diff_steps<'a>(old: &[&'a str], new: &[&'a str]) -> Vec<Step<'a>> {
    let mut prefix = 0;
    while prefix < old.len() && prefix < new.len() && old[prefix] == new[prefix] {
        prefix += 1;
    }
    let mut suffix = 0;
    while suffix < old.len() - prefix
        && suffix < new.len() - prefix
        && old[old.len() - 1 - suffix] == new[new.len() - 1 - suffix]
    {
        suffix += 1;
    }
    let mut steps: Vec<Step<'a>> = old[..prefix].iter().map(|line| Step::Same(line)).collect();
    steps.extend(middle_steps(
        &old[prefix..old.len() - suffix],
        &new[prefix..new.len() - suffix],
    ));
    steps.extend(
        old[old.len() - suffix..]
            .iter()
            .map(|line| Step::Same(line)),
    );
    steps
}

fn middle_steps<'a>(old: &[&'a str], new: &[&'a str]) -> Vec<Step<'a>> {
    let bounded = old.len().saturating_mul(new.len()) <= MAX_MIDDLE_CELLS;
    if old.is_empty() || new.is_empty() || !bounded {
        let mut steps: Vec<Step<'a>> = old.iter().map(|line| Step::Gone(line)).collect();
        steps.extend(new.iter().map(|line| Step::Came(line)));
        return steps;
    }
    let width = new.len() + 1;
    let mut kept = vec![0u32; (old.len() + 1) * width];
    for i in (0..old.len()).rev() {
        for j in (0..new.len()).rev() {
            kept[i * width + j] = if old[i] == new[j] {
                kept[(i + 1) * width + j + 1] + 1
            } else {
                kept[(i + 1) * width + j].max(kept[i * width + j + 1])
            };
        }
    }
    let mut steps = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < old.len() && j < new.len() {
        if old[i] == new[j] {
            steps.push(Step::Same(old[i]));
            i += 1;
            j += 1;
        } else if kept[(i + 1) * width + j] >= kept[i * width + j + 1] {
            steps.push(Step::Gone(old[i]));
            i += 1;
        } else {
            steps.push(Step::Came(new[j]));
            j += 1;
        }
    }
    steps.extend(old[i..].iter().map(|line| Step::Gone(line)));
    steps.extend(new[j..].iter().map(|line| Step::Came(line)));
    steps
}

fn render(steps: &[Step<'_>], collapse: bool) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut i = 0;
    while i < steps.len() {
        match &steps[i] {
            Step::Same(_) => {
                let start = i;
                while i < steps.len() && matches!(steps[i], Step::Same(_)) {
                    i += 1;
                }
                push_kept_run(
                    &mut lines,
                    &steps[start..i],
                    start > 0,
                    i < steps.len(),
                    collapse,
                );
            }
            Step::Gone(text) => {
                lines.push(marked("-", text, ERROR_RED));
                i += 1;
            }
            Step::Came(text) => {
                lines.push(marked("+", text, SUCCESS_GREEN));
                i += 1;
            }
        }
    }
    lines
}

/// A run of unchanged lines as the part worth showing: the lines next to a
/// change, and a count in place of the rest.
fn push_kept_run(
    out: &mut Vec<Line<'static>>,
    run: &[Step<'_>],
    after_change: bool,
    before_change: bool,
    collapse: bool,
) {
    let head = if !collapse {
        run.len()
    } else if after_change {
        CONTEXT.min(run.len())
    } else {
        0
    };
    let tail = if before_change {
        CONTEXT.min(run.len() - head)
    } else {
        0
    };
    for step in &run[..head] {
        if let Step::Same(text) = step {
            out.push(unchanged_line(text, false));
        }
    }
    let hidden = run.len() - head - tail;
    if hidden > 0 {
        out.push(unchanged_line(&hidden_count(hidden), true));
    }
    for step in &run[run.len() - tail..] {
        if let Step::Same(text) = step {
            out.push(unchanged_line(text, false));
        }
    }
}

fn hidden_count(hidden: usize) -> String {
    let noun = if hidden == 1 { "line" } else { "lines" };
    format!("  … {hidden} unchanged {noun} …")
}

fn marked(mark: &str, text: &str, color: Color) -> Line<'static> {
    Line::from(vec![
        Span::styled(mark.to_string(), Style::new().fg(color)),
        Span::raw(text.to_string()),
    ])
}

/// A kept line, or the count standing in for the kept lines between two
/// changes. Both are shown in the dim tone the marks are read against.
fn unchanged_line(text: &str, count: bool) -> Line<'static> {
    let body = if count {
        text.to_string()
    } else {
        format!(" {text}")
    };
    Line::from(Span::styled(body, Style::new().fg(Color::DarkGray)))
}

#[cfg(test)]
mod tests;
