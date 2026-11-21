//! The glyph vocabulary the transcript draws with, in three grades.
//!
//! Safe is plain ASCII, present in every font. Console is the neutral-width
//! subset carried by Windows console fonts (Cascadia Mono and Consolas). Rich
//! is the symbol blocks a desktop terminal backed by system symbol fonts
//! carries. Every leading marker, gutter, cursor, toggle, gate mark, down
//! mark, and spinner frame keeps identical column width under both ordinary
//! and East-Asian width rules. Inline punctuation is the exception: the rich
//! forms are Ambiguous-width, so a renderer that wraps must measure them with
//! the terminal's own table rather than counting columns itself.

use std::env::var;
use std::sync::OnceLock;

/// Plain ASCII spinner frames for unknown terminals and legacy consoles.
pub const SAFE_SPINNER_FRAMES: &[&str] = &["|", "/", "-", "\\"];

/// Eight-dot rotation for modern Windows terminals whose console fonts carry
/// this block while lacking the dingbat star block.
pub const CONSOLE_SPINNER_FRAMES: &[&str] = &["⣾", "⣽", "⣻", "⢿", "⡿", "⣟", "⣯", "⣷"];

/// Star-breathing sequence for terminals backed by system symbol fonts. Every
/// frame has neutral East-Asian width so CJK terminals hold one column.
pub const RICH_SPINNER_FRAMES: &[&str] = &["∙", "✢", "✳", "✶", "✻", "✶", "✳", "✢"];

/// One glyph grade: the values a caller draws rows with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GlyphSet {
    /// Plain ASCII only. Used for unknown terminals, legacy consoles without
    /// symbol font coverage, and an explicit safe override.
    Safe,
    /// Neutral-width glyphs covered by Windows console fonts (Cascadia Mono
    /// and Consolas) without reaching into technical or dingbat blocks.
    Console,
    /// The richer symbols, for desktop terminals backed by system symbol
    /// fonts: one column wide and font-covered on those hosts.
    Rich,
}

impl GlyphSet {
    /// The marker a tool call or finished delegation row leads with. Shares
    /// one shape with assistant_marker so color alone distinguishes a reply
    /// from an external action.
    #[must_use]
    pub fn tool_marker(&self) -> &'static str {
        match self {
            Self::Safe => "*",
            Self::Console => "∙",
            Self::Rich => "⏺",
        }
    }

    /// The gutter a tool result or continuation row indents under its call.
    #[must_use]
    pub fn result_gutter(&self) -> &'static str {
        match self {
            Self::Safe => "|",
            Self::Console => "›",
            Self::Rich => "⎿",
        }
    }

    /// The marker the assistant's reply row leads with. Shares one shape with
    /// tool_marker in default foreground so primary utterances and tool calls
    /// read as one column vocabulary differentiated by color.
    #[must_use]
    pub fn assistant_marker(&self) -> &'static str {
        self.tool_marker()
    }

    /// The marker a system notice, reasoning summary, or memory receipt row
    /// leads with. All grades stay calm rather than using an exclamation
    /// mark, which reads as a warning.
    #[must_use]
    pub fn notice_marker(&self) -> &'static str {
        match self {
            Self::Safe | Self::Console => "*",
            Self::Rich => "✻",
        }
    }

    /// The downward mark on the scrollback jump badge. Uses the neutral-width
    /// small downward triangle on modern desktop and Windows console hosts so
    /// East-Asian width tables never widen the badge rectangle.
    #[must_use]
    pub fn down_mark(&self) -> &'static str {
        match self {
            Self::Safe => "v",
            Self::Console | Self::Rich => "▾",
        }
    }

    /// The marker a toggle in its on state shows.
    #[must_use]
    pub fn toggle_on(&self) -> &'static str {
        match self {
            Self::Safe | Self::Console => "[x]",
            Self::Rich => "◉",
        }
    }

    /// The marker a toggle in its off state shows.
    #[must_use]
    pub fn toggle_off(&self) -> &'static str {
        match self {
            Self::Safe | Self::Console => "[ ]",
            Self::Rich => "◌",
        }
    }

    /// The marker a toggle shows while its change is running. The pane
    /// colours it apart from a settled state, and the mark itself differs
    /// from both settled marks so a test can tell the states apart.
    #[must_use]
    pub fn toggle_pending(&self) -> &'static str {
        match self {
            Self::Safe | Self::Console => "[-]",
            Self::Rich => "◍",
        }
    }

    /// The gate mark a row shows when it can be used.
    #[must_use]
    pub fn gate_open(&self) -> &'static str {
        match self {
            Self::Safe | Self::Console => "+",
            Self::Rich => "✓",
        }
    }

    /// The gate mark a row shows when it is blocked.
    #[must_use]
    pub fn gate_blocked(&self) -> &'static str {
        match self {
            Self::Safe | Self::Console => "x",
            Self::Rich => "✗",
        }
    }

    /// The mark a paused queue item or a stalled run shows.
    #[must_use]
    pub fn paused(&self) -> &'static str {
        match self {
            Self::Safe | Self::Console => "||",
            Self::Rich => "⏸",
        }
    }

    /// The mark a row that is next in line shows. The ASCII arrow across
    /// grades: the drawn arrow is Ambiguous-width, so it would widen the
    /// row it leads on a CJK terminal.
    #[must_use]
    pub fn arrow(&self) -> &'static str {
        "->"
    }

    /// The prefix a recap row leads with, including its trailing space.
    /// Every form is Neutral-width so the row's leading columns hold.
    #[must_use]
    pub fn recap_prefix(&self) -> &'static str {
        match self {
            Self::Safe => "* recap: ",
            Self::Console | Self::Rich => "» recap: ",
        }
    }

    /// The cursor a picker highlights the selected row with, including its
    /// trailing space.
    #[must_use]
    pub fn picker_cursor(&self) -> &'static str {
        match self {
            Self::Safe => "> ",
            Self::Console => "▸ ",
            Self::Rich => "❯ ",
        }
    }

    /// The inline separator between two facts on one line.
    #[must_use]
    pub fn separator_dot(&self) -> &'static str {
        match self {
            Self::Safe | Self::Console => "-",
            Self::Rich => "·",
        }
    }

    /// The em dash used inside a sentence.
    #[must_use]
    pub fn em_dash(&self) -> &'static str {
        match self {
            Self::Safe | Self::Console => "--",
            Self::Rich => "—",
        }
    }

    /// The ellipsis a truncation ends with. The safe and console forms are
    /// three columns wide, so a caller measures the mark with the
    /// display-width table instead of assuming one column; a single dot would
    /// read as the end of a sentence rather than as a cut.
    #[must_use]
    pub fn ellipsis(&self) -> &'static str {
        match self {
            Self::Safe | Self::Console => "...",
            Self::Rich => "…",
        }
    }

    /// The marker a warning suggestion row leads with. Symbol hosts use the
    /// single-column warning sign; console and safe hosts use ASCII.
    #[must_use]
    pub fn warning_marker(&self) -> &'static str {
        match self {
            Self::Safe | Self::Console => "!",
            Self::Rich => "\u{26A0}",
        }
    }

    /// The marker an informational suggestion row leads with. Symbol hosts
    /// use the single-column information source mark; console and safe hosts
    /// use ASCII.
    #[must_use]
    pub fn info_marker(&self) -> &'static str {
        match self {
            Self::Safe | Self::Console => "i",
            Self::Rich => "\u{2139}",
        }
    }

    /// The spinner frames for this grade, every frame one column wide.
    #[must_use]
    pub fn spinner_frames(&self) -> &'static [&'static str] {
        match self {
            Self::Safe => SAFE_SPINNER_FRAMES,
            Self::Console => CONSOLE_SPINNER_FRAMES,
            Self::Rich => RICH_SPINNER_FRAMES,
        }
    }

    /// The status-bar permission mode marks (auto, manual) for this grade.
    #[must_use]
    pub fn mode_marks(&self) -> (&'static str, &'static str) {
        match self {
            Self::Safe => (">>", "o"),
            Self::Console => ("»", "◦"),
            Self::Rich => ("⏵⏵", "◦"),
        }
    }
}

/// What the environment asked for. Auto is the ordinary case; the explicit
/// variants let a user select a grade when auto-detection does not match
/// their font.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GlyphPreference {
    /// Decide from the terminal and the host fonts.
    Auto,
    /// Draw with the safe ASCII grade whatever the detection says.
    Safe,
    /// Draw with the neutral-width Windows console grade whatever detection says.
    Console,
    /// Draw with the rich symbol grade whatever the detection says.
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

fn is_known_terminal(
    program: Option<&str>,
    wt_session: Option<&str>,
    wezterm: Option<&str>,
    ghostty: Option<&str>,
) -> bool {
    fn non_empty(value: Option<&str>) -> Option<&str> {
        value.filter(|value| !value.is_empty())
    }
    let (program, wt_session, wezterm, ghostty) = (
        non_empty(program),
        non_empty(wt_session),
        non_empty(wezterm),
        non_empty(ghostty),
    );
    matches!(
        program,
        Some("ghostty" | "WezTerm" | "iTerm.app" | "Apple_Terminal" | "vscode")
    ) || wt_session.is_some()
        || wezterm.is_some()
        || ghostty.is_some()
}

/// The grade the running terminal should draw static markers with, decided
/// from the identifiers the terminal carries and the fonts the host resolves
/// to. Pure over its inputs so the decision is testable without the process
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
    match preference {
        GlyphPreference::Safe => return GlyphSet::Safe,
        GlyphPreference::Console => return GlyphSet::Console,
        GlyphPreference::Rich => return GlyphSet::Rich,
        GlyphPreference::Auto => {}
    }
    if !is_known_terminal(program, wt_session, wezterm, ghostty) {
        GlyphSet::Safe
    } else if fonts == HostFonts::Windows {
        GlyphSet::Console
    } else {
        GlyphSet::Rich
    }
}

/// Resolve the spinner frame slice for the given terminal and host fonts.
/// Terminals backed by system symbol fonts get the star-breathing sequence;
/// recognized modern terminals on Windows get the eight-dot rotation carried
/// by console fonts; unknown terminals and safe overrides stay plain ASCII.
#[must_use]
pub fn resolve_spinner_frames(
    program: Option<&str>,
    wt_session: Option<&str>,
    wezterm: Option<&str>,
    ghostty: Option<&str>,
    preference: GlyphPreference,
    fonts: HostFonts,
) -> &'static [&'static str] {
    resolve_glyph_set(program, wt_session, wezterm, ghostty, preference, fonts).spinner_frames()
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

/// The spinner frames for the running terminal, decided once.
#[must_use]
pub fn detect_spinner_frames() -> &'static [&'static str] {
    static FRAMES: OnceLock<&'static [&'static str]> = OnceLock::new();
    FRAMES.get_or_init(|| detect_glyph_set().spinner_frames())
}

/// Resolve the status-bar permission mode marks (auto, manual) for the given
/// terminal and host fonts. Both modern symbol hosts and modern Windows
/// terminals share the calm hollow bullet for manual mode, while Windows uses
/// the compact double angle quote in place of the double triangle that
/// console fonts omit. Every mark keeps neutral East-Asian width.
#[must_use]
pub fn resolve_mode_marks(
    program: Option<&str>,
    wt_session: Option<&str>,
    wezterm: Option<&str>,
    ghostty: Option<&str>,
    preference: GlyphPreference,
    fonts: HostFonts,
) -> (&'static str, &'static str) {
    resolve_glyph_set(program, wt_session, wezterm, ghostty, preference, fonts).mode_marks()
}

/// The status-bar permission mode marks (auto, manual) for the running
/// terminal, decided once.
#[must_use]
pub fn detect_mode_marks() -> (&'static str, &'static str) {
    static MARKS: OnceLock<(&'static str, &'static str)> = OnceLock::new();
    *MARKS.get_or_init(|| detect_glyph_set().mode_marks())
}

/// The setting a user sets to select a grade when auto-detection does not
/// match their font.
pub const GLYPH_SET_ENV: &str = "HOUYICODER_GLYPHS";

/// The preference this setting value asks for. Grade names and the alphabet
/// name of the safe grade are accepted, in either case.
fn preference_from(value: Option<&str>) -> GlyphPreference {
    match value {
        Some(value)
            if value.eq_ignore_ascii_case("safe") || value.eq_ignore_ascii_case("ascii") =>
        {
            GlyphPreference::Safe
        }
        Some(value) if value.eq_ignore_ascii_case("console") => GlyphPreference::Console,
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
