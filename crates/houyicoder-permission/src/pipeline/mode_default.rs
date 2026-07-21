//! The mode-default stage: the fallback verdict when nothing else in the
//! ladder fired. This is the only validator classified as mode-governed, so
//! it is the only place where a future containment relaxation may live; the
//! pipeline itself never knows relaxation exists. The verdict is a pure
//! function of the request and its side-effect level, dispatched to the
//! per-mode policy that already lives in the gate module.

use crate::decision::Decision;
use crate::gate::mode_default;
use crate::mode::ToolRequest;
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
        // Always fires: the mode default is the ladder's floor. Returning
        // None here would drop the call off the end of the ladder, which is a
        // programming error the pipeline guards against.
        Some(mode_default(ctx.mode, req))
    }
}
