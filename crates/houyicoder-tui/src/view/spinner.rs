//! Spinner row rendering extracted from working.rs so the working-surface
//! draw file stays under the file-size gate. The live spinner row is rebuilt
//! each draw from the run-start Instant so the animation needs no mutable
//! tick state.

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

/// Spinner glyph bounce (6 frames forward + 6 reverse = 12), 120 ms per
/// frame, a standard 12-frame bounce. The live row is rebuilt each draw
/// from the run-start Instant so the animation needs no mutable tick state.
const SPINNER_FRAMES: [&str; 12] = ["·", "✢", "✳", "✶", "✻", "✽", "✽", "✻", "✶", "✳", "✢", "·"];

/// Build the full spinner row text for one draw: animated glyph, live-phase
/// verb (Thinking while the reasoning block is the active stream — held for
/// a 2-second minimum so a brief burst does not flash and vanish; Working
/// otherwise, including when assistant text has taken over the stream), and
/// the width-gated suffix. The token count is a chars/4 estimate that lerps
/// toward the actual value ~25% of the gap per draw so the meter climbs
/// smoothly instead of jumping.
pub fn spinner_row_text(
    app: &crate::state::App,
    elapsed: std::time::Duration,
    width: u16,
) -> String {
    let frame = (elapsed.as_millis() / 120) % 12;
    let glyph = SPINNER_FRAMES[frame as usize];
    let reasoning_active = app.live_block == crate::state::enums::LiveBlock::Thinking;
    let thinking_min_active = app
        .thinking_started_at
        .is_some_and(|t| t.elapsed().as_secs() < 2);
    let verb = if reasoning_active || thinking_min_active {
        "Thinking"
    } else {
        "Working"
    };
    let actual_tok = ((app.live_assistant_text.chars().count()
        + app.live_reasoning_text.chars().count())
        / 4) as u32;
    let displayed = app.displayed_tokens.get();
    let tok = if actual_tok > displayed {
        displayed + (actual_tok - displayed).div_ceil(4)
    } else {
        actual_tok
    };
    app.displayed_tokens.set(tok);
    format!(
        "{} {}… ({})",
        glyph,
        verb,
        spinner_suffix_gated(elapsed, tok, width)
    )
}

/// Stall threshold before the red fade-in begins (seconds without a delta).
const STALL_THRESHOLD_SECS: u64 = 3;

/// During reasoning (LiveBlock::Thinking) a slow token cadence is normal —
/// thinking tokens arrive sparsely, unlike the dense deltas of assistant
/// text. The stall gradient must not fire on healthy long thinking, or a
/// model that thinks for 30s reads as "stuck" and turns red. Raise the
/// threshold so reasoning stays calm; a genuine hang still trips the
/// gradient once it exceeds this longer window.
const STALL_THRESHOLD_REASONING_SECS: u64 = 10;

/// Duration of the red intensity fade-in after the stall threshold (seconds).
const STALL_FADE_SECS: u64 = 2;

/// Error red target color for the stall gradient.
const ERROR_RED: (u8, u8, u8) = (255, 107, 128);

/// Dim gray base color for the spinner glyph.
const DIM_GRAY: (u8, u8, u8) = (88, 88, 88);

/// Brighter gray the breathing pulse lerps toward at its peak.
const PULSE_GRAY: (u8, u8, u8) = (160, 160, 160);

/// Compute the stall intensity (0.0 = no stall, 1.0 = full red) from the
/// time since the last delta, against the given threshold. Returns 0.0
/// before the threshold, then ramps linearly to 1.0 over STALL_FADE_SECS
/// using fractional seconds so the fade is smooth, not stepped.
fn stall_intensity_with(last_delta_at: Option<std::time::Instant>, threshold_secs: u64) -> f32 {
    let Some(t) = last_delta_at else {
        return 0.0;
    };
    let elapsed = t.elapsed().as_secs_f32();
    let threshold = threshold_secs as f32;
    if elapsed < threshold {
        0.0
    } else {
        ((elapsed - threshold) / STALL_FADE_SECS as f32).min(1.0)
    }
}

/// Stall intensity for the default cadence (assistant text deltas). Slow here
/// does mean likely stuck — fire the red gradient at 3s.
pub fn stall_intensity(last_delta_at: Option<std::time::Instant>) -> f32 {
    stall_intensity_with(last_delta_at, STALL_THRESHOLD_SECS)
}

/// Stall intensity during reasoning. Thinking tokens arrive sparsely, so a
/// slow cadence is healthy, not stuck — push the red gradient out to 10s so
/// long thinking stays calm. A true hang still trips once it exceeds this.
pub fn stall_intensity_reasoning(last_delta_at: Option<std::time::Instant>) -> f32 {
    stall_intensity_with(last_delta_at, STALL_THRESHOLD_REASONING_SECS)
}

/// Interpolate between two RGB colors by intensity (0.0 = a, 1.0 = b).
fn lerp_color(a: (u8, u8, u8), b: (u8, u8, u8), t: f32) -> Color {
    let r = a.0 as f32 + (b.0 as f32 - a.0 as f32) * t;
    let g = a.1 as f32 + (b.1 as f32 - a.1 as f32) * t;
    let bl = a.2 as f32 + (b.2 as f32 - a.2 as f32) * t;
    Color::Rgb(r as u8, g as u8, bl as u8)
}

/// Compute the breathing pulse opacity for the tool-use effect. Returns 0.0
/// when no tool is active; otherwise a sine wave oscillating between 0.0 and
/// 0.3 at ~0.8 Hz (a gentle breathing rhythm, not a strobe).
pub fn tool_pulse_opacity(elapsed: std::time::Duration, tool_active: bool) -> f32 {
    if !tool_active {
        return 0.0;
    }
    let ms = elapsed.as_millis() as f32;
    let phase = (ms / 1250.0) * std::f32::consts::TAU;
    (phase.sin() * 0.5 + 0.5) * 0.3
}

/// Format the spinner suffix with progressive width gating. On narrow
/// terminals the token count is dropped first, then the duration. The width
/// parameter is the available terminal width for the spinner row.
pub fn spinner_suffix_gated(elapsed: std::time::Duration, tokens: u32, width: u16) -> String {
    let secs = elapsed.as_secs();
    let dur = if secs >= 60 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        format!("{}s", secs)
    };
    let tok = if tokens >= 1000 {
        format!("↓ {:.1}k tokens", tokens as f64 / 1000.0)
    } else {
        format!("↓ {} tokens", tokens)
    };
    // Width budget: the glyph + space + verb + ellipsis + parens take ~15
    // chars. The suffix components are: "8s · ↓ 42 tokens" (~18 chars).
    // Below 30 cols: drop tokens. Below 20 cols: drop duration too.
    if width < 20 {
        String::new()
    } else if width < 30 {
        dur
    } else {
        format!("{dur} · {tok}")
    }
}

/// Format the spinner suffix: duration (8m 14s / 14s) + ↓ N tokens (k-suffix
/// for thousands). A down-arrow token meter. Token count is
/// a live chars/4 estimate; authoritative usage lands on Done.
pub fn spinner_suffix(elapsed: std::time::Duration, tokens: u32) -> String {
    spinner_suffix_gated(elapsed, tokens, 80)
}

/// Shimmer + stall-gradient spinner row. A bright window slides across the
/// verb; the glyph color interpolates from dim gray to error red based on the
/// stall intensity (0.0 to 1.0). When a tool is active, the verb and suffix
/// breathe: their color lerps between dim and a brighter gray on a sine wave.
pub fn spinner_line(
    row: &str,
    elapsed: std::time::Duration,
    intensity: f32,
    tool_active: bool,
) -> Line<'static> {
    let dim = Style::new().fg(Color::DarkGray);
    // Glyph base is always the Rgb dim gray so the stall gradient starts from
    // the exact color it lerps from (no ANSI-to-Rgb jump at intensity > 0).
    let glyph_style = Style::new().fg(lerp_color(DIM_GRAY, ERROR_RED, intensity));
    let bright = Style::new().fg(Color::Cyan);
    // Row shape: "{glyph} {verb}… ({suffix})". Pull the verb out for shimmer.
    let mut chars = row.char_indices();
    let Some((_, glyph)) = chars.next() else {
        return Line::from(Span::styled(row.to_string(), dim));
    };
    let after_glyph = &row[glyph.len_utf8()..];
    let after_glyph_trim = after_glyph.trim_start();
    let (verb, tail) = match after_glyph_trim.find('…') {
        Some(i) => (&after_glyph_trim[..i], &after_glyph_trim[i..]),
        None => (after_glyph_trim, ""),
    };
    let mut spans: Vec<Span<'static>> = vec![Span::styled(glyph.to_string(), glyph_style)];
    if verb.is_empty() {
        spans.push(Span::styled(tail.to_string(), dim));
        return Line::from(spans);
    }
    spans.push(Span::raw(" ".to_string()));
    // Shimmer: a ~3-char bright window slides across the verb; rest dim.
    let vchars: Vec<char> = verb.chars().collect();
    let n = vchars.len();
    let pos = (elapsed.as_millis() / 100) as usize % (n + 4);
    let s_start = pos.min(n);
    let s_end = (s_start + 3).min(n);
    let before: String = vchars[..s_start].iter().collect();
    let mid: String = vchars[s_start..s_end].iter().collect();
    let after: String = vchars[s_end..].iter().collect();
    // Tool-use breathing pulse: lerp the non-shimmer text color between the
    // dim gray and a brighter gray, driven by the sine opacity (0.0 to 0.3
    // normalized to 0..1). No pulse: plain dim.
    let pulse = tool_pulse_opacity(elapsed, tool_active);
    let body_style = if pulse > 0.0 {
        Style::new().fg(lerp_color(DIM_GRAY, PULSE_GRAY, pulse / 0.3))
    } else {
        dim
    };
    if !before.is_empty() {
        spans.push(Span::styled(before, body_style));
    }
    if !mid.is_empty() {
        spans.push(Span::styled(mid, bright));
    }
    if !after.is_empty() {
        spans.push(Span::styled(after, body_style));
    }
    spans.push(Span::styled(tail.to_string(), body_style));
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_suffix_k() {
        let s = spinner_suffix(std::time::Duration::from_secs(494), 13400);
        assert!(s.contains("8m 14s"));
        assert!(s.contains("↓ 13.4k tokens"));
    }

    #[test]
    fn test_suffix_secs() {
        let s = spinner_suffix(std::time::Duration::from_secs(5), 80);
        assert!(s.contains("5s"));
        assert!(s.contains("↓ 80 tokens"));
    }

    #[test]
    fn test_suffix_gated_narrow() {
        let s = spinner_suffix_gated(std::time::Duration::from_secs(5), 80, 25);
        assert!(s.contains("5s"));
        assert!(!s.contains("tokens"));
    }

    #[test]
    fn test_suffix_gated_tiny() {
        let s = spinner_suffix_gated(std::time::Duration::from_secs(5), 80, 15);
        assert!(s.is_empty());
    }

    #[test]
    fn test_shimmers() {
        let line = spinner_line(
            "✻ Thinking… (5s · ↓ 80 tokens)",
            std::time::Duration::from_millis(200),
            0.0,
            false,
        );
        assert!(line.spans.len() >= 3);
    }

    #[test]
    fn test_stall_gradient_red() {
        let line = spinner_line(
            "✻ Thinking… (5s)",
            std::time::Duration::from_secs(4),
            1.0,
            false,
        );
        assert!(!line.spans.is_empty());
        let glyph_span = &line.spans[0];
        assert!(matches!(glyph_span.style.fg, Some(Color::Rgb(_, _, _))));
    }

    #[test]
    fn test_tool_pulse_bounded() {
        let pulse = tool_pulse_opacity(std::time::Duration::from_millis(0), true);
        assert!(pulse >= 0.0);
    }

    #[test]
    fn test_tool_pulse_off() {
        let pulse = tool_pulse_opacity(std::time::Duration::from_millis(500), false);
        assert_eq!(pulse, 0.0);
    }

    #[test]
    fn test_stall_intensity_none() {
        assert_eq!(stall_intensity(None), 0.0);
    }

    #[test]
    fn test_stall_reasoning_threshold_higher() {
        // Reasoning has a sparse token cadence; a 5s delta is healthy, so the
        // reasoning threshold (10s) must not fire red where the default (3s)
        // would. Pin both so a future tweak can't silently collapse them.
        let t = std::time::Instant::now() - std::time::Duration::from_secs(5);
        assert_eq!(stall_intensity_reasoning(Some(t)), 0.0);
        assert!(
            stall_intensity(Some(t)) > 0.0,
            "default 3s threshold fires at 5s"
        );
    }

    #[test]
    fn test_stall_reasoning_long_think() {
        // A 12s delta exceeds even the reasoning threshold (10s): a true
        // hang still trips the gradient, so long thinking is calm but a
        // genuine stall is not hidden.
        let t = std::time::Instant::now() - std::time::Duration::from_secs(12);
        assert!(
            stall_intensity_reasoning(Some(t)) > 0.0,
            "12s exceeds reasoning 10s threshold"
        );
    }
}
