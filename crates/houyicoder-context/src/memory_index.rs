//! The one text format a memory index uses.
//!
//! The disk index and the prompt index describe the same records, so they
//! render through the same functions: a row is a key, a colon, and its
//! one-line description, and rows are grouped under one heading per type.
//! Keeping the format here means a reader that parses a row and a writer that
//! emits one cannot drift apart, because both depend on this crate.

use crate::memory_types::MemorySource;

/// The order type groups appear in an index. A stable order keeps the
/// rendered bytes identical between two runs over the same records, which is
/// what lets the prompt prefix stay cacheable.
pub const MEMORY_INDEX_GROUPS: [MemorySource; 4] = [
    MemorySource::Feedback,
    MemorySource::Project,
    MemorySource::User,
    MemorySource::Reference,
];

/// One index row: a key, a colon, and its description.
pub fn memory_index_row(key: &str, description: &str) -> String {
    format!("- {key}: {description}")
}

/// Render rows under one heading per type. Rows keep their input order within
/// a group, and a group with no rows emits no heading.
pub fn render_memory_index_groups<'a, I>(rows: I) -> String
where
    I: IntoIterator<Item = (MemorySource, &'a str, &'a str)>,
{
    let rows: Vec<(MemorySource, &str, &str)> = rows.into_iter().collect();
    let mut out = String::new();
    for source in MEMORY_INDEX_GROUPS {
        let group: Vec<&(MemorySource, &str, &str)> = rows
            .iter()
            .filter(|(row_source, _, _)| *row_source == source)
            .collect();
        if group.is_empty() {
            continue;
        }
        out.push_str("### ");
        out.push_str(source.as_label());
        out.push('\n');
        for (_, key, description) in group {
            out.push_str(&memory_index_row(key, description));
            out.push('\n');
        }
    }
    out
}

#[cfg(test)]
mod tests;
