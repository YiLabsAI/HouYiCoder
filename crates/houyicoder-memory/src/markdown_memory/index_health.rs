//! Derived-index health for the markdown memory provider.
//!
//! A marker names a root whose index may not describe its topic set. A writer
//! creates it under the root lock before changing a topic and removes it only
//! after the index covering the new state is published, so a process that dies
//! in between leaves the next session a reason to rebuild. The marker is not
//! flushed to the device; the checks below repair a lost marker whenever the
//! topic set betrays the drift.

use std::collections::HashSet;
use std::fs;
use std::fs::OpenOptions;
use std::io::ErrorKind;
use std::path::Path;

#[cfg(test)]
use std::cell::Cell;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

use houyicoder_context::{MemoryError, MemorySource, render_memory_index_groups, tokens_for};

use super::MarkdownMemoryProvider;
use super::io::{
    DESCRIPTION_TOKEN_CAP, INDEX_FILE, INDEX_TRUNCATION_WARNING, cap_index_content,
    write_bytes_atomic,
};

/// Marker inside a root. The suffix keeps the topic scan, which lists
/// markdown files only, from reading it as a memory.
pub(super) const INDEX_DIRTY: &str = ".memory.index-dirty";

#[cfg(test)]
thread_local! {
    /// Number of marker creations that succeed before the next one fails.
    /// A test uses it to fail the repair's marker alone, leaving the marker
    /// the write itself raised.
    static FAIL_MARKER_AFTER: Cell<Option<usize>> = const { Cell::new(None) };
}

/// Make the marker creation that follows the given number of successful ones
/// fail.
#[cfg(test)]
pub(super) fn fail_marker_after(ok_calls: usize) {
    FAIL_MARKER_AFTER.with(|count| count.set(Some(ok_calls)));
}

impl MarkdownMemoryProvider {
    /// Render one root's index from its own topic files. The caller holds the
    /// in-process write lock and this root's lock, so the scan and the write
    /// see one topic set. Both the full rebuild and the write path call this,
    /// so the disk format has a single producer.
    pub(super) fn render_root_index(&self, root: &Path) -> Result<(), MemoryError> {
        let mut rows: Vec<(MemorySource, String, String)> = Vec::new();
        for topic in Self::scan_root(root) {
            // A key no row can address stays out of the index rather than
            // publishing a row that reads back as another key.
            if !Self::index_key_representable(&topic.key) {
                continue;
            }
            let Ok(frontmatter) = self.read_frontmatter(&topic.path) else {
                continue;
            };
            rows.push((
                frontmatter.source,
                topic.key.clone(),
                frontmatter.description,
            ));
        }
        let rendered = render_memory_index_groups(
            rows.iter()
                .map(|(source, key, description)| (*source, key.as_str(), description.as_str())),
        );
        let capped = cap_index_content(&rendered, self.max_lines);
        write_bytes_atomic(&Self::index_path_for(root), capped.as_bytes())
    }

    /// Record that the root's index no longer describes its topic set. The
    /// caller holds the root lock and changes the topic only after this
    /// returns, so a failure here leaves the store unchanged.
    pub(super) fn mark_index_dirty(root: &Path) -> Result<(), MemoryError> {
        #[cfg(test)]
        if let Some(remaining) = FAIL_MARKER_AFTER.with(Cell::get) {
            if remaining == 0 {
                FAIL_MARKER_AFTER.with(|count| count.set(None));
                return Err(MemoryError::Io);
            }
            FAIL_MARKER_AFTER.with(|count| count.set(Some(remaining - 1)));
        }
        let mut open = OpenOptions::new();
        open.create(true).write(true).truncate(false);
        #[cfg(unix)]
        open.mode(0o600);
        open.open(root.join(INDEX_DIRTY))
            .map(|_| ())
            .map_err(|error| {
                tracing::warn!(
                    "memory index marker under {} not created: {error}; the write stops",
                    root.display()
                );
                MemoryError::Io
            })
    }

    /// Clear the marker once an index covering the topic set is published.
    /// Only a real failure reaches diagnostics.
    pub(super) fn clear_index_dirty(root: &Path) {
        match fs::remove_file(root.join(INDEX_DIRTY)) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => tracing::warn!(
                "memory index marker under {} not cleared: {error}; the next start rebuilds",
                root.display()
            ),
        }
    }

    /// Whether the root carries the marker.
    pub(super) fn index_marked_dirty(root: &Path) -> bool {
        root.join(INDEX_DIRTY).is_file()
    }

    /// Whether the root's index must be rebuilt. A marker or a topic newer
    /// than the index says so without reading the index. Otherwise the
    /// published rows are read: one of them must not name a topic that is
    /// gone, and together they must name every topic the rebuild would list,
    /// because a complete index is written from exactly that set.
    pub(super) fn index_needs_rebuild(&self, root: &Path) -> bool {
        if Self::index_marked_dirty(root) || Self::root_is_stale(root) {
            return true;
        }
        let Ok(text) = fs::read_to_string(root.join(INDEX_FILE)) else {
            return false;
        };
        Self::index_has_dangling(root, &text)
            || Self::index_has_long_row(&text)
            || self.index_misses_topic(root, &text)
    }

    /// Whether any published row exceeds the description token cap, as in an
    /// index written before read-side description bounding existed. Checked
    /// ahead of the truncation-warning short-circuit so a pre-cap truncated
    /// index still repairs itself once.
    fn index_has_long_row(text: &str) -> bool {
        text.lines().filter_map(index_row_description).any(|desc| {
            desc.chars().any(char::is_control) || tokens_for(desc) > DESCRIPTION_TOKEN_CAP
        })
    }

    /// Whether a published row names a topic file that is not in this root.
    fn index_has_dangling(root: &Path, text: &str) -> bool {
        text.lines()
            .filter_map(index_row_key)
            .any(|key| !key.is_empty() && !root.join(format!("{key}.md")).is_file())
    }

    /// Whether a topic the rebuild would publish has no row. A complete index
    /// lists every addressable topic, so an unpublished topic means the
    /// index was written before it appeared, as when an external copy kept
    /// the topic's older timestamp. Three cases are excluded rather than
    /// repaired on every start: a capped index omits topics on purpose, a
    /// topic whose frontmatter does not parse never earns a row, and a
    /// legacy key embedding a complete delimiter cannot be addressed by any
    /// row.
    fn index_misses_topic(&self, root: &Path, text: &str) -> bool {
        if text.lines().any(|line| line == INDEX_TRUNCATION_WARNING) {
            return false;
        }
        let published: HashSet<&str> = text.lines().filter_map(index_row_key).collect();
        Self::scan_root(root)
            .iter()
            .filter(|topic| Self::index_key_representable(&topic.key))
            .filter(|topic| !published.contains(topic.key.as_str()))
            .any(|topic| self.read_frontmatter(&topic.path).is_ok())
    }

    /// Whether the index can read a row back as this whole key. A key that
    /// embeds the row delimiter reads back as the part before it, so the
    /// rebuild leaves such a key out instead of publishing a row that names
    /// another key and asks for another rebuild on every start.
    pub(super) fn index_key_representable(key: &str) -> bool {
        let probe = houyicoder_context::memory_index_row(key, "probe");
        index_row_key(&probe) == Some(key)
    }
}

/// The key a row names, or None for a line that is not a row. A row is a
/// dash, a space, the key, a colon, a space, and the description. A key may
/// itself hold spaces, so the delimiter is the first colon-space after the
/// dash; a key that contains that sequence cannot be represented and is kept
/// out by the caller's probe.
fn index_row_key(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("- ")?;
    let (key, _) = rest.split_once(": ")?;
    if key.is_empty() { None } else { Some(key) }
}

fn index_row_description(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("- ")?;
    let (key, desc) = rest.split_once(": ")?;
    if key.is_empty() { None } else { Some(desc) }
}
