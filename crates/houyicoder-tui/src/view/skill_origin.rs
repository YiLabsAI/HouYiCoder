//! The discovery-origin vocabulary the /skills pane and the /context
//! breakdown share: one label and scan path per origin, one order, and the
//! cost cell of a row. Both views group, sort and fill that cell through these, so a
//! skill reads under the same heading, in the same position, at the same
//! cost, whichever view shows it.
//!
//! The keys are what the discovery registry reports; a key this vocabulary
//! does not know renders as itself and sorts after the known ones, so a new
//! origin shows up rather than disappearing from a list.

use houyicoder_protocol::frontend::skills::SkillEntry;
use ratatui::style::Style;
use ratatui::text::Span;

use crate::command::render::format_tokens;
use crate::transcript::records::ContextSkillEntry;

/// Separator between an origin label and its scan path in a heading.
const HEADING_SEPARATOR: &str = " — ";

/// One discovery origin: the key the registry reports, the label both views
/// print, and the directory the origin scans.
struct OriginGroup {
    key: &'static str,
    label: &'static str,
    path: &'static str,
}

/// Origins in fixed precedence order, managed first. Rendering walks this
/// order, so the group order is stable across snapshots. Every key the skill
/// registry can report for a discovered skill has an entry: a key missing
/// here would still render, but without the path that says where the skill
/// sits on disk.
const ORIGIN_ORDER: &[OriginGroup] = &[
    OriginGroup {
        key: "managed",
        label: "Managed",
        path: "/etc/houyicoder/skills/",
    },
    OriginGroup {
        key: "user",
        label: "Native",
        path: "~/.houyicoder/skills/",
    },
    OriginGroup {
        key: "project",
        label: "Native",
        path: ".houyicoder/skills/",
    },
    OriginGroup {
        key: "claude_eco",
        label: "Ecosystem",
        path: ".claude/skills/",
    },
    OriginGroup {
        key: "agents",
        label: "Spec",
        path: ".agents/skills/",
    },
    OriginGroup {
        key: "mcp",
        label: "MCP",
        path: "(mcp server)",
    },
];

/// The label and scan path for one origin key, when it is known.
fn origin_group(origin: &str) -> Option<&'static OriginGroup> {
    ORIGIN_ORDER.iter().find(|g| g.key == origin)
}

/// The label for one origin key, falling back to the key itself so an origin
/// outside this vocabulary still names itself.
fn origin_label(origin: &str) -> &str {
    origin_group(origin).map(|g| g.label).unwrap_or(origin)
}

/// The heading of one origin group: the label, then the scan path when the
/// vocabulary knows one. Each view passes its own styles, so the group text
/// and the separator between label and path come from this one place and a
/// group cannot read differently in the two views.
pub(crate) fn heading_spans(
    origin: &str,
    label_style: Style,
    path_style: Style,
) -> Vec<Span<'static>> {
    let mut heading = vec![Span::styled(origin_label(origin).to_string(), label_style)];
    if let Some(group) = origin_group(origin) {
        heading.push(Span::raw(HEADING_SEPARATOR));
        heading.push(Span::styled(group.path.to_string(), path_style));
    }
    heading
}

/// The same heading as plain text, for a view that flattens the block instead
/// of styling it. Built from the label, separator and scan path the styled
/// heading uses, so neither can name a group the other does not.
pub(crate) fn heading_text(origin: &str) -> String {
    match origin_group(origin) {
        Some(group) => format!("{}{}{}", group.label, HEADING_SEPARATOR, group.path),
        None => origin_label(origin).to_string(),
    }
}

/// The cost cell of one skill row: the tokens the listing spends on a skill it
/// carries, or the state that says it carries none of it. A skill the listing
/// does not carry is the user's to invoke by hand, so it reports that state
/// rather than a zero the window never paid.
pub(crate) fn cost_label(row: &ContextSkillEntry) -> String {
    if row.announced {
        format!("{} tokens", format_tokens(u64::from(row.tokens)))
    } else {
        "manual".to_string()
    }
}

/// A skill row of either view, reduced to the two fields the shared order
/// reads. The protocol types carry the same facts under different field
/// names, so each view implements this rather than mapping its rows into an
/// intermediate list.
pub(crate) trait OriginRow {
    fn origin(&self) -> &str;
    fn name(&self) -> &str;
}

impl OriginRow for SkillEntry {
    fn origin(&self) -> &str {
        &self.origin
    }

    fn name(&self) -> &str {
        &self.name
    }
}

impl OriginRow for ContextSkillEntry {
    fn origin(&self) -> &str {
        &self.source
    }

    fn name(&self) -> &str {
        &self.name
    }
}

/// The display order of skill rows: grouped by origin in ORIGIN_ORDER,
/// sorted by name inside a group, an origin outside the vocabulary last and
/// name-sorted among its own. Rendering and cursor movement both resolve
/// their list through this, so the highlighted row and the acted-on row are
/// the same one.
pub(crate) fn display_order<T: OriginRow>(entries: &[T]) -> Vec<&T> {
    let mut out: Vec<&T> = entries.iter().collect();
    out.sort_by(|a, b| {
        origin_rank(a.origin())
            .cmp(&origin_rank(b.origin()))
            // Origins outside the vocabulary share the last rank, so the key
            // breaks the tie: a heading is emitted whenever the origin
            // changes, and interleaving two unknowns would print a heading
            // twice for one origin.
            .then_with(|| a.origin().cmp(b.origin()))
            .then_with(|| a.name().cmp(b.name()))
    });
    out
}

/// The position of one origin in ORIGIN_ORDER; an origin outside the
/// vocabulary sorts after every known one.
fn origin_rank(origin: &str) -> usize {
    ORIGIN_ORDER
        .iter()
        .position(|g| g.key == origin)
        .unwrap_or(ORIGIN_ORDER.len())
}

#[cfg(test)]
mod tests;
