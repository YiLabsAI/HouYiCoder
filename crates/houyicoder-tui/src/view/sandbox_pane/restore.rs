//! The restore confirmation: what a restore would do to the focused record,
//! said before the user commits to it.

use houyicoder_protocol::frontend::checkpoint::{CheckpointImage, CheckpointState, CheckpointView};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use super::{CheckpointRow, row_at, target_name, written_label};
use crate::state::App;
use crate::ui::RestoreFocus;
use crate::view::navigation::{block_with_keys, tab_header};

/// The restore confirmation: the focused buttons, what the restore will do to
/// this target, and the last answer the host gave. The buttons start on the
/// one that changes nothing.
pub(super) fn draw_restore(f: &mut Frame, area: Rect, app: &App) {
    let pane = &app.ui.sandbox_pane;
    let listing = pane.checkpoints();
    let row = listing
        .as_ref()
        .and_then(|list| row_at(list, pane.cursor.get()));
    // A restore already with the host is writing the earlier bytes back and
    // cannot be called off, so the buttons that offer a choice come off the
    // page. A Cancel that cannot cancel is worse than no button: the user
    // presses it, believes the restore stopped, and the bytes are back anyway.
    let restoring = pane.restore_request().is_some();
    let mut lines = if restoring {
        vec![tab_header(
            pane.focus.get(),
            &[(RestoreFocus::Restore, "Restore checkpoint")],
        )]
    } else {
        vec![tab_header(
            pane.focus.get(),
            &[
                (RestoreFocus::Cancel, "Cancel"),
                (RestoreFocus::Restore, "Restore checkpoint"),
            ],
        )]
    };
    match row {
        Some(CheckpointRow::Write(view)) => {
            lines.push(Line::from(vec![
                Span::styled(
                    target_name(&view.target),
                    Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(" \u{00b7} {}", written_label(view.written_ms)),
                    Style::new().fg(Color::DarkGray),
                ),
            ]));
            lines.push(Line::from(Span::styled(
                format!("Target:     {}", view.target),
                Style::new().fg(Color::DarkGray),
            )));
            lines.push(Line::from(format!(
                "Before:     {}",
                image_label(&view.before)
            )));
            lines.push(Line::from(restore_effect(view)));
        }
        _ => lines.push(Line::from("(no checkpoint selected)")),
    }
    if pane.restore_request().is_some() {
        lines.push(Line::from(Span::styled(
            "Restoring…",
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        )));
    }
    if let Some(notice) = pane.notice() {
        lines.push(Line::from(Span::styled(
            notice,
            Style::new().fg(Color::Yellow),
        )));
    }
    // The keys follow the buttons: while the restore is out none of them acts,
    // so naming them would promise actions the pane will ignore.
    let keys: &[(&str, &str)] = if restoring {
        &[]
    } else {
        &[
            ("Left/Right", "choose"),
            ("Enter", "confirm"),
            ("Esc", "back"),
        ]
    };
    block_with_keys(f, area, lines, keys, &pane.scroll);
}

/// What a restore does to this target, said before the user commits to it. A
/// target something else changed keeps its current bytes, so the sentence names
/// where the earlier ones go instead of promising an overwrite. A file the
/// write created has no earlier bytes: putting it back removes it, and the
/// sentence says so rather than describing a restore that would not happen.
fn restore_effect(view: &CheckpointView) -> String {
    if !view.before.present {
        return match view.state {
            CheckpointState::Applied => {
                "Restore:    removes the file this write created; there was nothing there before"
                    .to_string()
            }
            CheckpointState::NotApplied => {
                "Restore:    changes nothing; the file this write created is already gone"
                    .to_string()
            }
            CheckpointState::Conflict => {
                "Restore:    the target changed since the write, so it keeps its bytes, and there is nothing earlier to put beside it"
                    .to_string()
            }
            CheckpointState::Unverified => {
                "Restore:    nothing to put back; the earlier bytes were never kept".to_string()
            }
        };
    }
    match view.state {
        CheckpointState::Applied => {
            "Restore:    puts the bytes the write found back into the target".to_string()
        }
        CheckpointState::NotApplied => {
            "Restore:    changes nothing; the target already holds the bytes the write found"
                .to_string()
        }
        CheckpointState::Conflict => {
            "Restore:    the target changed since the write, so it keeps its bytes and the earlier ones go to a copy beside it"
                .to_string()
        }
        CheckpointState::Unverified => {
            "Restore:    nothing to put back; the earlier bytes were never kept".to_string()
        }
    }
}

/// One side of a write, as the restore page states it. The name the stored
/// bytes are kept under stays off the screen; how many bytes the file held is
/// what the user judges the restore by.
fn image_label(image: &CheckpointImage) -> String {
    if !image.present {
        return "absent".to_string();
    }
    match image.bytes {
        Some(bytes) => format!("{bytes} bytes"),
        None => "not kept".to_string(),
    }
}
