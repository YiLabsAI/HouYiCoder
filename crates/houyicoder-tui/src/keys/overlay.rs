//! Queue-management overlay keys (Ctrl+G). Extracted from keys.rs to keep
//! that module under the file-size gate. See keys::handle_working for the
//! dispatch. The queue has two tiers: entries the user can still reorder, and
//! the one already handed to the engine, which is fixed.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::agent_message::ClientCommand;
use crate::pending_queue::PendingItem;
use crate::state::App;

/// Queue overlay keys (Ctrl+G opened it). Up/Down move cursor; e recalls
/// focused (removed, overlay closes); d deletes; a recalls all; Esc/Ctrl+G
/// closes. Returns true when consumed; false falls through so Ctrl+A/E/U,
/// arrows, Backspace, and Enter (submit) work while open. Enter is NOT
/// recall; recall is e only. Remove-on-recall matches a typical editor.
pub(crate) fn handle_queue_overlay(app: &mut App, k: KeyEvent) -> bool {
    if app.pending.is_empty() {
        app.queue_view_open = false;
        return true;
    }
    let n = app.pending.len();
    let i = app.queue_focus.min(n - 1);
    match k.code {
        KeyCode::Esc => {
            app.queue_view_open = false;
            true
        }
        KeyCode::Char('g') if k.modifiers.contains(KeyModifiers::CONTROL) => {
            app.queue_view_open = false;
            true
        }
        KeyCode::Up => {
            app.queue_focus = (i + n - 1) % n;
            true
        }
        KeyCode::Down => {
            app.queue_focus = (i + 1) % n;
            true
        }
        // Enter closes + falls through to submit (not recall).
        KeyCode::Enter => {
            app.queue_view_open = false;
            false
        }
        KeyCode::Char('e') => {
            let item = app.pending.remove(i);
            // A Message has a live server copy to remove over the wire; a
            // ParkedMessage (barrier'd or orphaned) and a Command are
            // local-only. Exhaustive so a new variant forces a review here.
            match &item {
                PendingItem::Message(text) => {
                    app.send_cmd(ClientCommand::QueueRemove {
                        session_id: app.session_id.clone(),
                        text: text.clone(),
                    });
                }
                PendingItem::ParkedMessage(_) | PendingItem::Command(_) => {}
            }
            app.queue_view_open = false;
            app.input.set(item.display().to_string());
            true
        }
        KeyCode::Char('d') => {
            let item = app.pending.remove(i);
            match &item {
                PendingItem::Message(text) => {
                    app.send_cmd(ClientCommand::QueueRemove {
                        session_id: app.session_id.clone(),
                        text: text.clone(),
                    });
                }
                PendingItem::ParkedMessage(_) | PendingItem::Command(_) => {}
            }
            let m = app.pending.len();
            if m == 0 {
                app.queue_view_open = false;
            } else {
                app.queue_focus = app.queue_focus.min(m - 1);
            }
            true
        }
        KeyCode::Char('a') => {
            let items: Vec<PendingItem> = app.pending.drain(..).collect();
            let all = items
                .iter()
                .map(|it| it.display())
                .collect::<Vec<_>>()
                .join("\n");
            for it in &items {
                match it {
                    PendingItem::Message(text) => {
                        app.send_cmd(ClientCommand::QueueRemove {
                            session_id: app.session_id.clone(),
                            text: text.clone(),
                        });
                    }
                    PendingItem::ParkedMessage(_) | PendingItem::Command(_) => {}
                }
            }
            app.queue_focus = 0;
            app.queue_view_open = false;
            app.input.set(all);
            true
        }
        // Ctrl+A/E/U pass through to edit the input without closing first.
        KeyCode::Char(c)
            if k.modifiers.contains(KeyModifiers::CONTROL) && matches!(c, 'a' | 'e' | 'u') =>
        {
            false
        }
        KeyCode::Left | KeyCode::Right | KeyCode::Backspace | KeyCode::Home | KeyCode::End => false,
        _ => true, // Bare typing and shortcuts swallowed (management context).
    }
}
