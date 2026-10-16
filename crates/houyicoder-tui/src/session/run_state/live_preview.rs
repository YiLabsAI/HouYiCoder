//! The bounded display form of one live block's streamed text.

use crate::view::line_wrap::{DISPLAY_HEAD_CHARS, DISPLAY_TAIL_CHARS};

/// What a preview holds once the middle of the stream has been released.
const RETAINED_CHARS: usize = DISPLAY_HEAD_CHARS + DISPLAY_TAIL_CHARS;

/// How far the buffer may grow past the retained bound before the middle is
/// released. Releasing on the delta that crosses the bound would rebuild the
/// display form once per delta for the rest of the stream; letting the excess
/// gather first turns that into one rebuild per quantum of text.
const RELEASE_QUANTUM: usize = 512;

/// One live block's text, bounded as it streams.
///
/// The bound sits on the buffer rather than at the renderer because the
/// buffer is what grows without end: a long turn streams far more text into
/// one block than the block ever shows at once, and every frame re-renders
/// whatever the block holds. The text released in the middle is not the only
/// copy — the turn's frames carry it, and the row committed for the block
/// renders from them — so a reader still reaches the whole text once the
/// block lands.
#[derive(Debug, Default)]
pub(crate) struct LivePreview {
    /// Head, elision marker, and tail: the form the renderers read.
    display: String,
    /// Streamed characters the display form holds, the generated marker
    /// excluded. Past the retained bound it sits between that bound and the
    /// bound plus one release quantum.
    retained_chars: usize,
    /// Every character streamed in since the last clear, the released ones
    /// included, so a count over the stream keeps climbing past the bound.
    streamed_chars: usize,
}

impl LivePreview {
    /// Append a streamed delta. Once the excess over the retained bound
    /// reaches a whole quantum, the characters between the head and the
    /// newest tail are released.
    pub(crate) fn push_str(&mut self, delta: &str) {
        let arrived = delta.chars().count();
        self.display.push_str(delta);
        self.streamed_chars += arrived;
        self.retained_chars += arrived;
        if self.retained_chars > RETAINED_CHARS + RELEASE_QUANTUM {
            self.release_elapsed();
        }
    }

    /// Drop the whole stream, for a block whose text has landed as a durable
    /// row, or for a turn that ended.
    pub(crate) fn clear(&mut self) {
        self.display.clear();
        self.retained_chars = 0;
        self.streamed_chars = 0;
    }

    /// The display form.
    pub(crate) fn as_str(&self) -> &str {
        &self.display
    }

    /// Whether nothing has arrived.
    pub(crate) fn is_empty(&self) -> bool {
        self.streamed_chars == 0
    }

    /// Every character that has arrived since the last clear. The bound the
    /// display form carries does not bound this count.
    pub(crate) fn streamed_chars(&self) -> usize {
        self.streamed_chars
    }

    /// Keep the head and the newest tail, and name every character released
    /// so far. Characters are the unit because a released stretch can sit
    /// inside a single line, where a line count would read as none.
    fn release_elapsed(&mut self) {
        let shown = self.display.chars().count();
        let head_end = char_offset(&self.display, DISPLAY_HEAD_CHARS);
        let tail_start = char_offset(&self.display, shown - DISPLAY_TAIL_CHARS);
        let head = &self.display[..head_end];
        let tail = &self.display[tail_start..];
        let released = self.streamed_chars - RETAINED_CHARS;
        self.display = format!("{head}\n… +{released} chars …\n{tail}");
        self.retained_chars = RETAINED_CHARS;
    }
}

impl From<&str> for LivePreview {
    /// A preview of a whole text, as if it had arrived as one delta: the
    /// same bound applies.
    fn from(text: &str) -> Self {
        let mut preview = Self::default();
        preview.push_str(text);
        preview
    }
}

/// The byte offset where the nth character of the text begins, or the text's
/// length when it holds fewer characters. The offset is always a character
/// boundary, so a slice taken at it cannot split a character in two.
fn char_offset(text: &str, nth: usize) -> usize {
    text.char_indices()
        .nth(nth)
        .map_or(text.len(), |(at, _)| at)
}

#[cfg(test)]
mod tests;
