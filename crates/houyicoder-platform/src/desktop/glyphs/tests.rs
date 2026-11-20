//! Tests for this module.

use unicode_width::UnicodeWidthStr;

use super::{
    CONSOLE_SPINNER_FRAMES, GlyphPreference, GlyphSet, HostFonts, RICH_SPINNER_FRAMES,
    SAFE_SPINNER_FRAMES, preference_from, resolve_glyph_set, resolve_mode_marks,
    resolve_spinner_frames,
};

/// Every Safe glyph is plain ASCII: no font can miss it and no width
/// table can disagree about it.
#[test]
fn test_safe_glyphs_ascii() {
    let safe = GlyphSet::Safe;
    let all = [
        safe.tool_marker(),
        safe.result_gutter(),
        safe.assistant_marker(),
        safe.notice_marker(),
        safe.recap_prefix(),
        safe.picker_cursor(),
        safe.separator_dot(),
        safe.em_dash(),
        safe.ellipsis(),
        safe.toggle_on(),
        safe.toggle_off(),
        safe.toggle_pending(),
        safe.gate_open(),
        safe.gate_blocked(),
        safe.warning_marker(),
        safe.info_marker(),
        safe.paused(),
    ];
    for glyph in all {
        assert!(glyph.is_ascii(), "a Safe glyph must be ASCII: {glyph:?}");
    }
}

/// Every glyph a row leads with, in both grades, is exactly one display
/// column under both the ordinary and the CJK width rules. A glyph that
/// widened under either would push the text it leads on every row it
/// appears, which is the jitter this module exists to prevent. Inline
/// punctuation is measured by the wrapping table instead and is not part
/// of this contract.
#[test]
fn test_marker_glyphs_one_column() {
    for set in [GlyphSet::Safe, GlyphSet::Rich] {
        let leading = [
            set.tool_marker(),
            set.result_gutter(),
            set.assistant_marker(),
            set.notice_marker(),
            set.gate_open(),
            set.gate_blocked(),
            set.warning_marker(),
            set.info_marker(),
        ];
        for glyph in leading {
            for (glyph, width) in [
                (glyph, UnicodeWidthStr::width(glyph)),
                (glyph, UnicodeWidthStr::width_cjk(glyph)),
            ] {
                assert_eq!(
                    width, 1,
                    "a leading glyph must be one column: {glyph:?} (set {set:?})"
                );
            }
        }
        assert_eq!(
            UnicodeWidthStr::width(set.picker_cursor()),
            2,
            "the picker cursor is its glyph plus the separating space"
        );
        assert_eq!(
            UnicodeWidthStr::width_cjk(set.picker_cursor()),
            2,
            "the picker cursor is two columns under CJK rules too"
        );
        // A toggle's brackets make it wider than one column in the safe
        // grade; what matters is that the same table measures it the same
        // way under both policies, so the label after it stays put.
        for mark in [
            set.toggle_on(),
            set.toggle_off(),
            set.toggle_pending(),
            set.paused(),
        ] {
            assert_eq!(
                UnicodeWidthStr::width(mark),
                UnicodeWidthStr::width_cjk(mark),
                "a state mark keeps one width under both rules: {mark:?}"
            );
        }
    }
}

/// Every spinner frame, in both grades, is exactly one column. A frame
/// that widened would push the verb text on the frames it appears in.
#[test]
fn test_spinner_one_column() {
    assert_eq!(GlyphSet::Safe.spinner_frames(), SAFE_SPINNER_FRAMES);
    assert_eq!(GlyphSet::Rich.spinner_frames(), RICH_SPINNER_FRAMES);
    for set in [
        SAFE_SPINNER_FRAMES,
        CONSOLE_SPINNER_FRAMES,
        RICH_SPINNER_FRAMES,
    ] {
        for frame in set {
            assert_eq!(
                UnicodeWidthStr::width(*frame),
                1,
                "a spinner frame must be one column: {frame:?}"
            );
            assert_eq!(
                UnicodeWidthStr::width_cjk(*frame),
                1,
                "a spinner frame must be one column under CJK rules: {frame:?}"
            );
        }
    }
}

/// A pinned preference wins over both the terminal and the host fonts,
/// which is what lets a user on an unrecognized terminal reach the rich
/// grade and one on a recognized terminal leave it.
#[test]
fn test_preference_pins_grade() {
    assert_eq!(
        resolve_glyph_set(
            Some("ghostty"),
            None,
            None,
            None,
            GlyphPreference::Safe,
            HostFonts::SystemSymbols
        ),
        GlyphSet::Safe
    );
    assert_eq!(
        resolve_glyph_set(
            None,
            None,
            None,
            None,
            GlyphPreference::Rich,
            HostFonts::SystemSymbols
        ),
        GlyphSet::Rich
    );
}

/// Terminals that identify themselves through a known variable answer
/// Rich; the unknown answer is Safe.
#[test]
fn test_known_terminal_rich() {
    let case = |program, wt, wez, ghostty| {
        resolve_glyph_set(
            program,
            wt,
            wez,
            ghostty,
            GlyphPreference::Auto,
            HostFonts::SystemSymbols,
        )
    };
    assert_eq!(case(Some("WezTerm"), None, None, None), GlyphSet::Rich);
    assert_eq!(
        case(Some("Apple_Terminal"), None, None, None),
        GlyphSet::Rich
    );
    assert_eq!(case(None, Some("session"), None, None), GlyphSet::Rich);
    assert_eq!(case(None, None, Some("1"), None), GlyphSet::Rich);
    assert_eq!(case(Some("ghostty"), None, None, None), GlyphSet::Rich);
}

/// An unknown terminal program answers Safe, which is the set that cannot
/// break: no identification, no claim about the font. An empty identifier
/// is the same as none, so the pure decision and the detector agree.
#[test]
fn test_unknown_terminal_safe() {
    let case = |program| {
        resolve_glyph_set(
            program,
            None,
            None,
            None,
            GlyphPreference::Auto,
            HostFonts::SystemSymbols,
        )
    };
    assert_eq!(case(None), GlyphSet::Safe);
    assert_eq!(case(Some("dvtm")), GlyphSet::Safe);
    assert_eq!(case(Some("")), GlyphSet::Safe);
}

/// A modern terminal on Windows still draws with the console fonts, which
/// lack the blocks the rich grade uses; the grade stays Safe however the
/// terminal identifies itself. This is the case Windows Terminal sets
/// WT_SESSION and shows a missing-glyph box for the rich tool marker.
#[test]
fn test_windows_fonts_stay_safe() {
    let case = |program, wt| {
        resolve_glyph_set(
            program,
            wt,
            None,
            None,
            GlyphPreference::Auto,
            HostFonts::Windows,
        )
    };
    assert_eq!(case(None, Some("session")), GlyphSet::Safe);
    assert_eq!(case(Some("WezTerm"), None), GlyphSet::Safe);
    assert_eq!(
        GlyphSet::Safe.notice_marker(),
        "*",
        "safe notice marker avoids an exclamation alarm"
    );
}

/// System-symbol hosts use the star-breathing sequence on recognized
/// terminals, Windows console fonts use the eight-dot rotation, and unknown
/// terminals stay on the ASCII rotation.
#[test]
fn test_known_terminal_spinner_rich() {
    assert_eq!(
        resolve_spinner_frames(
            Some("iTerm.app"),
            None,
            None,
            None,
            GlyphPreference::Auto,
            HostFonts::SystemSymbols,
        ),
        RICH_SPINNER_FRAMES
    );
    assert_eq!(
        resolve_spinner_frames(
            None,
            Some("session"),
            None,
            None,
            GlyphPreference::Auto,
            HostFonts::Windows,
        ),
        CONSOLE_SPINNER_FRAMES
    );
    assert_eq!(
        resolve_spinner_frames(
            None,
            None,
            None,
            None,
            GlyphPreference::Auto,
            HostFonts::SystemSymbols,
        ),
        SAFE_SPINNER_FRAMES
    );
    assert_eq!(
        resolve_spinner_frames(
            Some("iTerm.app"),
            None,
            None,
            None,
            GlyphPreference::Safe,
            HostFonts::SystemSymbols,
        ),
        SAFE_SPINNER_FRAMES
    );
    assert_eq!(
        resolve_spinner_frames(
            None,
            None,
            None,
            None,
            GlyphPreference::Rich,
            HostFonts::Windows,
        ),
        RICH_SPINNER_FRAMES
    );
}

/// Mode marks unify manual mode on the hollow bullet across modern desktop
/// and Windows terminals while using neutral-width auto marks per font host.
#[test]
fn test_mode_marks_neutral_width() {
    let win = resolve_mode_marks(
        None,
        Some("session"),
        None,
        None,
        GlyphPreference::Auto,
        HostFonts::Windows,
    );
    assert_eq!(win, ("»", "◦"));
    let mac = resolve_mode_marks(
        Some("iTerm.app"),
        None,
        None,
        None,
        GlyphPreference::Auto,
        HostFonts::SystemSymbols,
    );
    assert_eq!(mac, ("⏵⏵", "◦"));
    let legacy = resolve_mode_marks(
        None,
        None,
        None,
        None,
        GlyphPreference::Auto,
        HostFonts::Windows,
    );
    assert_eq!(legacy, (">>", "o"));
    assert_eq!(
        resolve_mode_marks(
            Some("iTerm.app"),
            None,
            None,
            None,
            GlyphPreference::Safe,
            HostFonts::SystemSymbols,
        ),
        (">>", "o")
    );
    assert_eq!(
        resolve_mode_marks(
            None,
            None,
            None,
            None,
            GlyphPreference::Rich,
            HostFonts::Windows,
        ),
        ("⏵⏵", "◦")
    );
    for (auto_mark, manual_mark) in [win, mac, legacy] {
        for mark in [auto_mark, manual_mark] {
            assert_eq!(
                UnicodeWidthStr::width(mark),
                UnicodeWidthStr::width_cjk(mark),
                "mode mark keeps one width under CJK rules: {mark:?}"
            );
        }
    }
}

/// The setting accepts both grade names, the alphabet name of the safe
/// grade, and either case.
#[test]
fn test_setting_names_grades() {
    for value in ["safe", "SAFE", "ascii", "Ascii"] {
        assert_eq!(
            preference_from(Some(value)),
            GlyphPreference::Safe,
            "{value} asks for the safe grade"
        );
    }
    assert_eq!(preference_from(Some("rich")), GlyphPreference::Rich);
    assert_eq!(preference_from(Some("RICH")), GlyphPreference::Rich);
    assert_eq!(preference_from(None), GlyphPreference::Auto);
    assert_eq!(preference_from(Some("nonsense")), GlyphPreference::Auto);
}
