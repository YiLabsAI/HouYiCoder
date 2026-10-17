//! Tests for the shared origin vocabulary: the label a key maps to, the
//! heading both views print, the order both sort by, and where an origin
//! outside the vocabulary lands.

use super::*;

fn row(origin: &str, name: &str) -> ContextSkillEntry {
    ContextSkillEntry {
        source: origin.to_string(),
        name: name.to_string(),
        tokens: 0,
        announced: true,
    }
}

/// A styled heading reduced to the text it paints, for comparing it against
/// the plain form of the same heading.
fn joined(spans: Vec<Span<'static>>) -> String {
    spans
        .iter()
        .map(|s| s.content.as_ref().to_string())
        .collect()
}

#[test]
fn test_origin_label_is_shared() {
    assert_eq!(origin_label("project"), "Native");
    assert_eq!(origin_label("claude_eco"), "Ecosystem");
    assert_eq!(
        origin_group("project").map(|g| g.path),
        Some(".houyicoder/skills/")
    );
}

/// An origin the vocabulary does not carry still names itself rather than
/// rendering as nothing, so a new discovery root shows up in both lists.
#[test]
fn test_unknown_origin_names_itself() {
    assert_eq!(origin_label("workspace"), "workspace");
    assert!(origin_group("workspace").is_none());
}

#[test]
fn test_display_order_origin_rank() {
    let entries = [
        row("claude_eco", "alpha"),
        row("user", "beta"),
        row("project", "gamma"),
        row("claude_eco", "delta"),
    ];
    let ordered: Vec<&str> = display_order(&entries)
        .iter()
        .map(|e| e.name.as_str())
        .collect();
    assert_eq!(
        ordered,
        vec!["beta", "gamma", "alpha", "delta"],
        "user and project precede ecosystem, names sorted inside a group"
    );
}

#[test]
fn test_display_order_unknown_last() {
    let entries = [
        row("workspace", "alpha"),
        row("agents", "beta"),
        row("mcp", "gamma"),
    ];
    let ordered: Vec<&str> = display_order(&entries)
        .iter()
        .map(|e| e.name.as_str())
        .collect();
    assert_eq!(
        ordered,
        vec!["beta", "gamma", "alpha"],
        "an origin outside the vocabulary sorts after every known one"
    );
}

/// Two origins outside the vocabulary keep their own groups. Both would carry
/// the same rank, and a renderer emits a heading whenever the origin changes,
/// so interleaving them prints one origin's heading twice.
#[test]
fn test_unknown_groups_stay_apart() {
    let entries = [
        row("workspace", "aa"),
        row("zeta", "bb"),
        row("workspace", "cc"),
    ];
    let ordered: Vec<&str> = display_order(&entries)
        .iter()
        .map(|e| e.name.as_str())
        .collect();
    assert_eq!(
        ordered,
        vec!["aa", "cc", "bb"],
        "each unknown origin stays contiguous, names sorted inside it"
    );
}

#[test]
fn test_heading_joins_label_path() {
    let style = Style::new();
    assert_eq!(
        joined(heading_spans("project", style, style)),
        "Native — .houyicoder/skills/"
    );
    assert_eq!(
        joined(heading_spans("workspace", style, style)),
        "workspace",
        "an origin with no scan path names only itself"
    );
}

/// The plain heading names the same group as the styled one, over the known
/// origins and over one the vocabulary does not carry, so a flattened view
/// cannot print a heading the rendered view does not.
#[test]
fn test_heading_text_matches_spans() {
    let style = Style::new();
    for origin in ["project", "mcp", "workspace"] {
        assert_eq!(
            heading_text(origin),
            joined(heading_spans(origin, style, style)),
            "origin {origin}"
        );
    }
}

/// A row's cost cell reports a skill the listing carries by what it spends,
/// and one it does not carry by the state that says so: a zero would read as
/// a measurement the window never took.
#[test]
fn test_cost_label_reports_state() {
    let mut announced = row("project", "commit");
    announced.tokens = 541;
    assert_eq!(cost_label(&announced), "541 tokens");
    let mut manual = row("project", "brainstorm");
    manual.announced = false;
    assert_eq!(cost_label(&manual), "manual");
}
