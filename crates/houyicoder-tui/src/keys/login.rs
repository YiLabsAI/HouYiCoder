//! Login + console screen key handlers. Split from keys.rs so that file
//! stays under the size gate.

use crossterm::event::{KeyCode, KeyEvent};
use houyicoder_protocol::frontend::LoginMode;

use crate::state::{App, Screen};

/// Login screen keys: 1 = SSO, 2 = API key, 3 = local. All modes land on
/// Working: you arrive on the same surface you type into, with a welcome-back
/// line as the first transcript line. The review console is reachable via
/// /console, not a forced gate. Esc quits.
pub fn handle_login(app: &mut App, k: KeyEvent) {
    match k.code {
        KeyCode::Char('1') | KeyCode::Char('s') => {
            app.login_mode = Some(LoginMode::Sso);
            land_on_working(app);
        }
        KeyCode::Char('2') | KeyCode::Char('a') => {
            app.login_mode = Some(LoginMode::ApiKey);
            land_on_working(app);
        }
        KeyCode::Char('3') | KeyCode::Char('l') => {
            app.login_mode = Some(LoginMode::Local);
            app.status.sandbox = "off (local)".to_string();
            land_on_working(app);
        }
        KeyCode::Esc | KeyCode::Char('q') => app.quit = true,
        _ => {}
    }
}

fn land_on_working(app: &mut App) {
    app.screen = Screen::Working;
}

/// Console review keys (review-node console): Up/Down move focus, a/Enter sign off, r
/// rejects (writes back to org eval, stub), p replays the focused finding,
/// Enter (on an empty queue) or Esc -> working / quit.
pub fn handle_console(app: &mut App, k: KeyEvent) {
    match k.code {
        KeyCode::Up => app.console_focus_up(),
        KeyCode::Down => app.console_focus_down(),
        KeyCode::Char('a') => {
            app.signoff_focused("you", "now");
            app.system_line("finding approved -> decision log (stub)");
        }
        KeyCode::Enter => {
            if app.console_len() == 0 || app.review.findings[app.review.focus].resolved() {
                app.screen = Screen::Working;
            } else {
                app.signoff_focused("you", "now");
                app.system_line("finding approved -> decision log (stub)");
            }
        }
        KeyCode::Char('r') => {
            if let Some(note) = app.reject_focused("you", "now") {
                app.system_line(note);
            }
        }
        KeyCode::Char('p') => {
            if let Some(r) = app.review.findings.get(app.review.focus) {
                app.system_line(format!("replay -> {} (stub)", r.hunk_id));
            }
        }
        KeyCode::Esc | KeyCode::Char('q') => app.quit = true,
        _ => {}
    }
}
