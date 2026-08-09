//! Scroll-mode keys, split from keys.rs so the parent stays under the
//! file-size gate. In plain scroll: PgUp/PgDn page; '/' opens the legacy
//! inline search; Esc/End/any typing key returns to the prior viewport. In
//! the search view (search.active): n/N walk matches (older/newer) and
//! jump each to the focused row; g/G go to the top/bottom; q/Esc exit
//! (clearing verbose + the query, not just restoring the viewport); slash
//! opens the in-view re-search bar. While the re-search bar is open
//! (search.input_mode) printable/edit keys edit the buffer, Enter commits,
//! Esc or Ctrl+C/Ctrl+G cancels.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::state::App;

pub(super) fn handle_scroll(app: &mut App, k: KeyEvent) {
    if app.search.input_mode {
        handle_search_input(app, k);
        return;
    }
    if app.search.active {
        handle_search_view(app, k);
        return;
    }
    match k.code {
        KeyCode::Esc | KeyCode::End => {
            app.exit_scroll();
            app.scroll_transcript_follow_tail();
        }
        KeyCode::PageUp => app.scroll_transcript_up(),
        KeyCode::PageDown => app.scroll_transcript_down(),
        KeyCode::Char(c) if c.is_ascii_graphic() => app.exit_scroll(),
        _ => {}
    }
}

/// Search-view keys. n walks toward the older matches, N toward the newer
/// (matches are in transcript order, oldest first); each jumps the focused
/// match into view so the screen follows the walk. g/G jump to the top/bottom.
/// q/Esc exit the view. slash opens the in-view re-search bar.
/// PageUp/PageDown scroll a screenful. In byte-window mode the scroll state
/// is window_scroll (separate from TranscriptScroll); n/N + jump branch
/// internally on window_mode. Cross-window paging (scroll past the window
/// edge) lands with the G full-scan + per-window n/N scan.
fn handle_search_view(app: &mut App, k: KeyEvent) {
    // Esc: if the G full-index is building, interrupt it (stay in the view);
    // otherwise exit the view.
    if k.code == KeyCode::Esc {
        if app.interrupt_index() {
            return;
        }
        app.exit_search_view();
        return;
    }
    match k.code {
        KeyCode::Char('/') => app.enter_search_input(),
        KeyCode::Char('q') => app.exit_search_view(),
        KeyCode::Char('n') => {
            if app.window_mode {
                app.window_search_older();
            } else {
                app.search.prev();
                app.jump_to_focused_match();
            }
        }
        KeyCode::Char('N') => {
            if app.window_mode {
                app.window_search_newer();
            } else {
                app.search.next();
                app.jump_to_focused_match();
            }
        }
        KeyCode::Char('g') => {
            if app.window_mode {
                app.window_scroll.jump_to(0);
            } else {
                app.transcript_scroll.jump_to(0);
            }
        }
        KeyCode::Char('G') => {
            if app.window_mode {
                // Build the full event-byte-offset index across frames so
                // arbitrary seek + the total count become possible. Progress
                // shows in the chrome; Esc interrupts.
                app.start_full_index();
                app.window_scroll.follow_tail();
            } else {
                app.scroll_transcript_follow_tail();
            }
        }
        KeyCode::PageUp => {
            if app.window_mode {
                app.window_scroll.page_up();
            } else {
                app.scroll_transcript_up();
            }
        }
        KeyCode::PageDown => {
            if app.window_mode {
                app.window_scroll.page_down();
            } else {
                app.scroll_transcript_down();
            }
        }
        _ => {}
    }
}

/// In-view slash re-search bar keys. Printable chars and the cursor-edit
/// keys drive the input field; Enter commits (re-runs the search and
/// re-focuses the newest match), Esc or Ctrl+C/Ctrl+G cancels (discards the
/// buffer, keeps the prior query). The bar is a snapshot, not per-keystroke
/// live (decision 5): the re-scan runs once on Enter, not on each edit.
fn handle_search_input(app: &mut App, k: KeyEvent) {
    if k.modifiers.contains(KeyModifiers::CONTROL) && matches!(k.code, KeyCode::Char('c' | 'g')) {
        app.cancel_search_input();
        return;
    }
    match k.code {
        KeyCode::Enter => app.commit_search_input(),
        KeyCode::Esc => app.cancel_search_input(),
        KeyCode::Backspace => app.search.input.backspace(),
        KeyCode::Delete => app.search.input.delete(),
        KeyCode::Left => app.search.input.move_left(),
        KeyCode::Right => app.search.input.move_right(),
        KeyCode::Home => app.search.input.move_home(),
        KeyCode::End => app.search.input.move_end(),
        KeyCode::Char(c) => app.search.input.push(c),
        _ => {}
    }
}
