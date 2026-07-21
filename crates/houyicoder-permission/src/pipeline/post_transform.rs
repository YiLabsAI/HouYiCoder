use crate::decision::{AllowReason, AskReason, Decision, DenyReason, DenySource};
use crate::mode::ToolRequest;
use crate::pipeline::detection::NETWORK_EGRESS;
use crate::pipeline::mode_default::MODE_DEFAULT;
use crate::side_effect::side_effect_for;
use houyicoder_api::sandbox::{Containment, SideEffect};

/// Apply the post-ladder transforms. The gate calls this on the raw ladder
/// output before returning, so the transform is structurally unbypassable: a
/// validator that returns early still flows through here.
///
/// Three jobs, in order:
/// 1. Attach a containment note to an Ask when the fence is expected to
///    reject the call even after consent. Informational only; the gate never
///    turns the note into a rejection. Independent of auto_allow_fenced_exec
///    so the user sees the fence's verdict whether or not the relaxation is on.
/// 2. When auto_allow_fenced_exec is on + the call is exec + the fence is
///    active, upgrade a mode-default Ask to Allow(Containment(FenceProof)).
///    Only the mode-default verdict qualifies -- it is the single
///    mode-governed verdict; every immune Ask (protected path, network
///    egress, destructive, git-checkpoint) and every user-rule Ask survives
///    the fence, because the fence is not an authority over those intents.
///    The fence is the authority over containment; the gate does not ask a
///    question the fence answers.
/// 3. When headless, turn an Ask into a Deny (no human to answer).
pub fn post_transform(
    raw: Decision,
    headless: bool,
    containment: Option<&dyn Containment>,
    auto_allow_fenced_exec: bool,
    req: &ToolRequest,
) -> Decision {
    // Let the fence speak to the user. would_block returns Some only when the
    // fence will reject the call's effect, so the note lands on exactly the
    // calls approval cannot help. For an egress ask the fence speaks at the
    // network layer (the call runs via bash, so its side-effect classification
    // is Exec, but the fence blocks egress, not exec); for any other ask the
    // call's own side effect is the right one to query. The pipeline validators
    // cannot see containment (GateCtx has no containment field), so this is the
    // single place the fence's verdict reaches the approval card. It never
    // becomes a denial; the refusal happens at execution time where the single
    // authority lives. The note is independent of auto_allow_fenced_exec so the
    // user sees the fence's verdict whether or not the relaxation is on.
    let raw = match (raw, containment) {
        (Decision::Ask(ask), Some(c)) => {
            let effect = if ask.validator == NETWORK_EGRESS {
                SideEffect::Network
            } else {
                side_effect_for(req.tool_name)
            };
            match c.would_block(effect) {
                Some(note) => Decision::Ask(AskReason {
                    containment_note: Some(note),
                    ..ask
                }),
                None => Decision::Ask(ask),
            }
        }
        (other, _) => other,
    };
    // Auto-allow fenced exec -- but only the mode-default verdict. That is the
    // single mode-governed verdict (the ladder's floor), so the only one the
    // fence may relax. Every immune Ask (protected path, egress, destructive,
    // git-checkpoint) and every user-rule Ask must survive: the fence is not an
    // authority over those intents, and silently dropping them would void the
    // safety layer's "never silent" promise. Only exec qualifies (file writes
    // bypass the fence today), and the proof token proves the fence covers the
    // call; without it the relaxation does not fire.
    //
    // Interim, default-off. The criterion this block rests on -- fence coverage
    // as the basis for silent auto-allow -- does not hold: fence coverage says
    // the action is in-bounds, not that it is recoverable, and the commands this
    // relaxes (mode-default, non-destructive-looking) are exactly the ones the
    // snapshot layer never photographs, so the proof certifies a set-containment
    // that is structurally constant-true while the action has no snapshot to
    // restore. Kept (not deleted) only because removing the AllowReason variant
    // would strand a wire field and its acceptance anchors; the default is off so
    // no production caller lights it. The replacement is a detection-layer
    // rewrite that argues target paths from the command text and queries real
    // snapshot coverage; until that lands the honest posture is to Ask.
    if auto_allow_fenced_exec && let Some(c) = containment {
        let se = side_effect_for(req.tool_name);
        if se == SideEffect::Exec
            && let Decision::Ask(ask) = &raw
            && ask.validator == MODE_DEFAULT
        {
            let coverage = c.coverage();
            if let Some(proof) = crate::decision::FenceProof::new(&coverage, se) {
                return Decision::Allow(AllowReason::Containment(proof));
            }
        }
    }
    if !headless {
        return raw;
    }
    match raw {
        Decision::Ask(_) => {
            tracing::warn!(
                "[permission] headless mode: denied a tool that required \
                 interactive approval"
            );
            Decision::Deny(DenyReason {
                source: DenySource::Headless,
                validator: "post_transform",
                detail: "headless mode: no interactive approval available".into(),
            })
        }
        other => other,
    }
}
