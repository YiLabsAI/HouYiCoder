//! Slash-command palette keys. Split from the keys root so that module
//! stays a thin working-surface dispatcher. See keys::handle_working for the
//! dispatch routing.

use crossterm::event::{KeyCode, KeyEvent};

use crate::state::App;

/// Slash-command palette keys: type to filter, navigate the filtered list,
/// select, or close. Backspace edits the query; Esc closes.
pub(super) fn handle_palette(app: &mut App, k: KeyEvent) {
    match k.code {
        KeyCode::Esc => app.close_palette(),
        KeyCode::Up => app.palette_up(),
        KeyCode::Down => app.palette_down(),
        KeyCode::Enter => {
            if let Some(cmd) = app.selected_command() {
                // Arg-taking commands: keep the palette OPEN and seed the query
                // with the name and a trailing space, so the user keeps typing
                // the argument in the popup (which shows the arg hint after the
                // space). The popup stays open after a space: a typical
                // input-box autocomplete vanishes after a space, leaving blind
                // typing; our popup stays open and guides the argument. Argless
                // commands auto-run on select.
                if cmd.takes_arg() {
                    app.palette.query = format!("{} ", cmd.name().trim_start_matches('/'));
                    app.palette.sel = 0;
                } else {
                    app.close_palette();
                    app.input.set(cmd.name().to_string());
                    app.submit_input();
                }
            } else if !app.palette.query.is_empty() {
                // No palette entry matches the query. Submit it as a slash
                // command so arg-bearing forms typed in the filter
                // (/permissions git off, /resume file.json) and genuinely
                // unknown tokens (/nope) still run instead of dropping Enter on
                // the floor. Strip any leading slash the user typed so it does
                // not double-slash.
                let q = app.palette.query.trim_start_matches('/');
                let raw = format!("/{q}");
                app.close_palette();
                app.input.set(raw);
                app.submit_input();
            }
        }
        KeyCode::Backspace => {
            if app.palette.query.is_empty() {
                app.close_palette();
            } else {
                app.palette_pop();
            }
        }
        // Accept ascii-graphic chars and the space separator. The space is the
        // arg separator for arg-taking local commands (/permissions git off):
        // with no palette entry matching a spaced query, Enter falls through
        // to the raw-submit branch, and ships the typed query as a slash command.
        // Without accepting the space, arg-taking commands are unreachable
        // (the query arrives concatenated, e.g. "permissionsgitoff").
        KeyCode::Char(c) if c.is_ascii_graphic() || c == ' ' => app.palette_push(c),
        _ => {}
    }
}
