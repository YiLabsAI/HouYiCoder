//! The slash-command pane dispatchers: each renders a transcript tail above a
//! lower band of pane content, through one template so the panes share one
//! shape.

use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::Color,
};

use crate::selection::TAG_PLAIN;
use crate::state::App;
use crate::view::agents_pane::{AGENTS_PANE_HEIGHT, draw_content};
use crate::view::pane::render;
use crate::view::status::pane::{STATUS_PANE_HEIGHT, draw_content as status_draw_content};
use crate::view::{
    capability, hooks_pane, memory_pane, model_pane, queue_pane, resume_picker, sandbox_pane,
    skills_pane, trajectory_pane, worktree_pane,
};

use super::transcript;

/// The shared template: transcript tail above, a capped-height pane below.
pub(super) fn draw_command_pane<C>(f: &mut Frame, area: Rect, app: &App, pane_h: u16, content: C)
where
    C: Fn(&mut Frame, Rect, &App),
{
    let pane_h = pane_h.min(f.area().height / 2).max(8).min(area.height);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(pane_h)])
        .split(area);
    transcript::draw_transcript(f, chunks[0], app);
    render(f, chunks[1], Color::Cyan, |f, inner| {
        content(f, inner, app);
        stash_pane_rows(f, inner, app);
    });
}

/// Stash the rendered pane content rows and inner rect so the in-app
/// selection path can reach text the pane drew.
pub(super) fn stash_pane_rows(f: &mut Frame, inner: Rect, app: &App) {
    app.ui.frame.layout.pane_rect.set(inner);
    let mut rows = app.ui.frame.layout.last_pane_rows.borrow_mut();
    rows.clear();
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let buf = f.buffer_mut();
    for ry in 0..inner.height {
        let y = inner.y + ry;
        let mut row = String::with_capacity(inner.width as usize);
        for rx in 0..inner.width {
            let x = inner.x + rx;
            if let Some(cell) = buf.cell((x, y)) {
                row.push_str(cell.symbol());
            }
        }
        rows.push((TAG_PLAIN, row.trim_end().to_string()));
    }
}

/// Stash the rendered approval card rows + rect for the selection path.
pub(super) fn stash_approval_rows(f: &mut Frame, area: Rect, app: &App) {
    app.ui.frame.layout.approval_rect.set(area);
    let mut rows = app.ui.frame.layout.last_approval_rows.borrow_mut();
    rows.clear();
    if area.width == 0 || area.height == 0 {
        return;
    }
    let buf = f.buffer_mut();
    for ry in 0..area.height {
        let y = area.y + ry;
        let mut row = String::with_capacity(area.width as usize);
        for rx in 0..area.width {
            let x = area.x + rx;
            if let Some(cell) = buf.cell((x, y)) {
                row.push_str(cell.symbol());
            }
        }
        rows.push((TAG_PLAIN, row.trim_end().to_string()));
    }
}

pub(super) fn draw_permission_pane(f: &mut Frame, area: Rect, app: &App) {
    draw_command_pane(
        f,
        area,
        app,
        PERMISSION_PANE_HEIGHT,
        capability::draw_permission_content,
    );
}

const PERMISSION_PANE_HEIGHT: u16 = 18;

pub(super) fn draw_memory_pane(f: &mut Frame, area: Rect, app: &App) {
    draw_command_pane(f, area, app, MEMORY_PANE_HEIGHT, memory_pane::draw_content);
}

const MEMORY_PANE_HEIGHT: u16 = 16;

pub(super) fn draw_queue_pane(f: &mut Frame, area: Rect, app: &App) {
    draw_command_pane(f, area, app, QUEUE_PANE_HEIGHT, queue_pane::draw_content);
}

const QUEUE_PANE_HEIGHT: u16 = 12;

pub(super) fn draw_worktree_pane(f: &mut Frame, area: Rect, app: &App) {
    draw_command_pane(
        f,
        area,
        app,
        WORKTREE_PANE_HEIGHT,
        worktree_pane::draw_content,
    );
}

const WORKTREE_PANE_HEIGHT: u16 = 14;

/// The /trajectory pane at Level 0 shares the template; drilling goes
/// fullscreen.
pub(super) fn draw_trajectory_pane(f: &mut Frame, area: Rect, app: &App) {
    if app.ui.trajectory.level() >= 1 {
        render(f, area, Color::Cyan, |f, inner| {
            trajectory_pane::draw_content(f, inner, app);
            stash_pane_rows(f, inner, app);
        });
    } else {
        draw_command_pane(
            f,
            area,
            app,
            TRAJECTORY_PANE_HEIGHT,
            trajectory_pane::draw_content,
        );
    }
}

const TRAJECTORY_PANE_HEIGHT: u16 = 20;

pub(super) fn draw_status_pane(f: &mut Frame, area: Rect, app: &App) {
    draw_command_pane(f, area, app, STATUS_PANE_HEIGHT, status_draw_content);
}

pub(super) fn draw_resume_pane(f: &mut Frame, area: Rect, app: &App) {
    draw_command_pane(
        f,
        area,
        app,
        RESUME_PANE_HEIGHT,
        resume_picker::draw_content,
    );
}

const RESUME_PANE_HEIGHT: u16 = 16;

pub(super) fn draw_hooks_pane(f: &mut Frame, area: Rect, app: &App) {
    draw_command_pane(
        f,
        area,
        app,
        hooks_pane::HOOKS_PANE_HEIGHT,
        hooks_pane::draw_content,
    );
}

pub(super) fn draw_agents_pane(f: &mut Frame, area: Rect, app: &App) {
    draw_command_pane(f, area, app, AGENTS_PANE_HEIGHT, draw_content);
}

pub(super) fn draw_skills_pane(f: &mut Frame, area: Rect, app: &App) {
    draw_command_pane(
        f,
        area,
        app,
        skills_pane::SKILLS_PANE_HEIGHT,
        skills_pane::draw_content,
    );
}

pub(super) fn draw_model_pane(f: &mut Frame, area: Rect, app: &App) {
    draw_command_pane(
        f,
        area,
        app,
        model_pane::MODEL_PANE_HEIGHT,
        model_pane::draw_content,
    );
}

pub(super) fn draw_sandbox_pane(f: &mut Frame, area: Rect, app: &App) {
    draw_command_pane(
        f,
        area,
        app,
        SANDBOX_PANE_HEIGHT,
        sandbox_pane::draw_content,
    );
}

const SANDBOX_PANE_HEIGHT: u16 = 14;
