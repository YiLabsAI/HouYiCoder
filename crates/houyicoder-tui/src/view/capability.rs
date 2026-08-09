//! Capability panes: each slash-driven capability renders its artifact plus an
//! approval affordance. Panes: spec, plan, diff (full-width, side-by-side
//! rationale-evidence and patch), review findings, verify result, code graph,
//! memory, agents. All data is placeholder.
//!
//! The per-hunk evidence diff lives here: the diff pane left column is a
//! per-hunk evidence chain (spec clause + review finding + test ids, each a
//! click-jump stub) plus inline approve/reject affordances. The right column
//! is the patch.

use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, BorderType, Borders, List, ListItem, Paragraph, Wrap},
};

use crate::state::enums::CyclicTab;
use crate::state::{App, Pane, PermissionInput, PermissionTab, Stage, Verdict};
use crate::view::components;
use crate::view::status;
use houyicoder_protocol::frontend::permission::{
    PermissionDecisionEntry, PermissionRule, PermissionRuleContent,
};

/// Render the capability pane for the current app.pane (right column).
pub fn draw(f: &mut Frame, area: Rect, app: &App) {
    match app.pane {
        Pane::Spec => draw_spec(f, area, app),
        Pane::Plan => draw_plan(f, area, app),
        Pane::Review => draw_review(f, area, app),
        Pane::Verify => draw_verify(f, area, app),
        Pane::Graph => draw_graph(f, area, app),
        Pane::Agents => draw_agents(f, area, app),
        Pane::Artifact => crate::view::artifact::draw(f, area, app),
        // The interactive rule manager. draw_main intercepts Pane::Permission
        // and wraps this in the Pane frame (─ + cleared region); this arm
        // fills the content only, for any direct caller that already holds the
        // framed inner rect.
        Pane::Permission => draw_permission_content(f, area, app),
        // Diff, Transcript, Memory, Worktree, Trajectory, Status, and Resume
        // are handled by the working surface layout (draw_main / draw_focus_main
        // route them to the pane template). This arm is a no-op fallback for a
        // direct caller that already holds the framed rect — the pane template
        // owns the real render.
        Pane::Diff
        | Pane::Transcript
        | Pane::Memory
        | Pane::Worktree
        | Pane::Trajectory
        | Pane::Status
        | Pane::Resume
        | Pane::Hooks
        | Pane::Model => {}
    }
}

/// The /permissions content (no frame): a 5-tab header prefixed "Permissions:",
/// a one-line per-tab description, a resident SearchBox on the rule tabs, a
/// numbered rule list with a cyan cursor, and a dim footer. The frame (the
/// ─ Divider + cleared region) is drawn by the Pane primitive in
/// draw_permission_pane; this fills the padded inner rect. Rule surface only
/// — the mode pill and the ask-before-git toggle live on the status bar.
pub(crate) fn draw_permission_content(f: &mut Frame, area: Rect, app: &App) {
    let tab = app.permission_tab;
    let rules = crate::permission_input::filtered_rules(
        tab,
        &app.rules_cache,
        crate::permission_input::search_query(app),
    );
    let denials = filtered_denials(&app.verdict_log_cache);
    // Clamp the cursor against the list the focused tab actually renders —
    // Workspace shows dirs (not rules), Recent shows denials, the rule tabs
    // show rules. Clamping against max(rules, denials) let the Workspace
    // cursor run past the dirs list (highlight vanished) when the denial log
    // was longer than the dirs list.
    let list_len = match tab {
        PermissionTab::Workspace => app.dirs_cache.len(),
        PermissionTab::Recent => denials.len(),
        PermissionTab::Allow | PermissionTab::Ask | PermissionTab::Deny => rules.len(),
    };
    let cursor = app.permission_cursor.min(list_len.saturating_sub(1));
    let prompt = crate::permission_input::permission_prompt(app);

    // tab header (1) + per-tab description (1) + SearchBox on rule tabs (3) +
    // Add/Remove prompt (1 when active; Search has no prompt — the SearchBox
    // is its affordance) + body + footer (1). Absent slots are Length(0) so
    // the body absorbs the space; the slots stay in a fixed order.
    let search_h = if tab.is_rule_tab() { 3 } else { 0 };
    let prompt_h = match app.permission_input {
        PermissionInput::Add
        | PermissionInput::AddDir
        | PermissionInput::AddDestination { .. }
        | PermissionInput::Remove { .. }
        | PermissionInput::RemoveDir { .. } => 1,
        _ => 0,
    };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(search_h),
            Constraint::Length(prompt_h),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(area);

    f.render_widget(permission_tab_header(tab), chunks[0]);
    f.render_widget(
        Paragraph::new(permission_tab_description(tab)).style(Style::new().fg(Color::DarkGray)),
        chunks[1],
    );
    if search_h > 0 {
        render_search_box(f, chunks[2], app);
    }
    if prompt_h > 0
        && let Some(p) = prompt
    {
        f.render_widget(
            Paragraph::new(p).style(Style::new().fg(Color::Yellow)),
            chunks[3],
        );
    }

    let body = match tab {
        PermissionTab::Recent => permission_denial_list(&denials, cursor),
        PermissionTab::Workspace => permission_workspace_body(app, cursor),
        _ => permission_rule_list(&rules, cursor),
    };
    f.render_widget(body, chunks[4]);
    let footer = Paragraph::new(permission_footer(tab)).style(Style::new().fg(Color::DarkGray));
    f.render_widget(footer, chunks[5]);
}

/// The tab header row: a "Permissions:" prefix (the Tabs title, left-aligned)
/// then the five tabs in ORDER, the active one bracketed bold cyan. Picking
/// up the enum ORDER means a reorder there flows here without an edit.
fn permission_tab_header(tab: PermissionTab) -> Paragraph<'static> {
    let mut spans: Vec<Span<'static>> = vec![Span::styled(
        "Permissions:  ",
        Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
    )];
    for (i, t) in PermissionTab::ORDER.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
        }
        let active = *t == tab;
        let label = t.label();
        let span = if active {
            Span::styled(
                format!("[{label}]"),
                Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            )
        } else {
            Span::styled(format!(" {label} "), Style::new().fg(Color::DarkGray))
        };
        spans.push(span);
    }
    Paragraph::new(Line::from(spans))
}

/// The one-line description shown at the top of each tab's body. Wording
/// follows the rule-manager per-tab headers (subject rewritten to
/// "the agent"; no product names).
fn permission_tab_description(tab: PermissionTab) -> &'static str {
    match tab {
        PermissionTab::Allow => "The agent won't ask before using these allowed tools.",
        PermissionTab::Ask => "The agent will always ask before using these tools.",
        PermissionTab::Deny => "The agent will always reject these tools.",
        PermissionTab::Recent => "Commands the auto mode denied will appear here.",
        PermissionTab::Workspace => "Directories the agent can reach beyond the working directory.",
    }
}

/// Render the resident SearchBox: a rounded border with the ⌕ glyph and either
/// the live query (when the Search sub-mode is active) or a dim "Search…"
/// placeholder. Cyan when active, dark gray otherwise. The query lives in the
/// pane-local permission_search buffer (decoupled from the main input box so
/// search never eats the slash a slash command needs).
fn render_search_box(f: &mut Frame, area: Rect, app: &App) {
    let active = matches!(app.permission_input, PermissionInput::Search);
    let query = if active {
        app.permission_search.clone()
    } else {
        String::new()
    };
    let border_fg = if active { Color::Cyan } else { Color::DarkGray };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(border_fg));
    let glyph = "⌕ ";
    let text = if query.is_empty() {
        format!("{glyph}Search…")
    } else {
        format!("{glyph}{query}")
    };
    let style = if active {
        Style::new().fg(Color::Cyan)
    } else {
        Style::new().fg(Color::DarkGray)
    };
    f.render_widget(Paragraph::new(text).style(style), block.inner(area));
    f.render_widget(block, area);
}

/// A rule list body: one numbered row per rule in the rule-label format
/// (e.g. "Bash(npm:*)"), the cursor row cyan. Empty state when the tab has no
/// rules or the search filter matched nothing.
fn permission_rule_list(rules: &[&PermissionRule], cursor: usize) -> Paragraph<'static> {
    if rules.is_empty() {
        return Paragraph::new("(no rules — the mode default applies)")
            .style(Style::new().fg(Color::DarkGray));
    }
    let mut lines: Vec<Line<'static>> = Vec::new();
    for (i, r) in rules.iter().enumerate() {
        let label = permission_rule_label(r);
        let row = format!("{}. {label}", i + 1);
        let style = if i == cursor {
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(Color::White)
        };
        lines.push(Line::from(row).style(style));
    }
    Paragraph::new(lines)
}

/// The rule label: tool name with a capitalized first letter, the
/// content pattern in parens (exact value as-is, prefix as "value:*", glob as
/// the pattern). A tool-level rule has no parens.
fn permission_rule_label(r: &PermissionRule) -> String {
    let mut chars = r.action.chars();
    let first = chars.next();
    let action = match first {
        Some(c) => format!("{}{}", c.to_ascii_uppercase(), chars.as_str()),
        None => String::new(),
    };
    let body = match &r.content {
        None => action,
        Some(PermissionRuleContent::Exact { value }) => format!("{action}({value})"),
        Some(PermissionRuleContent::Prefix { value }) => format!("{action}({value}:*)"),
        Some(PermissionRuleContent::Glob { value }) => format!("{action}({value})"),
    };
    // Append the persistence scope so two rules that differ only by destination
    // (project vs user vs local) are distinguishable in the list — the
    // rule manager shows the scope per row.
    format!("{body} · {}", destination_label(r.destination))
}

/// The short persistence-scope label appended to a rule row.
fn destination_label(
    d: houyicoder_protocol::frontend::permission::RuleDestination,
) -> &'static str {
    use houyicoder_protocol::frontend::permission::RuleDestination;
    match d {
        RuleDestination::Project => "project",
        RuleDestination::User => "user",
        RuleDestination::Local => "local",
        RuleDestination::Session => "session",
        RuleDestination::Builtin => "builtin",
    }
}

/// The denial log entries for the Recently-denied tab. Only verdicts the
/// engine labeled "deny" surface here — the auto mode's refusals, not every
/// decision. Listing every verdict would duplicate what the rule tabs already
/// show for allow/ask.
fn filtered_denials(verdicts: &[PermissionDecisionEntry]) -> Vec<&PermissionDecisionEntry> {
    verdicts
        .iter()
        .filter(|v| v.verdict.eq_ignore_ascii_case("deny"))
        .collect()
}

/// The Recently-denied tab body: one numbered row per denial (a red ✗ marker,
/// the tool, the scope), the cursor row cyan. Empty state names what the tab
/// collects.
fn permission_denial_list(
    denials: &[&PermissionDecisionEntry],
    cursor: usize,
) -> Paragraph<'static> {
    if denials.is_empty() {
        return Paragraph::new("(no recent denials — auto-mode refusals will appear here)")
            .style(Style::new().fg(Color::DarkGray));
    }
    let mut lines: Vec<Line<'static>> = Vec::new();
    for (i, v) in denials.iter().enumerate() {
        let row = format!("{}. ✗ {}  {}", i + 1, v.tool, v.scope);
        let style = if i == cursor {
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(Color::White)
        };
        lines.push(Line::from(row).style(style));
    }
    Paragraph::new(lines)
}

/// The Workspace tab body: the original working directory at the top (the
/// non-deletable root the agent always touches), then one row per directory
/// the user added at runtime (cursor row cyan). Empty state names what the
/// tab collects; the Add/Remove keys (a / d) drive the flows owned in
/// permission_input.
fn permission_workspace_body(app: &App, cursor: usize) -> Paragraph<'static> {
    let cwd = if app.working_dir.is_empty() {
        "(unknown)"
    } else {
        &app.working_dir
    };
    let mut lines: Vec<Line<'static>> = vec![
        Line::from(format!("- {cwd}  (original working directory)"))
            .style(Style::new().fg(Color::White)),
        Line::from("").style(Style::new().fg(Color::White)),
    ];
    if app.dirs_cache.is_empty() {
        lines.push(
            Line::from("(no additional directories — a to add one)")
                .style(Style::new().fg(Color::DarkGray)),
        );
    } else {
        for (i, dir) in app.dirs_cache.iter().enumerate() {
            let row = format!("- {dir}");
            let style = if i == cursor {
                Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(Color::White)
            };
            lines.push(Line::from(row).style(style));
        }
    }
    Paragraph::new(lines)
}

/// The footer nav hint, context-sensitive to the tab. Recently-denied is
/// nav-only (no add/remove/search); the rule tabs and Workspace add the
/// add/remove keys (Workspace removes a directory, not a rule). "Esc to
/// cancel" follows the conventional wording.
fn permission_footer(tab: PermissionTab) -> Line<'static> {
    let hint = match tab {
        PermissionTab::Recent => "↑↓ navigate  ←→ tabs  Esc to cancel",
        PermissionTab::Workspace => "↑↓ navigate  ←→ tabs  a add dir  d remove  Esc to cancel",
        _ => "↑↓ navigate  ←→ tabs  a add  d remove  s search  Esc to cancel",
    };
    Line::from(hint.to_string())
}

/// A border block whose title fuses the progress bar in Focus mode (so the
/// header collapses into the pane title, 0 extra rows) and uses the plain
/// title in Working mode. base is the pane title without surrounding spaces.
fn focus_titled_block(app: &App, base: &str) -> Block<'static> {
    let title = if status::is_focus(app) {
        format!(" {} | {} ", status::progress_str(app), base)
    } else {
        format!(" {base} ")
    };
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(title)
}

/// Full-width per-hunk diff approval: evidence chain
/// on the left, patch on the right. The header shows the focused hunk and its
/// approval state; in Focus mode the progress bar is fused into the title.
/// The left column ends with inline approve/reject buttons.
pub fn draw_diff_full(f: &mut Frame, area: Rect, app: &App) {
    let total = app.diff.hunks.len();
    let idx = app.diff.focus.min(total.saturating_sub(1));
    let approved = app.diff.current().map_or(Verdict::Pending, |h| h.approved);
    let state_label = match approved {
        Verdict::Pending => "pending",
        Verdict::Approved => "approved",
        Verdict::Rejected => "rejected",
    };
    let base = format!(
        "diff approval | {} | change {}/{} | {}",
        app.diff.path,
        idx + 1,
        total,
        state_label,
    );
    let title = if status::is_focus(app) {
        format!(" {} | {} ", status::progress_str(app), base)
    } else {
        format!(" {base} ")
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(title);
    let inner = block.inner(area);
    f.render_widget(block, area);
    // Side-by-side rationale + patch reads cramped below ~100 cols: each
    // column gets too narrow for the patch text to stay readable. Below that
    // width stack rationale above patch (vertical) so the patch keeps the full
    // inner width; at 100+ cols keep the side-by-side layout.
    if inner.width < 100 {
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
            .split(inner);
        draw_evidence(f, rows[0], app);
        draw_patch(f, rows[1], app);
    } else {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(inner);
        draw_evidence(f, cols[0], app);
        draw_patch(f, cols[1], app);
    }
}

/// Left column: the evidence chain for the focused hunk, rendered as labeled
/// lines (spec / review / test / why) plus an approve/reject button row.
fn draw_evidence(f: &mut Frame, area: Rect, app: &App) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(" rationale (evidence) ");
    let inner = block.inner(area);
    f.render_widget(block, area);
    let Some(h) = app.diff.current() else {
        f.render_widget(
            Paragraph::new("(no changes)").style(Style::new().fg(Color::DarkGray)),
            inner,
        );
        return;
    };
    let ev = &h.evidence;
    let white = Style::new().fg(Color::White);
    let lines: Vec<Line> = vec![
        Line::from(vec![
            Span::styled(
                format!("{}  ", h.id),
                Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("{}:{}", h.file, h.range), white),
        ]),
        Line::from(format!(
            "  spec:   {} ({})",
            ev.spec_clause_id, ev.spec_clause_desc
        ))
        .style(white),
        Line::from(format!("  review: {} ({})", ev.finding_id, ev.finding_desc)).style(white),
        Line::from(format!("  test:   {}", ev.test_id)).style(white),
        Line::from(format!("  why:    {}", ev.why)).style(white),
        Line::from(""),
        components::approve_reject_row(h.approved),
    ];
    f.render_widget(
        Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }),
        inner,
    );
}

/// Right column: the patch text for the focused change.
fn draw_patch(f: &mut Frame, area: Rect, app: &App) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(" patch (what) ");
    let patch = app
        .diff
        .current()
        .map(|h| h.patch.as_str())
        .unwrap_or("(no patch)");
    f.render_widget(
        Paragraph::new(patch)
            .style(Style::new().fg(Color::White))
            .wrap(Wrap { trim: false }),
        block.inner(area),
    );
    f.render_widget(block, area);
}

fn draw_spec(f: &mut Frame, area: Rect, app: &App) {
    let block = titled_block(app, "spec draft");
    let a = &app.spec_artifact;
    let mut lines: Vec<Line> = vec![
        kv_line("id", &a.id),
        kv_line("title", &a.title),
        components::approval_line(a.approved, "spec"),
    ];
    lines.push(Line::from("").style(Style::new().fg(Color::White)));
    lines.push(section("acceptance:"));
    for x in &a.acceptance {
        lines.push(bullet(x));
    }
    lines.push(Line::from("").style(Style::new().fg(Color::White)));
    lines.push(section("contract:"));
    for x in &a.contract {
        lines.push(bullet(x));
    }
    lines.push(Line::from("").style(Style::new().fg(Color::White)));
    lines.push(section("test plan:"));
    for x in &a.test_plan {
        lines.push(bullet(x));
    }
    lines.push(Line::from(""));
    lines.push(clauses_block(app));
    lines.push(approve_hint(
        app.stage == Stage::Design,
        "a: approve design -> implement",
    ));
    render_lines(f, area, block, lines);
}

fn draw_plan(f: &mut Frame, area: Rect, app: &App) {
    let block = titled_block(app, "plan draft");
    let p = &app.plan_artifact;
    let mut lines: Vec<Line> = vec![
        kv_line("id", &p.id),
        components::approval_line(p.approved, "plan"),
        Line::from("").style(Style::new().fg(Color::White)),
        section("steps:"),
    ];
    for (i, s) in p.steps.iter().enumerate() {
        lines.push(Line::from(format!("  {}. {s}", i + 1)).style(Style::new().fg(Color::White)));
    }
    lines.push(Line::from(""));
    lines.push(approve_hint(
        app.stage == Stage::Design,
        "a: approve design -> implement",
    ));
    render_lines(f, area, block, lines);
}

fn draw_review(f: &mut Frame, area: Rect, app: &App) {
    let block = titled_block(app, "review findings");
    let inner = block.inner(area);
    f.render_widget(block, area);
    let Some(r) = app.review.current() else {
        f.render_widget(
            Paragraph::new("(no findings)").style(Style::new().fg(Color::DarkGray)),
            inner,
        );
        return;
    };
    let white = Style::new().fg(Color::White);
    let dim = Style::new().fg(Color::Gray);
    let lines: Vec<Line> = vec![
        Line::from(vec![
            Span::styled(
                format!("finding {}  ", r.id),
                Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("[{}]  ", r.lens), white),
            Span::styled(
                format!("verdict: {}  ", r.verdict),
                components::verdict_style(&r.verdict),
            ),
            Span::styled(
                format!("severity: {}", r.severity),
                components::severity_style(&r.severity),
            ),
        ]),
        Line::from(format!(
            "  evidence: {} | spec {} | test {}",
            r.hunk_id, r.spec_clause_id, r.test_id
        ))
        .style(white),
        Line::from(format!("  note:     {}", r.note)).style(white),
        Line::from(""),
        components::consensus_line(&app.review.findings),
        Line::from(format!("  approved: {}", r.signoff_label())).style(white),
        Line::from(""),
        components::signoff_row(r.signoff),
        Line::from(""),
        Line::from(format!(
            "  finding {}/{}  (Up/Down=navigate a=approve r=reject i=rework)",
            app.review.focus + 1,
            app.review.len()
        ))
        .style(dim),
    ];
    f.render_widget(
        Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }),
        inner,
    );
}

fn draw_verify(f: &mut Frame, area: Rect, app: &App) {
    let block = titled_block(app, "verify result");
    let v = &app.verify_result;
    let pass_color = if v.passed { Color::Green } else { Color::Red };
    let mut lines: Vec<Line> = vec![
        Line::from(vec![
            Span::styled("passed: ", Style::new().fg(Color::White)),
            Span::styled(
                if v.passed { "true" } else { "false" },
                Style::new().fg(pass_color).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from("").style(Style::new().fg(Color::White)),
        section("checks:"),
    ];
    for c in &v.checks {
        lines.push(bullet(c));
    }
    lines.push(Line::from(""));
    let done = app.stage == Stage::Done;
    let completion = if done {
        Span::styled(
            "DONE: all checks green, spec satisfied",
            Style::new().fg(Color::Green).add_modifier(Modifier::BOLD),
        )
    } else if v.passed {
        Span::styled(
            "a: complete verification -> done",
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        )
    } else {
        Span::styled(
            "checks failed -- r: rework -> back to implementing",
            Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
        )
    };
    lines.push(Line::from(completion));
    render_lines(f, area, block, lines);
}

fn draw_graph(f: &mut Frame, area: Rect, app: &App) {
    let block = titled_block(app, "code graph");
    let g = &app.graph_result;
    let mut lines: Vec<Line> = vec![
        Line::from(format!("query: {}", g.query)).style(Style::new().fg(Color::White)),
        Line::from("").style(Style::new().fg(Color::White)),
        section("impact set (affected symbols):"),
    ];
    for (i, p) in g.impact.iter().enumerate() {
        lines.push(Line::from(format!("  {}. {p}", i + 1)).style(Style::new().fg(Color::White)));
    }
    render_lines(f, area, block, lines);
}

fn draw_agents(f: &mut Frame, area: Rect, app: &App) {
    let block = titled_block(app, "agents");
    let items: Vec<ListItem> = app
        .agents
        .iter()
        .map(|a| ListItem::new(format!("{} ({}) -- {}", a.name, a.role, a.state)))
        .collect();
    f.render_widget(
        List::new(items).style(Style::new().fg(Color::White)),
        block.inner(area),
    );
    f.render_widget(block, area);
}

fn titled_block(app: &App, base: &str) -> Block<'static> {
    focus_titled_block(app, base)
}

fn render_lines(f: &mut Frame, area: Rect, block: Block, lines: Vec<Line>) {
    f.render_widget(
        Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }),
        block.inner(area),
    );
    f.render_widget(block, area);
}

fn kv_line(k: &str, v: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{k}: "), Style::new().fg(Color::DarkGray)),
        Span::styled(v.to_string(), Style::new().fg(Color::White)),
    ])
}

fn section(s: &str) -> Line<'static> {
    Line::from(s.to_string()).style(Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD))
}

fn bullet(s: &str) -> Line<'static> {
    Line::from(format!("  - {s}")).style(Style::new().fg(Color::White))
}

fn approve_hint(active: bool, msg: &str) -> Line<'static> {
    let style = if active {
        Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else {
        Style::new().fg(Color::DarkGray)
    };
    Line::from(format!("  {msg}")).style(style)
}

/// One-line summary of clause statuses for the spec pane body.
fn clauses_block(app: &App) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = vec![Span::styled(
        "  clauses: ",
        Style::new().fg(Color::DarkGray),
    )];
    for (i, c) in app.spec_clauses.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw(" "));
        }
        let focused = i == app.spec_ctx.clause_focus;
        let style = if focused {
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(Color::DarkGray)
        };
        spans.push(Span::styled(
            format!("{} {}", c.id, c.status.label()),
            style,
        ));
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_review_uses_shared_verdict() {
        // The review pane delegates to the shared component, so a real verdict
        // is Cyan (monochrome), not Red.
        assert_eq!(components::verdict_style("real").fg, Some(Color::Cyan));
        assert_eq!(
            components::verdict_style("refuted").fg,
            Some(Color::DarkGray)
        );
    }

    #[test]
    fn test_signoff_row_buttons() {
        let line = components::signoff_row(Verdict::Pending);
        let joined: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(joined.contains("approve"));
        assert!(joined.contains("reject"));
    }
}
