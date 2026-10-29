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
//!
//! The stage is write-only: a call that declares itself read-only passes
//! through untouched, because a read cannot damage a protected path. A call
//! that does not declare itself read-only stays guarded.

use std::sync::Arc;

use houyicoder_api::sandbox::{Containment, Coverage, names_credential_path};

use crate::compound::{reads_only_protected, writes_fence_denied_path};
use crate::decision::{AskReason, AskSource, Decision, DenyReason, DenySource};
use crate::mode::ToolRequest;
use crate::pipeline::{GateCtx, Immunity, Stage, Validator};
use crate::rule::input_content;
use crate::safety::{fence_covers_all_hits, marker_hit, safety_check};
use crate::side_effect::is_shell_tool;
use std::fs::canonicalize;

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

    /// Whether a kernel fence refuses writes to the control files this stage
    /// protects. Only then is a fence-denied path certain to fail, so both
    /// answers that rest on the kernel -- the outright refusal and the
    /// exemption for a command that only names such a path -- are taken here.
    ///
    /// An active fence is necessary but not sufficient: a backend that grants
    /// writable roots without nested denies leaves a control file inside a
    /// granted root writable, and a verdict built on a refusal that never
    /// comes would either promise a block that does not happen or wave through
    /// a write that lands.
    fn fence_denies_control_writes(&self) -> bool {
        self.containment.as_ref().is_some_and(|containment| {
            matches!(containment.coverage(), Coverage::Fenced { .. })
                && containment.denies_control_writes()
        })
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
        let resolved = canonicalize(candidate.parent()?).ok()?;
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
    fn check(&self, req: &ToolRequest<'_>, ctx: &GateCtx<'_>) -> Option<Decision> {
        // The stage guards writes: a read-only call on a protected path
        // cannot damage the files the markers protect. A tool that does not
        // declare itself read-only stays guarded (an unknown read claim is
        // treated as a write, fail-closed).
        if req.is_read_only {
            return None;
        }
        let shell = is_shell_tool(req.tool_name);
        // The segments that name a protected path are proven read-only, so
        // this command cannot damage the files the markers protect, whatever
        // the mode. A credential is the exception: reading one leaks the
        // secret even though it damages nothing, so a read that names one
        // stays guarded.
        let reads = shell && reads_only_protected(ctx.content);
        if reads && !names_credential_path(ctx.content) {
            return None;
        }
        // A write a fence that closes control paths refuses is evidence on its
        // own. The text scan strips quoted heredoc bodies, so a script that
        // replaces a control file inside one leaves no marker behind for the
        // scan to see, while the write detection reads the body as the code
        // the interpreter runs.
        let fence_write =
            shell && self.fence_denies_control_writes() && writes_fence_denied_path(ctx.content);
        let hit = safety_check(req.tool_name, req.input).is_some()
            || self.resolved_path(req).is_some_and(|p| marker_hit(&p))
            || fence_write;
        if !hit {
            return None;
        }
        // A command that writes a control path can only end in a kernel
        // refusal while the fence holds. Asking would collect a yes the fence
        // then overrules, so the call is refused here with the reason instead.
        // Naming the path is not writing it: the same text appears in a commit
        // message or in a string redirected elsewhere, and those run fine, so
        // only a redirect target or a writing command's operand counts.
        if fence_write && !reads {
            return Some(Decision::Deny(DenyReason {
                source: DenySource::Fence,
                validator: self.name(),
                detail: "this command names a control file the sandbox fence \
                         protects, so it is refused rather than approved. Edit \
                         the file yourself, or use the app's own settings \
                         command to change it."
                    .into(),
            }));
        }
        // A fence that closes control paths covers every one the command
        // names, and the command writes none of them, so an approval adds
        // nothing: the kernel boundary holds with or without the answer. A
        // credential or startup file keeps its question, since reading one
        // leaks a secret without writing anything.
        if shell
            && !reads
            && self.fence_denies_control_writes()
            && !names_credential_path(ctx.content)
            && fence_covers_all_hits(ctx.content)
            && !writes_fence_denied_path(ctx.content)
        {
            return None;
        }
        Some(Decision::Ask(AskReason {
            source: AskSource::SystemSafety,
            validator: self.name(),
            detail: "accessing a protected path needs confirmation".into(),
            containment_note: None,
        }))
    }
}

#[cfg(test)]
mod tests;
