//! Shared view components and the single color vocabulary. One
//! implementation of the sign-off / approve-reject row, the verdict and
//! severity styles, and the artifact approval line, used by both the console
//! and the review/diff panes so the approval pattern is identical everywhere.
//!
//! Color vocabulary (governs every screen):
//! - Cyan    accent / focus / active / pending-approve
//! - DarkGray dim / inactive
//! - White   primary text
//! - Green   completed / satisfied / approved
//! - Red     rejected / failed
//! - Yellow  warning only (rare)

use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

use crate::evidence::{ReviewFinding, Verdict};

/// Style for a review verdict. Monochrome: a real finding is the actionable
/// one so it takes the Cyan accent; a refuted finding is dim. No red/green
/// verdict hues (state is conveyed by the text label, not the color).
pub fn verdict_style(verdict: &str) -> Style {
    match verdict {
        "real" => Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        "refuted" => Style::new().fg(Color::DarkGray),
        _ => Style::new().fg(Color::DarkGray),
    }
}

/// Style for a severity label. Monochrome: high is Cyan + BOLD so it reads as
/// the one to act on; medium and info are dim.
pub fn severity_style(sev: &str) -> Style {
    match sev {
        "high" => Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        _ => Style::new().fg(Color::DarkGray),
    }
}

/// The shared approve/reject button row for a pending-or-resolved decision.
/// One approval pattern everywhere: Cyan marks the pending approve action,
/// Red marks the reject action, Green marks a completed approval, and a
/// rejected state is Red. The inactive side dims to DarkGray.
pub fn approve_reject_row(decision: Verdict) -> Line<'static> {
    let approve = match decision {
        Verdict::Approved => Span::styled(
            "[ approved ]",
            Style::new().fg(Color::Green).add_modifier(Modifier::BOLD),
        ),
        Verdict::Pending => Span::styled(
            "[ approve ]",
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ),
        Verdict::Rejected => Span::styled("  approve  ", Style::new().fg(Color::DarkGray)),
    };
    let reject = match decision {
        Verdict::Rejected => Span::styled(
            "[ rejected ]",
            Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
        Verdict::Pending => Span::styled(
            "[ reject ]",
            Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
        Verdict::Approved => Span::styled("  reject  ", Style::new().fg(Color::DarkGray)),
    };
    Line::from(vec![Span::raw("  "), approve, Span::raw("  "), reject])
}

/// The sign-off row variant used by the review console: same colors and
/// labels as the diff approve/reject row so the approval pattern is unified.
/// The sign-off action is labeled "approve" to match the diff row.
pub fn signoff_row(signoff: Verdict) -> Line<'static> {
    let approve = match signoff {
        Verdict::Approved => Span::styled(
            "[ approved ]",
            Style::new().fg(Color::Green).add_modifier(Modifier::BOLD),
        ),
        Verdict::Pending => Span::styled(
            "[ approve ]",
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ),
        Verdict::Rejected => Span::styled("  approve  ", Style::new().fg(Color::DarkGray)),
    };
    let reject = match signoff {
        Verdict::Rejected => Span::styled(
            "[ rejected ]",
            Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
        Verdict::Pending => Span::styled(
            "[ reject -> org eval ]",
            Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
        Verdict::Approved => Span::styled("  reject  ", Style::new().fg(Color::DarkGray)),
    };
    Line::from(vec![Span::raw("  "), approve, Span::raw("  "), reject])
}

/// One-line artifact approval state for the spec and plan panes. Approved is
/// Green (completed); a draft is Cyan (active, pending approval) — not Yellow.
pub fn approval_line(approved: bool, what: &str) -> Line<'static> {
    let (label, color) = if approved {
        ("approved", Color::Green)
    } else {
        ("draft (not approved)", Color::Cyan)
    };
    Line::from(vec![
        Span::styled(format!("{what}: "), Style::new().fg(Color::DarkGray)),
        Span::styled(label, Style::new().fg(color)),
    ])
}

/// The multi-agent consensus line: one chip per review lens (check for
/// refuted / no-issue, cross for real / issue), then the weighted verdict and
/// the real/total tally. Renders the per-lens verdicts as chips so the
/// consensus is readable at a glance.
pub fn consensus_line(findings: &[ReviewFinding]) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = vec![Span::styled(
        "consensus: ",
        Style::new().fg(Color::DarkGray),
    )];
    let total = findings.len();
    let real_count = findings.iter().filter(|f| f.verdict == "real").count();
    for (i, f) in findings.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw(" "));
        }
        let (mark, color) = if f.verdict == "real" {
            ('\u{2717}', Color::Red)
        } else {
            ('\u{2713}', Color::Green)
        };
        spans.push(Span::styled(
            format!("{mark}{}", f.lens),
            Style::new().fg(color),
        ));
    }
    let weighted = if real_count * 2 > total {
        "real"
    } else {
        "refuted"
    };
    let wstyle = if weighted == "real" {
        Style::new().fg(Color::Red).add_modifier(Modifier::BOLD)
    } else {
        Style::new().fg(Color::Green).add_modifier(Modifier::BOLD)
    };
    spans.push(Span::raw(" \u{2192} "));
    spans.push(Span::styled(weighted, wstyle));
    spans.push(Span::styled(
        format!(" ({}/{})", real_count, total),
        Style::new().fg(Color::DarkGray),
    ));
    Line::from(spans)
}

#[cfg(test)]
mod tests;
