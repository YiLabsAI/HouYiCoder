//! Artifact closed-loop domain: an opened document plus a modal edit ->
//! proposed change -> review -> apply cycle.
//!
//! Direct edits (replace, insert, delete) build a ProposedChange on the
//! session without a proposer; natural-language mode is the seam where a real
//! LLM-backed ChangeProposer plugs in. This module is TUI-local: the artifact
//! loop is presentation-side orchestration today, so the aggregate + the
//! proposer trait live here alongside StubProposer (the only impl). The
//! ChangeProposer trait is the seam for a future LLM-backed proposer; when
//! that second impl lands in a non-TUI layer, lift the trait (and the typed
//! session payload it names) into a shared layer at that point rather than
//! speculatively now. File content is read via std::fs here; routing the
//! artifact load/propose/apply verbs over the wire (so the host owns the fs,
//! not the TUI) is a separate future slice.

use std::fs;
use std::{fmt, io};

use houyicoder_protocol::frontend::Verdict;
use std::error::Error;

/// Errors raised by the artifact domain. Hand-rolled (no thiserror dep) to
/// match the workspace per-crate error convention.
#[derive(Debug)]
pub enum TuiError {
    /// A file read or write failed.
    Io,
    /// A proposer feature is not implemented by this (stub) implementation.
    Unsupported,
}

impl fmt::Display for TuiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io => write!(f, "artifact io error"),
            Self::Unsupported => write!(f, "feature unsupported by this proposer"),
        }
    }
}

impl Error for TuiError {}

impl From<io::Error> for TuiError {
    fn from(_: io::Error) -> Self {
        Self::Io
    }
}

/// The edit mode of the artifact pane. Normal is the resting state: the input
/// box accepts slash commands and the line cursor is movable. Replace, Insert,
/// and NaturalLanguage are edit modes entered with c/o/i; Esc cancels back to
/// Normal. Delete (d) is immediate: it sets a pending proposal without an edit
/// mode. Direct edits bypass the proposer; NaturalLanguage is the seam where a
/// real LLM-backed ChangeProposer interprets the typed text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactMode {
    Normal,
    Replace,
    Insert,
    NaturalLanguage,
}

impl ArtifactMode {
    /// Whether this is the resting state (not an edit mode).
    pub fn is_normal(self) -> bool {
        self == Self::Normal
    }
}

/// One inline annotation attached to a line of an opened artifact. Authored
/// in natural-language mode: the user moves the line cursor, enters NL mode,
/// types, and Enter attaches the text to the focused line for the proposer to
/// interpret. Annotations double as the rationale for any proposed edit.
#[derive(Debug, Clone)]
pub struct Annotation {
    pub line: usize,
    pub text: String,
}

/// A proposed edit the agent derived from an annotation, awaiting human review.
/// verdict is Pending until the user approves or rejects. original and proposed
/// carry the before/after line content so the review pane renders a diff
/// without re-reading the document.
#[derive(Debug, Clone)]
pub struct ProposedChange {
    pub id: String,
    pub line_start: usize,
    pub original: Vec<String>,
    pub proposed: Vec<String>,
    pub rationale: String,
    pub verdict: Verdict,
}

/// An applied edit: append-only audit record. The applied log is never mutated,
/// only appended (immutable history for replay and audit). line_start is
/// replay-state-relative: it indexes the document as it stood after replaying
/// all earlier entries, so push-order replay stays consistent even when an
/// earlier change shifts line count.
#[derive(Debug, Clone)]
pub struct AppliedChange {
    pub id: String,
    pub line_start: usize,
    pub before: Vec<String>,
    pub after: Vec<String>,
    pub rationale: String,
}

/// An opened artifact and its review session: base lines loaded from disk,
/// annotations and the pending/applied edits layered on top, and the focused
/// line cursor plus the edit mode. Direct edits (replace/insert/delete) build
/// a ProposedChange on the session; the NL annotate -> propose -> store
/// orchestration lives on App so the session stays a pure data and logic type.
#[derive(Debug, Clone)]
pub struct ArtifactSession {
    path: String,
    /// Whether a file backs this session. A loaded document has one; the
    /// canned stub does not, so saving it back needs a path from the user.
    from_disk: bool,
    base_lines: Vec<String>,
    annotations: Vec<Annotation>,
    proposed: Option<ProposedChange>,
    applied: Vec<AppliedChange>,
    focus: usize,
    mode: ArtifactMode,
}

impl ArtifactSession {
    /// The path the document was loaded from.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// The focused line cursor (0-indexed).
    pub fn focus(&self) -> usize {
        self.focus
    }

    /// Total annotations across all lines.
    pub fn annotation_count(&self) -> usize {
        self.annotations.len()
    }

    /// Total edits applied (the append-only audit log length).
    pub fn applied_count(&self) -> usize {
        self.applied.len()
    }

    /// The most recently applied change, for audit visibility in the review pane.
    pub fn applied_last(&self) -> Option<&AppliedChange> {
        self.applied.last()
    }

    /// True when at least one annotation is attached to the given line.
    pub fn has_annotation(&self, line: usize) -> bool {
        self.annotations.iter().any(|a| a.line == line)
    }

    /// All annotations attached to the given line, in insertion order.
    pub fn annotations_for(&self, line: usize) -> Vec<&Annotation> {
        self.annotations.iter().filter(|a| a.line == line).collect()
    }

    /// The pending proposal awaiting review, if any. At most one is pending at
    /// a time: the user reviews it before the next can be produced.
    pub fn pending_proposal(&self) -> Option<&ProposedChange> {
        self.proposed.as_ref()
    }

    /// The document as it stands after every applied edit is replayed over the
    /// base lines. Replays in push order; each splice uses the change's own
    /// replay-relative line_start so insert or delete shifts stay consistent.
    pub fn current_lines(&self) -> Vec<String> {
        let mut lines = self.base_lines.clone();
        for change in &self.applied {
            let start = change.line_start.min(lines.len());
            let end = (start + change.before.len()).min(lines.len());
            lines.splice(start..end, change.after.iter().cloned());
        }
        lines
    }

    /// Per-line marks for the current replay state: true for lines that an
    /// applied change produced (the after content), so the view can flag them
    /// as reviewed/applied. Replays marks in lockstep with lines so insert and
    /// delete shifts stay consistent. Lines still at their base value are false.
    pub fn applied_marks(&self) -> Vec<bool> {
        let mut marks = vec![false; self.base_lines.len()];
        let mut lines: Vec<String> = self.base_lines.clone();
        for change in &self.applied {
            let start = change.line_start.min(lines.len());
            let end = (start + change.before.len()).min(lines.len());
            let new_marks: Vec<bool> = (0..change.after.len()).map(|_| true).collect();
            marks.splice(start..end, new_marks);
            lines.splice(start..end, change.after.iter().cloned());
        }
        marks
    }

    /// The live line count: the base count when no edits are applied (the fast
    /// path), otherwise the replayed count. Insert/delete applied edits change
    /// the count, so focus wrapping and paging must use this, not base count.
    pub fn line_count(&self) -> usize {
        if self.applied.is_empty() {
            self.base_lines.len()
        } else {
            self.current_lines().len()
        }
    }

    /// Move the line cursor up, wrapping around. No-op when the document is
    /// empty. Wraps over the live line count so insert/delete stays consistent.
    pub fn focus_up(&mut self) {
        let n = self.line_count();
        if n == 0 {
            return;
        }
        self.focus = (self.focus + n - 1) % n;
    }

    /// Move the line cursor down, wrapping around. No-op when empty.
    pub fn focus_down(&mut self) {
        let n = self.line_count();
        if n == 0 {
            return;
        }
        self.focus = (self.focus + 1) % n;
    }

    /// Move the line cursor up by a page, clamping at the first line. No-op
    /// when empty. Does not wrap (scroll-style clamping).
    pub fn focus_page_up(&mut self, page: usize) {
        let n = self.line_count();
        if n == 0 {
            return;
        }
        self.focus = self.focus.saturating_sub(page.max(1));
    }

    /// Move the line cursor down by a page, clamping at the last line.
    pub fn focus_page_down(&mut self, page: usize) {
        let n = self.line_count();
        if n == 0 {
            return;
        }
        self.focus = (self.focus + page.max(1)).min(n - 1);
    }

    /// The active edit mode (Normal resting; Replace/Insert/NaturalLanguage
    /// are edit modes entered with c/o/i).
    pub fn mode(&self) -> ArtifactMode {
        self.mode
    }

    /// Enter Replace edit mode: the next Enter proposes replacing the focused
    /// line with the typed text.
    pub fn enter_replace(&mut self) {
        self.mode = ArtifactMode::Replace;
    }

    /// Enter Insert edit mode: the next Enter proposes inserting the typed text
    /// as a new line below the focused line.
    pub fn enter_insert(&mut self) {
        self.mode = ArtifactMode::Insert;
    }

    /// Enter NaturalLanguage mode: the next Enter attaches the typed text to the
    /// focused line and hands it to the proposer (the LLM seam).
    pub fn enter_nl(&mut self) {
        self.mode = ArtifactMode::NaturalLanguage;
    }

    /// Cancel any edit mode and return to Normal. Called on Esc in an edit
    /// mode, or after submitting an edit.
    pub fn cancel_mode(&mut self) {
        self.mode = ArtifactMode::Normal;
    }

    /// Build and store a pending proposal to replace the focused line with the
    /// given text. No-op on an empty document. The proposal is reviewable via
    /// approve_pending/reject_pending.
    pub fn set_pending_replace(&mut self, text: String) {
        let lines = self.current_lines();
        if lines.is_empty() {
            return;
        }
        let focus = self.focus.min(lines.len() - 1);
        let id = format!("pc-{}", self.applied_count() + 1);
        self.proposed = Some(ProposedChange {
            id,
            line_start: focus,
            original: vec![lines[focus].clone()],
            proposed: vec![text],
            rationale: format!("replace line {}", focus + 1),
            verdict: Verdict::Pending,
        });
    }

    /// Build and store a pending proposal to insert the given text as a new
    /// line below the focused line. No-op on an empty document.
    pub fn set_pending_insert(&mut self, text: String) {
        let lines = self.current_lines();
        if lines.is_empty() {
            return;
        }
        let focus = self.focus.min(lines.len() - 1);
        let id = format!("pc-{}", self.applied_count() + 1);
        self.proposed = Some(ProposedChange {
            id,
            line_start: focus + 1,
            original: Vec::new(),
            proposed: vec![text],
            rationale: format!("insert below line {}", focus + 1),
            verdict: Verdict::Pending,
        });
    }

    /// Build and store a pending proposal to delete the focused line. No-op on
    /// an empty document.
    pub fn set_pending_delete(&mut self) {
        let lines = self.current_lines();
        if lines.is_empty() {
            return;
        }
        let focus = self.focus.min(lines.len() - 1);
        let id = format!("pc-{}", self.applied_count() + 1);
        self.proposed = Some(ProposedChange {
            id,
            line_start: focus,
            original: vec![lines[focus].clone()],
            proposed: Vec::new(),
            rationale: format!("delete line {}", focus + 1),
            verdict: Verdict::Pending,
        });
    }

    /// Append an annotation on the focused line. Returns the annotation by
    /// value so the caller can hand it to a proposer without holding a borrow
    /// into the session (keeping the propose call's field borrows disjoint).
    /// Returns None on empty text or an empty document (no-op).
    pub fn push_annotation(&mut self, text: String) -> Option<Annotation> {
        if self.base_lines.is_empty() || text.trim().is_empty() {
            return None;
        }
        let ann = Annotation {
            line: self.focus,
            text,
        };
        self.annotations.push(ann.clone());
        Some(ann)
    }

    /// Set the pending proposal (replaces any existing pending one).
    pub fn set_pending(&mut self, proposal: Option<ProposedChange>) {
        self.proposed = proposal;
    }

    /// Approve the pending proposal: move it to the applied log (append-only)
    /// and return the audit entry. Returns None when no proposal is pending.
    pub fn approve_pending(&mut self) -> Option<AppliedChange> {
        let proposal = self.proposed.take()?;
        let applied = AppliedChange {
            id: proposal.id,
            line_start: proposal.line_start,
            before: proposal.original,
            after: proposal.proposed,
            rationale: proposal.rationale,
        };
        self.applied.push(applied.clone());
        Some(applied)
    }

    /// Reject the pending proposal: drop it. The applied log is untouched.
    pub fn reject_pending(&mut self) {
        self.proposed = None;
    }

    /// Load a document from disk into a fresh session. The path is recorded;
    /// content is split on newlines. Returns TuiError::Io on a read failure.
    pub fn load(path: &str) -> Result<Self, TuiError> {
        let content = fs::read_to_string(path)?;
        let base_lines: Vec<String> = content.lines().map(String::from).collect();
        Ok(Self {
            path: path.to_string(),
            from_disk: true,
            base_lines,
            annotations: Vec::new(),
            proposed: None,
            applied: Vec::new(),
            focus: 0,
            mode: ArtifactMode::Normal,
        })
    }

    /// Persist the current (post-edit) document to disk, joined by newlines.
    /// Writes the replayed content, not the base. Returns TuiError::Io on a
    /// write failure. No auto-save: the user invokes this explicitly.
    pub fn save(&self, path: &str) -> Result<(), TuiError> {
        let content = self.current_lines().join("\n");
        fs::write(path, content).map_err(TuiError::from)
    }

    /// The path this session was loaded from, or None for the canned stub,
    /// which no file backs. A caller saving without a path needs one of these.
    pub fn file_path(&self) -> Option<&str> {
        self.from_disk.then_some(self.path.as_str())
    }

    /// Canned session for the stub App (no file IO). Used before the user
    /// opens a real document. Content matches the strategy draft so the pane
    /// opens on a real multi-section document on first launch. The path is a
    /// label, not a file: nothing on disk backs it, so it says so rather than
    /// naming a document that does not exist.
    pub fn stub() -> Self {
        let raw = "# houyicoder strategy
## 1. diagnosis
coding agents: TUI hands all to AI, IDE self-writes
both split write vs review into two surfaces
## 4.6 multimodal
agent-level vision + local-first pipeline
STT gate: AST-corrected dictionary before shipping voice
## 8. risk
provider absorbs single-feature leads in 6-24mo
lead comes from integrated composition, not any one feature";
        let base_lines: Vec<String> = raw.lines().map(String::from).collect();
        Self {
            path: "(canned stub)".to_string(),
            from_disk: false,
            base_lines,
            annotations: Vec::new(),
            proposed: None,
            applied: Vec::new(),
            focus: 0,
            mode: ArtifactMode::Normal,
        }
    }

    /// Build a session directly from in-memory lines (test helper for core's
    /// own tests). Note: cfg(test) is crate-local, so this is not visible to
    /// downstream crates' tests — TUI tests use stub() or load() instead.
    #[cfg(test)]
    pub fn from_lines(lines: &[&str]) -> Self {
        Self {
            path: "test".to_string(),
            from_disk: false,
            base_lines: lines.iter().map(|s| s.to_string()).collect(),
            annotations: Vec::new(),
            proposed: None,
            applied: Vec::new(),
            focus: 0,
            mode: ArtifactMode::Normal,
        }
    }
}

/// The seam between the artifact session and whatever turns a natural-language
/// annotation into a proposed edit. Object-safe so a real LLM-backed proposer
/// can be swapped in behind Box<dyn ChangeProposer> without touching the
/// session or the view. Sync now (the stub is deterministic); an async
/// proposer would adopt the workspace boxed-future alias pattern later.
pub trait ChangeProposer: Send + Sync {
    /// Derive a proposed edit for the given annotation, or Ok(None) when the
    /// annotation implies no edit the proposer can interpret.
    fn propose(
        &self,
        session: &ArtifactSession,
        annotation: &Annotation,
    ) -> Result<Option<ProposedChange>, TuiError>;
}

#[cfg(test)]
mod tests;
