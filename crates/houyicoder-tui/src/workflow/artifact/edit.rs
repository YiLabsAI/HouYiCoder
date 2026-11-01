//! Artifact editing on App. Replace and insert build a pending change from
//! the typed text and the focused line; natural language attaches the text
//! as an annotation and asks the proposer for a change. Applying or rejecting
//! the pending change belongs to the artifact pane's approve and reject
//! actions, so this module stops at proposing.

use crate::artifact::{ArtifactMode, ChangeProposer};
use crate::state::App;

impl App {
    /// Submit the in-progress artifact edit on Enter. Dispatches by the
    /// session's edit mode: Replace and Insert build a pending proposal
    /// directly from the typed text and the focused line (no proposer);
    /// NaturalLanguage attaches the text as an annotation and asks the
    /// proposer (the LLM seam) to derive an edit. The mode returns to Normal
    /// after submit. Empty text is a no-op (the mode is preserved so the user
    /// can type and resubmit, or Esc to cancel).
    pub(crate) fn artifact_submit_edit(&mut self, text: String) {
        let line = self.workflow.artifact.focus() + 1;
        match self.workflow.artifact.mode() {
            ArtifactMode::Replace => {
                if text.trim().is_empty() {
                    self.ui.input.editor.set(text);
                    return;
                }
                self.workflow.artifact.set_pending_replace(text);
                self.workflow.artifact.cancel_mode();
                self.system_line(if self.workflow.artifact.pending_proposal().is_some() {
                    format!("artifact: replace proposed for line {line} (a=approve r=reject)")
                } else {
                    "artifact: empty document, nothing to replace".to_string()
                });
            }
            ArtifactMode::Insert => {
                if text.trim().is_empty() {
                    self.ui.input.editor.set(text);
                    return;
                }
                self.workflow.artifact.set_pending_insert(text);
                self.workflow.artifact.cancel_mode();
                self.system_line(if self.workflow.artifact.pending_proposal().is_some() {
                    format!("artifact: insert proposed below line {line} (a=approve r=reject)")
                } else {
                    "artifact: empty document, nothing to insert below".to_string()
                });
            }
            ArtifactMode::NaturalLanguage => {
                if text.trim().is_empty() {
                    self.ui.input.editor.set(text);
                    return;
                }
                // push_annotation mutates .artifact and ends; propose borrows
                // .proposer and .artifact immutably (disjoint fields); the
                // pending setter mutates .artifact.
                let Some(annotation) = self.workflow.artifact.push_annotation(text) else {
                    self.workflow.artifact.cancel_mode();
                    self.system_line("artifact: empty document, nothing to annotate");
                    return;
                };
                match self.workflow.proposer.propose(&self.workflow.artifact, &annotation) {
                    Ok(Some(proposal)) => {
                        self.workflow.artifact.set_pending(Some(proposal));
                        self.system_line(format!(
                            "artifact: NL proposal pending for line {line} (a=approve r=reject)"
                        ));
                    }
                    Ok(None) => self.system_line(
                        "natural-language proposer needs an LLM (not connected); use c/o/d for direct edits",
                    ),
                    Err(_) => self.system_line("artifact: proposer error (stub)"),
                }
                self.workflow.artifact.cancel_mode();
            }
            ArtifactMode::Normal => {}
        }
    }

    /// Propose deleting the focused line immediately (the d key). Builds the
    /// pending proposal on the session without entering an edit mode.
    pub(crate) fn artifact_propose_delete(&mut self) {
        let line = self.workflow.artifact.focus() + 1;
        self.workflow.artifact.set_pending_delete();
        self.system_line(if self.workflow.artifact.pending_proposal().is_some() {
            format!("artifact: delete proposed for line {line} (a=approve r=reject)")
        } else {
            "artifact: empty document, nothing to delete".to_string()
        });
    }
}
