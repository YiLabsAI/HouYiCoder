//! Admission of key events before they reach a handler.
//!
//! A terminal reports a release for every key press, and one platform also
//! reports auto-repeats. A release must change nothing, and a held action key
//! must fire once, while typed text, arrows, and editing keys keep their
//! repeat. One character above the basic plane arrives as two presses with no
//! release between them, so the second press is dropped when no other key came
//! between them.

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use super::pane_predicates::{child_action_pending, types_text};
use crate::state::App;

/// How close together the two presses for one non-BMP character arrive.
const DUPLICATE_WINDOW: Duration = Duration::from_millis(15);

/// What each key event is allowed to do, across a run of events.
#[derive(Debug, Default)]
pub(crate) struct KeyFilter {
    last_high: Option<(char, Instant)>,
}

impl KeyFilter {
    /// Decide whether one key event may act. A false answer means the event
    /// changes no state and needs no redraw.
    pub(crate) fn admit(&mut self, app: &App, key: &KeyEvent, now: Instant) -> bool {
        match key.kind {
            KeyEventKind::Release => {
                // A release ends the press it belongs to, so the same
                // character pressed again after it is a real second press.
                self.last_high = None;
                false
            }
            KeyEventKind::Repeat => {
                self.last_high = None;
                repeat_allowed(app, key)
            }
            KeyEventKind::Press => self.admit_press(key, now),
        }
    }

    /// A press acts unless it repeats the character the press just before it
    /// spelled. Any other key between the two makes the second press a real
    /// one.
    fn admit_press(&mut self, key: &KeyEvent, now: Instant) -> bool {
        let previous = self.last_high.take();
        let KeyCode::Char(c) = key.code else {
            return true;
        };
        if c <= '\u{FFFF}' {
            return true;
        }
        if let Some((last, at)) = previous
            && last == c
            && now.saturating_duration_since(at) < DUPLICATE_WINDOW
        {
            return false;
        }
        self.last_high = Some((c, now));
        true
    }
}

/// Whether an auto-repeat of this key may act. A chord never repeats: every
/// chord here is a one-shot action. A plain key repeats where holding it types
/// or scrolls.
fn repeat_allowed(app: &App, key: &KeyEvent) -> bool {
    // A printable character carrying both control and alt is a layout's AltGr
    // form of that character, not a chord, so it keeps its repeat.
    let printable_alt_gr = key
        .modifiers
        .contains(KeyModifiers::CONTROL | KeyModifiers::ALT)
        && matches!(key.code, KeyCode::Char(c) if !c.is_control());
    if key
        .modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        && !printable_alt_gr
    {
        return false;
    }
    match key.code {
        // A held kill or background key would act once per repeat, killing one
        // child after another; the gesture is one keypress.
        KeyCode::Char('k' | 'K' | 'b') if child_action_pending(app) => false,
        KeyCode::Char(_) => types_text(app),
        // Tab walks the question tabs while a question card is up, which is
        // navigation rather than the one-shot pane cycle it is elsewhere.
        KeyCode::Tab if app.ask_question().is_some() => true,
        KeyCode::Up
        | KeyCode::Down
        | KeyCode::Left
        | KeyCode::Right
        | KeyCode::PageUp
        | KeyCode::PageDown
        | KeyCode::Home
        | KeyCode::End
        | KeyCode::Backspace
        | KeyCode::Delete => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests;
