//! Scope-root resolution for the markdown memory provider. Split out of the
//! main module to keep it under the file-size gate. Inherent impl (not the
//! trait impl) so it can live in a submodule; the trait impl resolves
//! self.root_for_scope across impl blocks. The method is pub(super) so the
//! parent module's trait impl can call it (a private fn in a child module is
//! not visible to the parent).

use std::path::Path;

use houyicoder_context::MemoryScope;

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
                .map(std::path::PathBuf::as_path)
                .unwrap_or(self.write_root()),
            MemoryScope::Project => self
                .roots
                .get(1)
                .map(std::path::PathBuf::as_path)
                .unwrap_or(self.write_root()),
            MemoryScope::Auto => self.write_root(),
        }
    }
}
