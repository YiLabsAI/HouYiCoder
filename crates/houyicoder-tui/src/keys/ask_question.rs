//! Key handlers for the AskUserQuestion card: tab navigation between
//! questions, option navigation, selection, Other text input, submit-view
//! Submit/Cancel, and cancel.
//!
//! The card owns its own state transitions and reports what the shell should
//! do with the result, so this module never reaches into the shell to read a
//! field the card already holds.
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
use serde_json::Value;

use crate::ask_question_model::AskQuestion;
use crate::state::App;

/// What the card decided the shell should do with the key.
///
/// The card answers with a value rather than acting on the shell so its own
/// state transitions stay a pure function of the card and the key.
pub(super) enum AskDecision {
    /// The key changed card state only.
    None,
    /// Ship this verdict and close the card.
    Resolve(ApprovalDecision),
}

/// Dispatch a key for the AskUserQuestion card.
pub(super) fn handle_ask_question(app: &mut App, k: KeyEvent) {
    let Some(decision) = app.ask_question_mut().map(|aq| aq.handle_key(k)) else {
        return;
    };
    match decision {
        AskDecision::None => {}
        AskDecision::Resolve(verdict) => {
            if app.sessions.current.connection.is_some() {
                app.resolve_current_approval(verdict);
            } else {
                // No connection can carry the verdict, so the card closes and
                // the answers stay readable in the transcript.
                app.sessions.current.clear_prompt();
            }
        }
    }
}

impl AskQuestion {
    /// Handle one key against this card and report what the shell should do.
    pub(super) fn handle_key(&mut self, k: KeyEvent) -> AskDecision {
        // Other text input mode: typing writes to other_text, Enter submits or
        // advances, Esc returns to the option list. Tab-nav is disabled.
        if self.other_focused {
            return self.handle_other_input(k);
        }
        // Submit view: Up/Down cycle Submit-answers/Cancel, Enter acts.
        if self.is_submit_view() {
            return self.handle_submit_view(k);
        }
        let total = self.current_option_count();
        let qi = self.current;
        match k.code {
            // Tab navigation between questions: Tab/Right forward,
            // Shift+Tab/Left backward. Clamped to [0, max_index].
            KeyCode::Tab | KeyCode::Right => {
                if self.current < self.max_index() {
                    self.current += 1;
                }
                AskDecision::None
            }
            KeyCode::BackTab | KeyCode::Left => {
                if self.current > 0 {
                    self.current -= 1;
                }
                AskDecision::None
            }
            // Option cursor within the current question.
            KeyCode::Up => {
                if let Some(cur) = self.cursors.get_mut(qi) {
                    *cur = (*cur + total.saturating_sub(1)) % total.max(1);
                }
                AskDecision::None
            }
            KeyCode::Down => {
                if let Some(cur) = self.cursors.get_mut(qi) {
                    *cur = (*cur + 1) % total.max(1);
                }
                AskDecision::None
            }
            KeyCode::Enter => self.handle_enter(),
            KeyCode::Esc => self.cancel_decision(),
            _ => {
                let _ = k.modifiers.contains(KeyModifiers::SHIFT);
                AskDecision::None
            }
        }
    }

    /// Handle Enter on the current question: Other focus, single-select
    /// advance/auto-submit, multi-select toggle, multi-select Submit button.
    fn handle_enter(&mut self) -> AskDecision {
        let qi = self.current;
        let cursor = self.cursors.get(qi).copied().unwrap_or(0);
        let other_idx = self.current_other_idx();
        let submit_btn_idx = self.current_submit_btn_idx();
        let multi = self.current_multi();
        let hide_submit = self.hide_submit_tab();

        // Other: focus the text input and record the Other row as chosen, so
        // the answers carry whatever the user types into it.
        if cursor == other_idx {
            self.other_focused = true;
            if self.other_text[qi].is_none() {
                self.other_text[qi] = Some(String::new());
            }
            if !self.selections[qi].contains(&other_idx) {
                self.selections[qi].push(other_idx);
            }
            return AskDecision::None;
        }
        // Multi-select Submit button: advance to the next question or the
        // submit view.
        if multi && cursor == submit_btn_idx {
            return self.advance_decision();
        }
        // A plain option: record the answer.
        if let Some(selections) = self.selections.get_mut(qi) {
            if multi {
                if let Some(pos) = selections.iter().position(|&s| s == cursor) {
                    selections.remove(pos);
                } else {
                    selections.push(cursor);
                }
            } else {
                selections.clear();
                selections.push(cursor);
            }
        }
        // A single single-select question has nothing left to ask.
        if hide_submit {
            return self.submit_decision();
        }
        if multi {
            return AskDecision::None;
        }
        self.advance_decision()
    }

    /// Handle keys while the Other text input is focused.
    fn handle_other_input(&mut self, k: KeyEvent) -> AskDecision {
        let qi = self.current;
        let hide_submit = self.hide_submit_tab();
        match k.code {
            KeyCode::Esc => {
                self.other_focused = false;
                AskDecision::None
            }
            KeyCode::Enter => {
                self.other_focused = false;
                // Single single-select question: auto-submit. Otherwise
                // advance, which is also the multi-select submit path.
                if hide_submit {
                    self.submit_decision()
                } else {
                    self.advance_decision()
                }
            }
            KeyCode::Backspace => {
                if let Some(t) = self.other_text[qi].as_mut() {
                    t.pop();
                } else {
                    self.other_text[qi] = Some(String::new());
                }
                AskDecision::None
            }
            KeyCode::Char(c) => {
                let entry = self.other_text[qi].get_or_insert_with(String::new);
                entry.push(c);
                AskDecision::None
            }
            _ => AskDecision::None,
        }
    }

    /// Handle keys in the submit (review) view: navigate Submit/Cancel, act.
    fn handle_submit_view(&mut self, k: KeyEvent) -> AskDecision {
        match k.code {
            KeyCode::Up | KeyCode::Left | KeyCode::Down | KeyCode::Right | KeyCode::Tab => {
                self.submit_cursor = (self.submit_cursor + 1) % 2;
                AskDecision::None
            }
            KeyCode::Enter => {
                if self.submit_cursor == 0 {
                    self.submit_decision()
                } else {
                    self.cancel_decision()
                }
            }
            KeyCode::Esc => self.cancel_decision(),
            _ => AskDecision::None,
        }
    }

    /// Advance to the next question, or enter the submit view if this was the
    /// last question. Has no effect when already in the submit view.
    fn advance_decision(&mut self) -> AskDecision {
        if self.current < self.questions.len() {
            self.current += 1;
        }
        AskDecision::None
    }

    /// Build the wire verdict that carries this card's answers. The card is
    /// read, not taken, so a failed enqueue still holds the question and its
    /// answers for a retry.
    fn submit_decision(&self) -> AskDecision {
        AskDecision::Resolve(ApprovalDecision {
            call_id: self.call_id.clone(),
            approved: true,
            updated_input: Some(self.build_updated_input()),
            scope: "once".to_string(),
        })
    }

    /// Build the declined verdict: a declined marker (not a plain reject) so
    /// the tool formats the declined-to-answer text. The declined flag rides on
    /// the original input as updated_input.
    fn cancel_decision(&self) -> AskDecision {
        let mut input = self.original_input.clone();
        if let Value::Object(ref mut obj) = input {
            obj.insert("declined".into(), serde_json::json!(true));
        }
        AskDecision::Resolve(ApprovalDecision {
            call_id: self.call_id.clone(),
            approved: true,
            updated_input: Some(input),
            scope: "once".to_string(),
        })
    }
}

#[cfg(test)]
mod tests;
