//! /trajectory pane key handlers: 3-level drill-down.
//! Level 0: turn list — Up/Down select, Enter expands, Esc closes.
//! Level 1: turn detail — Up/Down select events, Enter shows detail, Esc back.
//! Level 2: event detail — Esc back to level 1.

use crate::state::App;
use crate::state::enums::Pane;
use crate::view::trajectory_pane;
use crossterm::event::{KeyCode, KeyEvent};

/// Record the turn the L0 cursor sits on, so a page that arrives under it can
/// put the cursor back on the same turn instead of the same row index.
fn note_selected_turn(app: &App) {
    if app.trajectory.level() != 0 {
        return;
    }
    if let Some(log) = app.trajectory_log.as_ref() {
        trajectory_pane::note_selected_turn(&app.trajectory, &log.trajectory());
    }
}

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
            if app.trajectory.level() < 2 {
                let c = app.trajectory.cursor();
                if c == 0 {
                    // At the top of the loaded window there is nowhere to move,
                    // so widen it: the pane loads the tail first and older
                    // history arrives a page at a time.
                    if let Some(log) = app.trajectory_log.as_ref() {
                        log.load_older();
                    }
                }
                app.trajectory.set_cursor(c.saturating_sub(1));
                note_selected_turn(app);
            }
            true
        }
        KeyCode::Down => {
            if app.trajectory.level() < 2 {
                let c = app.trajectory.cursor();
                let last = app.trajectory.list_len().saturating_sub(1);
                app.trajectory.set_cursor((c + 1).min(last));
                note_selected_turn(app);
            }
            true
        }
        // At the turn list these are the ends of the history, not the ends of
        // the loaded window: the window slides, so its first and last rows are
        // only the first and last turns once the pane has read the head or the
        // tail. At the record levels they stay the ends of the list in hand,
        // which the window moves would otherwise re-point under the drill.
        KeyCode::Home => {
            if app.trajectory.level() == 0
                && let Some(log) = app.trajectory_log.as_ref()
            {
                log.load_earliest();
                // The history's first turn, so the cursor lands on it once the
                // head page arrives.
                let generation = log.trajectory().history_generation;
                app.trajectory.select_turn(1, generation);
            }
            if app.trajectory.level() < 2 {
                app.trajectory.set_cursor(0);
            }
            true
        }
        KeyCode::End => {
            match (app.trajectory.level(), app.trajectory_log.as_ref()) {
                (0, Some(log)) => {
                    log.return_to_tail();
                    // The history's newest turn, so the cursor lands on it
                    // once the tail page arrives. The cursor itself is left
                    // alone: until then it names a row of the window being
                    // replaced.
                    let view = log.trajectory();
                    app.trajectory
                        .select_turn(view.total_turns, view.history_generation);
                }
                // A record list, or a turn list with no paged history: the
                // rows in hand are the whole list, so the last one is the end.
                (1, _) | (0, None) => {
                    let last = app.trajectory.list_len().saturating_sub(1);
                    app.trajectory.set_cursor(last);
                }
                _ => {}
            }
            true
        }
        KeyCode::Enter if app.input.is_empty() => {
            let level = app.trajectory.level();
            if level == 0 && app.trajectory.list_len() > 0 {
                // Freeze the turn-list selection so the turn-detail and
                // event-detail levels render THAT row, not the first turn.
                // Works for both Turn and [bg] rows. Skip the drill when the
                // row list is empty (a fresh session with no turns yet) —
                // drilling into no rows rendered "no row data" at the
                // turn-detail level, which read as a crash.
                app.trajectory.set_turn_idx(app.trajectory.cursor());
                app.trajectory.set_level(1);
                app.trajectory.set_cursor(0);
            } else if level == 1 {
                // [bg] rows have no event list to drill into — stay at L1.
                if !app.trajectory.at_bg() {
                    app.trajectory.set_level(2);
                    // Keep the cursor so L2 shows the event selected at L1.
                }
            }
            true
        }
        // An ordinary character is not the pane's to keep: it closes the pane
        // and falls through to the input box. The shared pane policy would
        // otherwise drop it, and this pane's contract says a character key is
        // not intercepted. Closing first is what keeps that from typing into a
        // box the pane is covering.
        KeyCode::Char(_) => {
            app.pane = Pane::Transcript;
            // Closing the pane must also leave the viewport that drew it: a
            // pane closed while Focus still owns the screen would send the
            // character to a box the user cannot see.
            app.fold_to_working();
            false
        }
        KeyCode::Esc => {
            let level = app.trajectory.level();
            if level == 0 {
                app.pane = Pane::Transcript;
                app.fold_to_working();
            } else {
                app.trajectory.set_level(level - 1);
                app.trajectory.set_cursor(0);
            }
            true
        }
        _ => false,
    }
}
