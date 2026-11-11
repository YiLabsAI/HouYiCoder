//! Fold a burst of key events into one paste.
//!
//! One platform reports a clipboard paste as a run of ordinary key events and
//! offers no paste event, so the text would reach the editor as keystrokes and
//! every newline would submit the draft. The reader collects the run while the
//! console queue still holds a record and hands it on as one text event. The
//! test for a run is the queue, not a clock: a typed key leaves an empty
//! queue, while a paste leaves its next record already waiting.

use std::io;
use std::time::Duration;

use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};

/// The idle gap that ends a burst once the console drains between records.
///
/// It is only paid after the run is already paste-shaped: a typed key never
/// waits here, so the next keystroke keeps its own meaning.
const BURST_GAP: Duration = Duration::from_millis(8);
/// The most records one burst folds, which bounds the memory a single paste
/// can claim before the rest arrives as the next burst.
const MAX_BURST_RECORDS: usize = 131_072;

/// What one collected record contributes to a paste run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Contribution {
    /// Part of the pasted text.
    Text,
    /// An event that is not text but must survive the fold unchanged.
    Preserved,
    /// A real key that ends the text run.
    Ends,
}

/// The text a burst contributes, built record by record.
#[derive(Debug, Default)]
struct PasteBurst {
    text: String,
    characters: usize,
    last_non_bmp: Option<char>,
    released_since_character: bool,
}

impl PasteBurst {
    /// Add one record and report what it contributes to the run.
    fn push(&mut self, event: &Event) -> Contribution {
        let Event::Key(key) = event else {
            return Contribution::Preserved;
        };
        match key.kind {
            KeyEventKind::Release => {
                self.released_since_character = true;
                return Contribution::Text;
            }
            KeyEventKind::Press | KeyEventKind::Repeat => {}
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('j') {
            self.text.push('\n');
            self.characters += 1;
            return Contribution::Text;
        }
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            // A layout may deliver a printable character with both control and
            // alt held (AltGr on a keyboard that maps the third level that
            // way). It is text, not a chord, and dropping it would lose that
            // character from the paste.
            let printable = key
                .modifiers
                .contains(KeyModifiers::CONTROL | KeyModifiers::ALT)
                && matches!(key.code, KeyCode::Char(c) if !c.is_control());
            if !printable {
                return Contribution::Ends;
            }
        }
        match key.code {
            KeyCode::Char(c) => {
                // One character above the basic plane arrives as two presses
                // with no release between them; a real second character has a
                // release first.
                if c > '\u{FFFF}' && !self.released_since_character && self.last_non_bmp == Some(c)
                {
                    self.last_non_bmp = None;
                    return Contribution::Text;
                }
                self.last_non_bmp = (c > '\u{FFFF}').then_some(c);
                self.text.push(c);
            }
            // The paste markers carry an escape byte; inside a burst it is
            // text, and the markers are removed before the run is folded.
            KeyCode::Esc => self.text.push('\u{1b}'),
            KeyCode::Enter => self.text.push('\n'),
            KeyCode::Tab => self.text.push('\t'),
            _ => return Contribution::Ends,
        }
        self.characters += 1;
        self.released_since_character = false;
        Contribution::Text
    }

    /// Whether the run so far can only be a paste.
    ///
    /// A run of two or more characters, one carrying a newline with text after
    /// it, or the terminal's own paste marker is a paste. A single key, with or
    /// without its release, is not: treating it as one would delay it and let a
    /// following key be folded into it.
    fn paste_shaped(&self) -> bool {
        self.characters >= 2
            || self.text.contains("\u{1b}[200~")
            || self
                .text
                .rfind('\n')
                .is_some_and(|at| at + 1 < self.text.len())
    }

    /// The paste text, or None when the run is not a paste. The terminal's own
    /// paste markers are removed from the text.
    fn paste_text(self) -> Option<String> {
        if !self.paste_shaped() {
            return None;
        }
        Some(
            self.text
                .replace("\u{1b}[200~", "")
                .replace("\u{1b}[201~", ""),
        )
    }
}

/// One candidate paste, collecting records while it decides.
#[derive(Debug, Default)]
struct PasteRun {
    text: PasteBurst,
    /// Events that are not text, kept to emit after the paste.
    preserved: Vec<Event>,
    /// The real key that ended the text run, if one did.
    ending: Option<Event>,
    /// Every record in arrival order, returned unchanged when the run turns
    /// out not to be a paste.
    records: Vec<Event>,
}

impl PasteRun {
    /// Add one record and report what it contributed.
    fn absorb(&mut self, event: Event) -> Contribution {
        let contribution = self.text.push(&event);
        match contribution {
            Contribution::Text => {}
            Contribution::Preserved => self.preserved.push(event.clone()),
            Contribution::Ends => self.ending = Some(event.clone()),
        }
        self.records.push(event);
        contribution
    }

    /// Take every record the console already holds, without waiting. False
    /// means a real key stopped the run.
    fn fill_ready(
        &mut self,
        pending: &mut impl FnMut(Duration) -> bool,
        read: &mut impl FnMut() -> io::Result<Event>,
    ) -> bool {
        self.fill(pending, read, Duration::ZERO)
    }

    /// Take records while the console keeps producing them within the gap.
    fn fill_waiting(
        &mut self,
        pending: &mut impl FnMut(Duration) -> bool,
        read: &mut impl FnMut() -> io::Result<Event>,
    ) {
        self.fill(pending, read, BURST_GAP);
    }

    fn fill(
        &mut self,
        pending: &mut impl FnMut(Duration) -> bool,
        read: &mut impl FnMut() -> io::Result<Event>,
        window: Duration,
    ) -> bool {
        while self.records.len() < MAX_BURST_RECORDS && pending(window) {
            let Ok(event) = read() else {
                return true;
            };
            if self.absorb(event) == Contribution::Ends {
                return false;
            }
        }
        true
    }

    /// The events to hand the run loop: one paste followed by whatever was
    /// preserved and the key that ended the run, or every record unchanged
    /// when the run is not a paste.
    fn finish(self) -> Vec<Event> {
        let Some(text) = self.text.paste_text() else {
            return self.records;
        };
        let mut events = Vec::with_capacity(2 + self.preserved.len());
        events.push(Event::Paste(text));
        events.extend(self.preserved);
        events.extend(self.ending);
        events
    }
}

/// Collect the records that follow one key event while the console queue has
/// more, folding a text run into a single paste event.
///
/// Nothing waits on the console up front: a run that is already paste-shaped
/// keeps collecting within a short gap, and anything else is handed back
/// exactly as it arrived so the next keystroke is never delayed or swallowed.
/// The readiness check and the read are supplied so the burst rules can be
/// driven without a console.
pub(crate) fn coalesce_with(
    first: Event,
    mut pending: impl FnMut(Duration) -> bool,
    mut read: impl FnMut() -> io::Result<Event>,
) -> Vec<Event> {
    if !matches!(first, Event::Key(_)) || !pending(Duration::ZERO) {
        return vec![first];
    }
    let mut run = PasteRun::default();
    run.absorb(first);
    if !run.fill_ready(&mut pending, &mut read) {
        return run.finish();
    }
    if !run.text.paste_shaped() {
        return run.finish();
    }
    run.fill_waiting(&mut pending, &mut read);
    run.finish()
}

#[cfg(test)]
mod tests;
