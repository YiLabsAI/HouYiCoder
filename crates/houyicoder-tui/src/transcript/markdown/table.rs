//! The ASCII table the transcript draws for a markdown table: column widths,
//! cell wrapping, and the stacked form a pane too narrow for columns falls
//! back to.

use crate::view::line_wrap::wrap_line;
use pulldown_cmark::Alignment;
use unicode_width::UnicodeWidthStr;

/// Render a markdown table as ASCII: header row, separator, data rows.
///
/// Column widths are the widest cell per column, then shrunk to the width the
/// pane can spare. A cell wider than its column is wrapped inside the cell and
/// the row grows downward, so the pipe separators stay in one column on every
/// visual row. Wrapping the finished table as prose would break the header's
/// trailing pipe and every column after it.
pub(super) fn render_table(
    rows: &[Vec<String>],
    aligns: &[Alignment],
    avail: usize,
) -> Vec<String> {
    if rows.is_empty() || rows[0].is_empty() {
        return Vec::new();
    }
    let ncols = rows[0].len();
    let mut widths: Vec<usize> = (0..ncols)
        .map(|c| {
            let mut w = 3usize;
            for row in rows {
                if let Some(cell) = row.get(c) {
                    w = w.max(UnicodeWidthStr::width(cell.as_str()));
                }
            }
            w
        })
        .collect();
    const MIN_COL: usize = 4;
    let spent = |widths: &[usize]| widths.iter().sum::<usize>() + 3 * ncols + 1;
    if avail > 0 {
        while spent(&widths) > avail {
            let Some((widest, _)) = widths.iter().enumerate().max_by_key(|(_, w)| **w) else {
                break;
            };
            if widths[widest] <= MIN_COL {
                break;
            }
            widths[widest] -= 1;
        }
        if spent(&widths) > avail {
            return stacked_table(rows, avail);
        }
    }
    let align_of = |c: usize| aligns.get(c).copied().unwrap_or(Alignment::None);
    let mut out = Vec::new();
    for (r, row) in rows.iter().enumerate() {
        let cells: Vec<Vec<String>> = (0..ncols)
            .map(|c| {
                let cell = row.get(c).map(String::as_str).unwrap_or("");
                wrap_line(cell, widths[c])
            })
            .collect();
        let height = cells.iter().map(Vec::len).max().unwrap_or(1);
        for line_idx in 0..height {
            let mut line = String::from("| ");
            for (c, cell_rows) in cells.iter().enumerate() {
                let text = cell_rows.get(line_idx).map(String::as_str).unwrap_or("");
                line.push_str(&pad_cell(text, widths[c], align_of(c)));
                line.push_str(" | ");
            }
            out.push(line.trim_end().to_string());
        }
        if r == 0 {
            let mut sep = String::from('|');
            for w in &widths {
                sep.push_str(&"-".repeat(w + 2));
                sep.push('|');
            }
            out.push(sep);
        }
    }
    out
}

/// The table a pane too narrow for any column layout falls back to: one
/// header-and-value pair per line, wrapped to the pane. A table that cannot
/// keep its columns aligned must stop pretending to be a table.
fn stacked_table(rows: &[Vec<String>], avail: usize) -> Vec<String> {
    let headers = &rows[0];
    if rows.len() == 1 {
        // A header with no body is still content: state its columns instead of
        // dropping the table.
        return wrap_line(&headers.join(" | "), avail);
    }
    let mut out = Vec::new();
    for (r, row) in rows.iter().enumerate().skip(1) {
        if r > 1 {
            out.push(String::new());
        }
        for (c, cell) in row.iter().enumerate() {
            let label = headers.get(c).map(String::as_str).unwrap_or("");
            let prefix = format!("{label}: ");
            let indent = UnicodeWidthStr::width(prefix.as_str());
            // A label wider than the pane leaves no hanging column: wrap the
            // pair as one line instead of printing a prefix that already runs
            // past the edge.
            if indent + 4 > avail {
                out.extend(wrap_line(&format!("{prefix}{cell}"), avail));
                continue;
            }
            let budget = avail - indent;
            for (i, line) in wrap_line(cell, budget).iter().enumerate() {
                if i == 0 {
                    out.push(format!("{prefix}{line}"));
                } else {
                    out.push(format!("{}{}", " ".repeat(indent), line));
                }
            }
        }
    }
    out
}

/// Pad a cell to the column width per its alignment. Left = trailing spaces,
/// right = leading spaces, center = split. None and Left both render
/// left-aligned. The pad-aligned rule: dash-only separator, no
/// alignment colons rendered.
fn pad_cell(content: &str, width: usize, align: Alignment) -> String {
    let display_w = UnicodeWidthStr::width(content);
    let pad = width.saturating_sub(display_w);
    match align {
        Alignment::Center => {
            let left = pad / 2;
            format!("{}{}{}", " ".repeat(left), content, " ".repeat(pad - left))
        }
        Alignment::Right => format!("{}{}", " ".repeat(pad), content),
        Alignment::Left | Alignment::None => {
            format!("{content}{}", " ".repeat(pad))
        }
    }
}
