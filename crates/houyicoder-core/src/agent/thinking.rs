//! Brief helper for reasoning text: truncate to a one-line summary so a
//! collapsed thinking block fits in a single transcript row. The full text
//! stays in the AssistantMessage thinking field and the raw Reasoning events;
//! this helper only produces the label a host renders in the collapsed view.

/// Maximum characters in a brief before it is cut with an ellipsis.
const BRIEF_MAX: usize = 120;

/// Produce a one-line summary of reasoning text. Returns the first
/// non-empty line, truncated to BRIEF_MAX characters with an ellipsis when
/// it exceeds the budget. Returns an empty string for empty input.
pub fn thinking_brief(text: &str) -> String {
    let line = text
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    if line.len() <= BRIEF_MAX {
        return line.to_string();
    }
    let cut: String = line.chars().take(BRIEF_MAX).collect();
    let mut brief = cut;
    brief.push('\u{2026}');
    brief
}

#[cfg(test)]
mod tests;
