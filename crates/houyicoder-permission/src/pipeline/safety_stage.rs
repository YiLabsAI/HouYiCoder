//! The system-safety stage: protected paths the agent must never write to
//! silently. Bypass-immune: the verdict survives every mode and is not
//! overridable by a stored consent, because the protected paths are a hard
//! floor regardless of what the user pre-approved.
//!
//! The stage judges twice: once on the path the caller supplied, and once on
//! the path that spelling resolves to. The second pass exists because the
//! markers are matched as substrings, so a name that reaches a protected
//! directory without containing its marker -- a symlink inside the workspace
//! -- passes the first pass while landing inside the directory anyway. For a
//! shell command the kernel fence refuses that write on its own, since it
//! matches the resolved path. The file tools have no such backstop: they
//! write from the host process, which the fence does not cover, so this stage
//! is the only thing standing between the agent and the file.

use std::sync::Arc;

use houyicoder_api::sandbox::Containment;

use crate::decision::{AskReason, AskSource, Decision};
use crate::mode::ToolRequest;
use crate::pipeline::{GateCtx, Immunity, Stage, Validator};
use crate::rule::input_content;
use crate::safety::{marker_hit, safety_check};

/// Tools whose input names a single file the host process writes directly.
/// These bypass the kernel fence, so the resolved-path pass is what protects
/// them. Shell tools are absent on purpose: their content is a command line,
/// not a path, and the fence already judges what they touch.
const HOST_WRITE_TOOLS: &[&str] = &["write", "edit", "multiedit", "patch", "str_replace"];

/// Protected paths: the version-control directory, the agent's own config
/// directory, and shell rc files. Always asks; never silently allowed; never
/// waived by a prior consent.
///
/// Holds the fence handle directly rather than reading it from the shared
/// context, matching the path-bounds stage: the context stays free of a
/// containment field by design. The handle supplies the workspace root a
/// relative path resolves against; without it the stage falls back to judging
/// the supplied string alone.
pub struct ProtectedPathValidator {
    containment: Option<Arc<dyn Containment>>,
}

impl ProtectedPathValidator {
    pub fn new(containment: Option<Arc<dyn Containment>>) -> Self {
        Self { containment }
    }

    /// The file a host-process write would land on, as a string to judge.
    /// Returns None when the tool does not name a single path, when no fence
    /// supplies a root, or when the name cannot be resolved -- each case
    /// leaves the supplied-string verdict as the only one, which is the
    /// behaviour that held before this pass existed.
    ///
    /// Only the parent is resolved: the file itself is usually absent, since
    /// the common case is creating it. That matches how the sandbox resolves
    /// a write target, so the two layers agree on which file is named. The
    /// string is lossy, which costs nothing here -- a byte sequence that is
    /// not valid text cannot spell a marker either.
    fn resolved_path(&self, req: &ToolRequest<'_>) -> Option<String> {
        let tool = req.tool_name.to_ascii_lowercase();
        if !HOST_WRITE_TOOLS.contains(&tool.as_str()) {
            return None;
        }
        let supplied = input_content(req.tool_name, req.input);
        if supplied.is_empty() {
            return None;
        }
        let root = self.containment.as_ref()?.boundary_root()?;
        let candidate = root.join(&supplied);
        let resolved = std::fs::canonicalize(candidate.parent()?).ok()?;
        let named = resolved.join(candidate.file_name()?);
        Some(named.to_string_lossy().into_owned())
    }
}

impl Validator for ProtectedPathValidator {
    fn name(&self) -> &'static str {
        "protected_path"
    }
    fn stage(&self) -> Stage {
        Stage::SystemSafety
    }
    fn immunity(&self) -> Immunity {
        Immunity::ModeImmune
    }
    fn consent_overridable(&self) -> bool {
        false
    }
    fn check(&self, req: &ToolRequest<'_>, _ctx: &GateCtx<'_>) -> Option<Decision> {
        let hit = safety_check(req.tool_name, req.input).is_some()
            || self.resolved_path(req).is_some_and(|p| marker_hit(&p));
        if !hit {
            return None;
        }
        Some(Decision::Ask(AskReason {
            source: AskSource::SystemSafety,
            validator: self.name(),
            detail: "writing to a protected path needs confirmation".into(),
            containment_note: None,
        }))
    }
}

#[cfg(test)]
#[path = "safety_stage_tests.rs"]
mod tests;
