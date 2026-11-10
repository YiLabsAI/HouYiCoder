//! Scope-root resolution for the markdown memory provider. Roots are fixed
//! at construction; this module maps a scope to the root that holds it and
//! resolves two spellings of one directory to a single identity.

use houyicoder_context::MemoryScope;
use std::fs::canonicalize;
use std::path::Path;
use std::path::PathBuf;

/// The identity two paths share when they name the same directory. A root
/// that does not exist yet cannot be resolved and keeps its literal path;
/// the first write creates it under that name.
pub(super) fn canonical_identity(root: &Path) -> PathBuf {
    canonicalize(root).unwrap_or_else(|_| root.to_path_buf())
}

impl super::MarkdownMemoryProvider {
    /// Resolve a storage root by scope. User -> roots[0], Project -> roots[1]
    /// (when multi-root), Auto -> the last root. Falls back to the write
    /// (last) root when the requested scope has no dedicated root, so a
    /// single-root provider still lands writes (the scope is advisory, not a
    /// hard gate).
    pub(super) fn root_for_scope(&self, scope: MemoryScope) -> &Path {
        match scope {
            MemoryScope::User => self
                .roots
                .first()
                .map(PathBuf::as_path)
                .unwrap_or(self.write_root()),
            MemoryScope::Project => self
                .roots
                .get(1)
                .map(PathBuf::as_path)
                .unwrap_or(self.write_root()),
            MemoryScope::Auto => self.write_root(),
        }
    }
}
