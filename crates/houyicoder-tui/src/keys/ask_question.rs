//! Key handlers for the AskUserQuestion card: tab navigation between
//! questions, option navigation, selection, Other text input, submit-view
//! Submit/Cancel, and cancel. Included into keys.rs via a path attribute
//! so keys.rs stays under the file-size gate.
//!
//! Tab/Shift+Tab/Left/Right move the question index (clamped 0..=len).
//! Up/Down move the option cursor within the current question. Enter on a
//! single-select option stores the answer and advances (or auto-submits
//! for a single single-select question). Enter on a multi-select option
//! toggles selection (no advance); the Submit/Next button row advances.
//! Enter on Other focuses a text input; Enter in the text input submits
//! (single single-select) or advances (multi-select or multi-question).
//! The submit view has a Submit-answers/Cancel select.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use houyicoder_protocol::frontend::run::ApprovalDecision;

use crate::state::App;

/// Dispatch a key for the AskUserQuestion card.
pub(super) fn handle_ask_question(app: &mut App, k: KeyEvent) {
    // Other text input mode: typing writes to other_text, Enter submits or
    // advances, Esc returns to the option list. Tab-nav is disabled.
    if app.ask_question.as_ref().is_some_and(|aq| aq.other_focused) {
        handle_other_input(app, k);
        return;
    }

    // Submit view: Up/Down cycle Submit-answers/Cancel, Enter acts, Esc cancels.
    if app
        .ask_question
        .as_ref()
        .is_some_and(|aq| aq.is_submit_view())
    {
        handle_submit_view(app, k);
        return;
    }

    let Some(aq) = app.ask_question.as_mut() else {
        return;
    };
    let total = aq.current_option_count();
    let qi = aq.current;
    let shift = k.modifiers.contains(KeyModifiers::SHIFT);

    match k.code {
        // Tab navigation between questions: Tab/Right forward,
        // Shift+Tab/Left backward. Clamped to [0, max_index].
        KeyCode::Tab | KeyCode::Right => {
            if aq.current < aq.max_index() {
                aq.current += 1;
            }
        }
        KeyCode::BackTab | KeyCode::Left => {
            if aq.current > 0 {
                aq.current -= 1;
            }
        }
        // Option cursor within the current question.
        KeyCode::Up => {
            if let Some(cur) = aq.cursors.get_mut(qi) {
                *cur = (*cur + total.saturating_sub(1)) % total.max(1);
            }
        }
        KeyCode::Down => {
            if let Some(cur) = aq.cursors.get_mut(qi) {
                *cur = (*cur + 1) % total.max(1);
            }
        }
        KeyCode::Enter => {
            handle_enter(app, qi);
        }
        KeyCode::Esc => {
            cancel_ask_question(app);
        }
        _ => {
            let _ = shift;
        }
    }
}

/// Handle Enter on the current question: Other focus, single-select
/// advance/auto-submit, multi-select toggle, multi-select Submit button.
fn handle_enter(app: &mut App, qi: usize) {
    let Some(aq) = app.ask_question.as_mut() else {
        return;
    };
    let cursor = aq.cursors.get(qi).copied().unwrap_or(0);
    let other_idx = aq.current_other_idx();
    let submit_btn_idx = aq.current_submit_btn_idx();
    let multi = aq.current_multi();
    let hide_submit = aq.hide_submit_tab();

    // Other: focus the text input and ensure it is in selections.
    if cursor == other_idx {
        aq.other_focused = true;
        if aq.other_text[qi].is_none() {
            aq.other_text[qi] = Some(String::new());
        }
        if !aq.selections[qi].contains(&other_idx) {
            aq.selections[qi].push(other_idx);
        }
        return;
    }

    // Multi-select Submit/Next button: advance to next question or submit view.
    if multi && cursor == submit_btn_idx {
        let _ = aq;
        advance_ask_question(app);
        return;
    }

    if multi {
        // Toggle the cursor index in selections (no advance).
        if let Some(pos) = aq.selections[qi].iter().position(|&x| x == cursor) {
            aq.selections[qi].remove(pos);
        } else {
            aq.selections[qi].push(cursor);
        }
    } else {
        // Single-select: store and advance (or auto-submit).
        aq.selections[qi] = vec![cursor];
    }

    // Decide what to do after selection. Release the borrow before
    // calling functions that take &mut App.
    let action = if multi {
        AskAction::None
    } else if hide_submit {
        AskAction::Submit
    } else {
        AskAction::Advance
    };
    let _ = aq;
    match action {
        AskAction::Submit => submit_ask_question(app),
        AskAction::Advance => advance_ask_question(app),
        AskAction::None => {}
    }
}

/// Handle keys while the Other text input is focused.
fn handle_other_input(app: &mut App, k: KeyEvent) {
    let Some(aq) = app.ask_question.as_mut() else {
        return;
    };
    let qi = aq.current;
    let hide_submit = aq.hide_submit_tab();
    let multi = aq.current_multi();
    match k.code {
        KeyCode::Esc => {
            aq.other_focused = false;
        }
        KeyCode::Enter => {
            aq.other_focused = false;
            let _ = aq;
            // Single single-select question: auto-submit. Otherwise advance.
            if hide_submit {
                submit_ask_question(app);
            } else {
                advance_ask_question(app);
            }
            // For multi-select, advancing is the "submit the question" path.
            let _ = multi;
        }
        KeyCode::Backspace => {
            if let Some(t) = aq.other_text[qi].as_mut() {
                t.pop();
            } else {
                aq.other_text[qi] = Some(String::new());
            }
        }
        KeyCode::Char(c) => {
            let entry = aq.other_text[qi].get_or_insert_with(String::new);
            entry.push(c);
        }
        _ => {}
    }
}

/// Handle keys in the submit (review) view: navigate Submit/Cancel, act.
fn handle_submit_view(app: &mut App, k: KeyEvent) {
    let Some(aq) = app.ask_question.as_mut() else {
        return;
    };
    match k.code {
        KeyCode::Up | KeyCode::Left => {
            aq.submit_cursor = (aq.submit_cursor + 1) % 2;
        }
        KeyCode::Down | KeyCode::Right | KeyCode::Tab => {
            aq.submit_cursor = (aq.submit_cursor + 1) % 2;
        }
        KeyCode::Enter => {
            let choice = aq.submit_cursor;
            let _ = aq;
            if choice == 0 {
                submit_ask_question(app);
            } else {
                cancel_ask_question(app);
            }
        }
        KeyCode::Esc => {
            let _ = aq;
            cancel_ask_question(app);
        }
        _ => {}
    }
}

/// Internal action queued during the Enter handler, dispatched after the
/// mutable borrow on app.ask_question is released.
enum AskAction {
    None,
    Advance,
    Submit,
}

/// Advance to the next question, or enter the submit view if this was the
/// last question. Has no effect when already in the submit view.
fn advance_ask_question(app: &mut App) {
    if let Some(aq) = app.ask_question.as_mut()
        && aq.current < aq.questions.len()
    {
        aq.current += 1;
    }
}

/// Submit the AskUserQuestion card: build the wire decision with the answer-
/// populated input and ship it as the reverse response so the server resumes
/// the turn with the human's answers injected.
fn submit_ask_question(app: &mut App) {
    let Some(aq) = app.ask_question.take() else {
        return;
    };
    let call_id = aq.call_id.clone();
    let updated_input = aq.build_updated_input();
    if app.session.is_some() {
        app.resolve_current_approval(ApprovalDecision {
            call_id,
            approved: true,
            updated_input: Some(updated_input),
            scope: "once".to_string(),
        });
    }
}

/// Cancel the AskUserQuestion card: ship a declined marker (not a plain
/// reject) so the tool formats the declined-to-answer text. The declined flag
/// rides on the original input as updated_input.
fn cancel_ask_question(app: &mut App) {
    let Some(aq) = app.ask_question.take() else {
        return;
    };
    let call_id = aq.call_id.clone();
    let mut input = aq.original_input.clone();
    if let serde_json::Value::Object(ref mut obj) = input {
        obj.insert("declined".into(), serde_json::json!(true));
    }
    if app.session.is_some() {
        app.resolve_current_approval(ApprovalDecision {
            call_id,
            approved: true,
            updated_input: Some(input),
            scope: "once".to_string(),
        });
    }
}
