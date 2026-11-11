//! The mode-default stage: the fallback verdict when nothing else in the
//! ladder fired. This is the only validator classified as mode-governed, so
//! it is the only place where a future containment relaxation may live; the
//! pipeline itself never knows relaxation exists. The verdict is the per-mode
//! policy's decision on the request's side-effect level, with one waiver in
//! Manual: a shell command the read-only proof accepts has already passed
//! every earlier stage, so the mode adds no question of its own.

use houyicoder_api::sandbox::names_credential_path;
use houyicoder_api::tool::is_shell_tool;

use crate::compound::is_readonly_compound;
use crate::decision::{AllowReason, Decision};
use crate::egress::http_origin;
use crate::gate::mode_policy::mode_default;
use crate::mode::{PermissionMode, ToolRequest};
use crate::pipeline::{GateCtx, Immunity, Stage, Validator};

/// The fallback verdict. Always returns a concrete Allow / Ask, so the ladder
/// never falls off the end. Reads the mode from the shared context.
pub struct ModeDefaultValidator;

/// The stable name of the mode-default validator. The only mode-governed
/// verdict, so the only one the fenced-exec relaxation may touch -- post_transform
/// recognizes it by this name and refuses to relax any other (immune) Ask.
pub(crate) const MODE_DEFAULT: &str = "mode_default";

impl Validator for ModeDefaultValidator {
    fn name(&self) -> &'static str {
        MODE_DEFAULT
    }
    fn stage(&self) -> Stage {
        Stage::ModeDefault
    }
    fn immunity(&self) -> Immunity {
        Immunity::ModeGoverned
    }
    fn consent_overridable(&self) -> bool {
        true
    }
    fn check(&self, req: &ToolRequest<'_>, ctx: &GateCtx<'_>) -> Option<Decision> {
        if ctx.mode == PermissionMode::Manual && readonly_shell(req, ctx) {
            return Some(Decision::Allow(AllowReason::ModeDefault));
        }
        if ctx.mode == PermissionMode::Auto
            && matches!(
                req.tool_name.to_ascii_lowercase().as_str(),
                "webfetch" | "netfetch" | "fetch"
            )
            && let Some(origin) = http_origin(ctx.content)
        {
            let curl_scope = format!("curl {origin}");
            if ctx
                .egress_allow
                .iter()
                .any(|allowed| allowed == &origin || allowed == &curl_scope)
            {
                return Some(Decision::Allow(AllowReason::ModeDefault));
            }
        }
        // Always fires: the mode default is the ladder's floor. Returning
        // None here would drop the call off the end of the ladder, which is a
        // programming error the pipeline guards against.
        Some(mode_default(ctx.mode, req))
    }
}

/// Whether a shell request proves read-only by the same rule the compound
/// stage applies: every segment is an attested read-only program with
/// attested arguments, and a leading move into one literal directory is
/// transparent. The credential check is the part the proof cannot supply:
/// reading a secret line is still the user's decision, so the mode keeps the
/// question instead of waiving it.
fn readonly_shell(req: &ToolRequest<'_>, ctx: &GateCtx<'_>) -> bool {
    if !is_shell_tool(req.tool_name) {
        return false;
    }
    let segments: Vec<&str> = ctx.segments.iter().map(String::as_str).collect();
    is_readonly_compound(&segments) && !names_credential_path(ctx.content)
}
