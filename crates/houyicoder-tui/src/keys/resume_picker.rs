//! Session-picker keys (the /resume picker). Extracted from keys.rs to keep
//! that module under the file-size gate. See keys::handle_working for the
//! dispatch routing. Type to filter by sid or name, Up/Down navigate, Enter
//! resumes the selection (in-process swap), Esc closes. Follows the palette
//! key shape so muscle memory transfers.

use crossterm::event::{KeyCode, KeyEvent};

use crate::pending_queue::PendingItem;
use crate::state::{App, Pane};

pub(super) fn handle_resume_picker(app: &mut App, k: KeyEvent) {
    match k.code {
        KeyCode::Esc => {
            app.resume_picker.close();
            app.pane = Pane::Transcript;
        }
        KeyCode::Up => app.resume_picker.prev(),
        KeyCode::Down => app.resume_picker.next(),
        KeyCode::Enter => {
            if let Some(row) = app.resume_picker.selected().map(|r| r.sid_str.clone()) {
                app.resume_picker.close();
                app.pane = Pane::Transcript;
                if app.agent_busy {
                    // Defer: a run is in flight. Enqueue a Command so the
                    // swap happens when the run resolves (drained FIFO at
                    // idle), not now (would fight the run).
                    app.pending
                        .push(PendingItem::Command(format!("/resume {row}")));
                    app.system_line(app.deferred_command_message(&format!("resume {row}")));
                } else {
                    app.pending_resume_target = Some(row.clone());
                    app.system_line(app.resume_switch_message(&row));
                }
            } else if !app.resume_picker.query.is_empty() {
                let q = app.resume_picker.query.trim().to_string();
                app.resume_picker.close();
                app.pane = Pane::Transcript;
                if app.agent_busy {
                    app.pending
                        .push(PendingItem::Command(format!("/resume {q}")));
                    app.system_line(app.deferred_command_message(&format!("resume {q}")));
                } else {
                    app.run_resume(Some(&q));
                }
            }
        }
        KeyCode::Backspace => {
            if app.resume_picker.query.is_empty() {
                app.resume_picker.close();
                app.pane = Pane::Transcript;
            } else {
                app.resume_picker.pop();
            }
        }
        KeyCode::Char(c) if c.is_ascii_graphic() || c == ' ' => app.resume_picker.push(c),
        _ => {}
    }
}
