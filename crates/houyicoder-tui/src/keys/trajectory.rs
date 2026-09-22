//! /trajectory pane key handlers: 3-level drill-down.
//! Level 0: turn list — Up/Down select, Enter expands, Esc closes.
//! Level 1: turn detail — Up/Down select events, Enter shows detail, Esc back.
//! Level 2: event detail — Esc back to level 1.

use crate::state::App;
use crate::state::enums::Pane;
use crossterm::event::{KeyCode, KeyEvent};

/// Handle a key when the /trajectory pane is active. Returns true if the key
/// was consumed (the caller should not fall through to generic input).
///
/// The list is an audit trail with a direction in time, so it does not wrap: Up
/// from the oldest loaded turn stays there and Down from the newest stays there.
/// Wrapping would jump a thousand-turn history from end to start on one
/// keystroke and destroy the sense of where the user is.
pub fn handle(app: &mut App, k: KeyEvent) -> bool {
    match k.code {
        // Level 2 is a stable detail view (the record selected at L1), not a
        // switcher — Up/Down is a no-op there; switch records at L1. The keys
        // are still consumed so they never move the input cursor.
        KeyCode::Up => {
            if app.trajectory_level.get() < 2 {
                let c = app.trajectory_cursor.get();
                if c == 0 {
                    // At the top of the loaded window there is nowhere to move,
                    // so widen it: the pane loads the tail first and older
                    // history arrives a page at a time.
                    if let Some(log) = app.trajectory_log.as_ref() {
                        log.load_older();
                    }
                }
                app.trajectory_cursor.set(c.saturating_sub(1));
            }
            true
        }
        KeyCode::Down => {
            if app.trajectory_level.get() < 2 {
                let c = app.trajectory_cursor.get();
                let last = app.trajectory_list_len.get().saturating_sub(1);
                app.trajectory_cursor.set((c + 1).min(last));
            }
            true
        }
        KeyCode::Home => {
            if app.trajectory_level.get() < 2 {
                app.trajectory_cursor.set(0);
            }
            true
        }
        KeyCode::End => {
            if app.trajectory_level.get() < 2 {
                let len = app.trajectory_list_len.get();
                if len > 0 {
                    app.trajectory_cursor.set(len.saturating_sub(1));
                }
            }
            true
        }
        KeyCode::Enter if app.input.is_empty() => {
            let level = app.trajectory_level.get();
            if level == 0 && app.trajectory_list_len.get() > 0 {
                // Freeze the turn-list selection so the turn-detail and
                // event-detail levels render THAT row, not the first turn.
                // Works for both Turn and [bg] rows. Skip the drill when the
                // row list is empty (a fresh session with no turns yet) —
                // drilling into no rows rendered "no row data" at the
                // turn-detail level, which read as a crash.
                app.trajectory_turn_idx.set(app.trajectory_cursor.get());
                app.trajectory_level.set(1);
                app.trajectory_cursor.set(0);
            } else if level == 1 {
                // [bg] rows have no event list to drill into — stay at L1.
                if !app.trajectory_at_bg.get() {
                    app.trajectory_level.set(2);
                    // Keep the cursor so L2 shows the event selected at L1.
                }
            }
            true
        }
        KeyCode::Esc => {
            let level = app.trajectory_level.get();
            if level == 0 {
                app.pane = Pane::Transcript;
            } else {
                app.trajectory_level.set(level - 1);
                app.trajectory_cursor.set(0);
            }
            true
        }
        _ => false,
    }
}
