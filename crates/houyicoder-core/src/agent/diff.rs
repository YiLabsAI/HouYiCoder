//! Unified-diff generation for the Edit tools, backed by the similar crate
//! (Myers diff). Pure (no I/O) so it is unit-testable without a sandbox. Used
//! by EditTool/MultiEditTool to return what changed, and later by the TUI diff
//! renderer to color green +/red - lines.
//!
//! similar natively emits multiple separated hunks (Myers, not one merged
//! block), the No-newline-at-end-of-file marker, and an empty string for
//! identical inputs. CRLF is normalized to LF before diffing (so CR does
//! not leak into the hunk bodies). No file header is emitted (callers want hunk bodies only).

use similar::Algorithm;

/// Line-based unified diff of two texts. Emits one or more hunks with context
/// unchanged lines around each changed region. Empty string when the texts are
/// identical. CRLF normalized to LF first. No file header (callers want hunk
/// bodies); the No-newline marker + multi-hunk split are native.
pub fn unified_diff(original: &str, modified: &str, context: usize) -> String {
    let a = original.replace("\r\n", "\n");
    let b = modified.replace("\r\n", "\n");
    // Fully-qualified to avoid the name collision with this fn itself.
    similar::udiff::unified_diff(Algorithm::Myers, &a, &b, context, None)
}

#[cfg(test)]
mod tests;
