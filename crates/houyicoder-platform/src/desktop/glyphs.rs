//! The glyph vocabulary the transcript draws with, in two grades.
//!
//! Safe is plain ASCII, present in every font. Rich is the symbol blocks a
//! modern terminal carries. Every leading marker, gutter, cursor, toggle,
//! gate mark, and spinner frame is one column wide under both width
//! policies. Inline punctuation is the exception: the rich forms are
//! Ambiguous-width, so a renderer that wraps must measure them with the
//! terminal's own table rather than counting columns itself.

use std::env::var;
use std::sync::OnceLock;

/// One glyph grade: the values a caller draws rows with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GlyphSet {
    /// Plain ASCII only. Used for unknown terminals, terminals without the
    /// richer symbol blocks, and an explicit user preference.
    Safe,
    /// The richer symbols, for terminals that carry their blocks: one
    /// column wide and font-covered on the terminals this product targets.
    Rich,
}

impl GlyphSet {
    /// The marker a tool call row leads with.
    #[must_use]
    pub fn tool_marker(&self) -> &'static str {
        match self {
            Self::Safe => "*",
            Self::Rich => "⏺",
        }
    }

    /// The gutter a tool result or continuation row indents under its call.
    #[must_use]
    pub fn result_gutter(&self) -> &'static str {
        match self {
            Self::Safe => "|",
            Self::Rich => "⎿",
        }
    }

    /// The marker the assistant's reply row leads with.
    #[must_use]
    pub fn assistant_marker(&self) -> &'static str {
        match self {
            Self::Safe => "*",
            Self::Rich => "◉",
        }
    }

    /// The marker a system notice row leads with.
    #[must_use]
    pub fn notice_marker(&self) -> &'static str {
        match self {
            Self::Safe => "!",
            Self::Rich => "✻",
        }
    }

    /// The marker a toggle in its on state shows.
    #[must_use]
    pub fn toggle_on(&self) -> &'static str {
        match self {
            Self::Safe => "[x]",
            Self::Rich => "◉",
        }
    }

    /// The marker a toggle in its off state shows.
    #[must_use]
    pub fn toggle_off(&self) -> &'static str {
        match self {
            Self::Safe => "[ ]",
            Self::Rich => "◌",
        }
    }

    /// The marker a toggle shows while its change is in flight. The pane
    /// colours it apart from a settled state, and the mark itself differs
    /// from both settled marks so a test can tell the states apart.
    #[must_use]
    pub fn toggle_pending(&self) -> &'static str {
        match self {
            Self::Safe => "[-]",
            Self::Rich => "◍",
        }
    }

    /// The gate mark a row shows when it can be used.
    #[must_use]
    pub fn gate_open(&self) -> &'static str {
        match self {
            Self::Safe => "+",
            Self::Rich => "✓",
        }
    }

    /// The gate mark a row shows when it is blocked.
    #[must_use]
    pub fn gate_blocked(&self) -> &'static str {
        match self {
            Self::Safe => "x",
            Self::Rich => "✗",
        }
    }

    /// The mark a paused queue item or a stalled run shows.
    #[must_use]
    pub fn paused(&self) -> &'static str {
        match self {
            Self::Safe => "||",
            Self::Rich => "⏸",
        }
    }

    /// The mark a row that is next in line shows. The ASCII arrow in both
    /// grades: the drawn arrow is Ambiguous-width, so it would widen the
    /// row it leads on a CJK terminal.
    #[must_use]
    pub fn arrow(&self) -> &'static str {
        "->"
    }

    /// The mark a running mode shows, distinct from the paused mark.
    #[must_use]
    pub fn play(&self) -> &'static str {
        match self {
            Self::Safe => ">>",
            Self::Rich => "⏵⏵",
        }
    }

    /// The prefix a recap row leads with, including its trailing space.
    /// Both forms are Neutral-width so the row's leading columns hold.
    #[must_use]
    pub fn recap_prefix(&self) -> &'static str {
        match self {
            Self::Safe => "* recap: ",
            Self::Rich => "» recap: ",
        }
    }

    /// The cursor a picker highlights the selected row with, including its
    /// trailing space.
    #[must_use]
    pub fn picker_cursor(&self) -> &'static str {
        match self {
            Self::Safe => "> ",
            Self::Rich => "❯ ",
        }
    }

    /// The inline separator between two facts on one line.
    #[must_use]
    pub fn separator_dot(&self) -> &'static str {
        match self {
            Self::Safe => "-",
            Self::Rich => "·",
        }
    }

    /// The em dash used inside a sentence.
    #[must_use]
    pub fn em_dash(&self) -> &'static str {
        match self {
            Self::Safe => "--",
            Self::Rich => "—",
        }
    }

    /// The ellipsis a truncation ends with. Both grades are one column wide, so
    /// a column budget measures the same in either grade.
    #[must_use]
    pub fn ellipsis(&self) -> &'static str {
        match self {
            Self::Safe => ".",
            Self::Rich => "…",
        }
    }

    /// The spinner frames for this grade, every frame one column wide.
    #[must_use]
    pub fn spinner_frames(&self) -> &'static [&'static str] {
        match self {
            Self::Safe => &["|", "/", "-", "\\"],
            Self::Rich => &["⣾", "⣽", "⣻", "⢿", "⡿", "⣟", "⣯", "⣷"],
        }
    }
}

/// What the environment asked for. Auto is the ordinary case; the other two
/// let a user pin either grade when the detected one is wrong for their
/// font.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GlyphPreference {
    /// Decide from the terminal and the host fonts.
    Auto,
    /// Draw with the safe grade whatever the detection says.
    Safe,
    /// Draw with the rich grade whatever the detection says.
    Rich,
}

/// Which fonts the host draws with. Measured apart from the terminal
/// identifier because the two are not the same question: a modern terminal
/// on Windows still draws with the console fonts, and the symbol blocks the
/// rich grade uses are not in them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostFonts {
    /// The console fonts the Windows terminal and console resolve to:
    /// Cascadia Mono and Consolas. Their coverage stops short of the
    /// technical and dingbat blocks the rich grade draws from.
    Windows,
    /// A host whose terminal falls back through system symbol fonts.
    SystemSymbols,
}

/// The grade the running terminal should draw with, decided from the
/// identifiers the terminal carries and the fonts the host resolves to.
/// Pure over its inputs so the decision is testable without the process
/// environment.
#[must_use]
pub fn resolve_glyph_set(
    program: Option<&str>,
    wt_session: Option<&str>,
    wezterm: Option<&str>,
    ghostty: Option<&str>,
    preference: GlyphPreference,
    fonts: HostFonts,
) -> GlyphSet {
    fn non_empty(value: Option<&str>) -> Option<&str> {
        value.filter(|value| !value.is_empty())
    }
    let (program, wt_session, wezterm, ghostty) = (
        non_empty(program),
        non_empty(wt_session),
        non_empty(wezterm),
        non_empty(ghostty),
    );
    match preference {
        GlyphPreference::Safe => return GlyphSet::Safe,
        GlyphPreference::Rich => return GlyphSet::Rich,
        GlyphPreference::Auto => {}
    }
    // The terminal being modern says nothing about the font: Windows
    // Terminal answers yes and still draws Cascadia, which has no box for
    // the tool marker or the result gutter. Those hosts stay Safe.
    if fonts == HostFonts::Windows {
        return GlyphSet::Safe;
    }
    let rich = matches!(
        program,
        Some("ghostty" | "WezTerm" | "iTerm.app" | "vscode")
    ) || wt_session.is_some()
        || wezterm.is_some()
        || ghostty.is_some();
    if rich { GlyphSet::Rich } else { GlyphSet::Safe }
}

/// The grade the process's own terminal reports, decided once. Unknown and
/// unidentified terminals answer Safe, which is the set that cannot break.
#[must_use]
pub fn detect_glyph_set() -> GlyphSet {
    static SET: OnceLock<GlyphSet> = OnceLock::new();
    *SET.get_or_init(|| {
        resolve_glyph_set(
            env_var("TERM_PROGRAM").as_deref(),
            env_var("WT_SESSION").as_deref(),
            env_var("WezTerm").as_deref(),
            env_var("GHOSTTY_RESOURCES_DIR").as_deref(),
            preference_from(env_var("HOUYICODER_GLYPHS").as_deref()),
            host_fonts(),
        )
    })
}

/// The setting a user sets to pin a grade. Auto-detection is wrong when the
/// terminal is unrecognized but does carry the fonts, or recognized while
/// the font does not carry the glyphs.
pub const GLYPH_SET_ENV: &str = "HOUYICODER_GLYPHS";

/// The preference this setting value asks for. Both grade names and the
/// alphabet name of the safe grade are accepted, in either case.
fn preference_from(value: Option<&str>) -> GlyphPreference {
    match value {
        Some(value)
            if value.eq_ignore_ascii_case("safe") || value.eq_ignore_ascii_case("ascii") =>
        {
            GlyphPreference::Safe
        }
        Some(value) if value.eq_ignore_ascii_case("rich") => GlyphPreference::Rich,
        _ => GlyphPreference::Auto,
    }
}

/// The fonts this build's host resolves terminal glyphs through.
const fn host_fonts() -> HostFonts {
    #[cfg(target_os = "windows")]
    {
        HostFonts::Windows
    }
    #[cfg(not(target_os = "windows"))]
    {
        HostFonts::SystemSymbols
    }
}

/// The environment variable read as an owned string, or None when unset.
fn env_var(name: &str) -> Option<String> {
    var(name).ok().filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests;
