//! What an approval card says about the grants a yes would install.
//!
//! The card is the only place a user learns that a call reaches outside the
//! workspace, so the sentence has to name every directory the answer covers.

use houyicoder_api::sandbox::{BoundaryAccess, BoundaryGrant};
use houyicoder_api::skill::SkillScriptRef;
use houyicoder_protocol::frontend::permission::{AskReason, AskSource};

/// Add the effective directory capability to the approval reason, so a call
/// refused for another cause still says which outside directory a yes
/// authorizes. A path-bounds reason already names every grant, so it is left
/// as it is; otherwise the first directory is named and the rest counted.
pub(crate) fn disclose_directory_grants(reason: &mut Option<AskReason>, grants: &[BoundaryGrant]) {
    if grants.is_empty()
        || reason
            .as_ref()
            .is_some_and(|current| current.validator == "path-bounds")
    {
        return;
    }
    let current = reason.get_or_insert_with(|| AskReason {
        source: AskSource::Detection,
        validator: "directory-capability".to_string(),
        detail: "external path requires a sandbox directory capability".into(),
        containment_note: None,
    });
    let first = &grants[0];
    let capability = match first.access {
        BoundaryAccess::ReadOnly => "read-only directory",
        BoundaryAccess::ReadWrite => "parent directory with read-write access",
    };
    let remaining = match grants.len() {
        0 | 1 => String::new(),
        2 => " and 1 additional directory".to_string(),
        n => format!(" and {} additional directories", n - 1),
    };
    current.detail.push_str(&format!(
        ". Approving also grants {capability} {}{remaining}.",
        first.directory.display()
    ));
}

/// Format the approval-card detail for a Bash command that runs one or more
/// skill-directory scripts: the skill name + relative script path. No first
/// line is shown — it is attacker-controlled text the card would frame as an
/// authoritative summary. Multiple scripts name the first and count the rest.
pub(super) fn format_skill_script_detail(scripts: &[SkillScriptRef]) -> String {
    match scripts.len() {
        0 => String::new(),
        1 => {
            let s = &scripts[0];
            format!("runs skill script {}/{}", s.skill_name, s.script_rel_path)
        }
        _ => {
            let s = &scripts[0];
            format!(
                "runs {} skill scripts, first is {}/{}",
                scripts.len(),
                s.skill_name,
                s.script_rel_path
            )
        }
    }
}
