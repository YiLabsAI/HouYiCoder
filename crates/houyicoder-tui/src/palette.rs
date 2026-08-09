//! Slash-command palette state: open/close, the inline filter query, and the
//! selection index. Extracted from App so the palette surface is one cohesive
//! unit; App delegates the palette methods here.

use houyicoder_protocol::frontend::SlashCommand;

/// Palette overlay state: whether it is open, the inline filter query, and the
/// selected index into the filtered list. All fields are stub state mutated by
/// the key handlers.
#[derive(Debug, Clone, Default)]
pub struct PaletteState {
    /// Whether the palette overlay is currently shown.
    pub open: bool,
    /// Selected index into the filtered list.
    pub sel: usize,
    /// Inline filter query typed while the palette is open. Empty = show all.
    pub query: String,
}

impl PaletteState {
    /// The filtered slash-command list, narrowed by the inline query. Empty
    /// query returns the full set. Match is case-insensitive substring on the
    /// command name (without the leading slash), so typing "spe" narrows to
    /// /spec, /release-notes, etc.
    pub fn filtered(&self) -> Vec<SlashCommand> {
        let q = self.query.trim().to_ascii_lowercase();
        let q = q.strip_prefix('/').unwrap_or(&q);
        if q.is_empty() {
            return SlashCommand::ALL.to_vec();
        }
        SlashCommand::ALL
            .iter()
            .filter(|c| {
                c.name()
                    .trim_start_matches('/')
                    .to_ascii_lowercase()
                    .contains(q)
            })
            .copied()
            .collect()
    }

    /// Number of palette entries after filtering.
    pub fn len(&self) -> usize {
        self.filtered().len()
    }

    /// True when no commands match the current filter.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The currently selected slash command, if the palette is open and the
    /// filtered list is non-empty.
    pub fn selected(&self) -> Option<SlashCommand> {
        if !self.open {
            return None;
        }
        self.filtered().get(self.sel).copied()
    }

    /// Move the selection up, wrapping around the filtered list.
    pub fn prev(&mut self) {
        let n = self.len();
        if n > 0 {
            self.sel = (self.sel + n - 1) % n;
        }
    }

    /// Move the selection down, wrapping around the filtered list.
    pub fn next(&mut self) {
        let n = self.len();
        if n > 0 {
            self.sel = (self.sel + 1) % n;
        }
    }

    /// Push a character onto the inline filter query and reset the selection.
    pub fn push(&mut self, c: char) {
        self.query.push(c);
        self.sel = 0;
    }

    /// Remove the trailing character of the inline filter query.
    pub fn pop(&mut self) {
        self.query.pop();
        self.sel = 0;
    }

    /// Open the palette with an empty filter at the first command.
    pub fn open(&mut self) {
        self.open = true;
        self.sel = 0;
        self.query.clear();
    }

    /// Close the palette without running a command.
    pub fn close(&mut self) {
        self.open = false;
        self.query.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_closed_empty() {
        let p = PaletteState::default();
        assert!(!p.open);
        assert!(p.query.is_empty());
        assert_eq!(p.sel, 0);
    }

    #[test]
    fn test_open_resets_palette_state() {
        let mut p = PaletteState::default();
        p.query.push('x');
        p.sel = 3;
        p.open();
        assert!(p.open);
        assert!(p.query.is_empty());
        assert_eq!(p.sel, 0);
    }

    #[test]
    fn test_filter_narrows_then_resets() {
        let mut p = PaletteState::default();
        p.open();
        let full = p.len();
        assert_eq!(full, SlashCommand::ALL.len());
        p.push('s');
        p.push('p');
        p.push('e');
        assert!(p.len() < full);
        let cmd = p.selected().expect("filtered list non-empty");
        assert!(cmd.name().contains("spe"));
    }
}
