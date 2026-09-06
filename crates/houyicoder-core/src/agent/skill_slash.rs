//! agent::skill_slash — the skill activation dispatch path.
//!
//! Resolves @skill:name input against the registry, prepares the body
//! (same path as the Skill tool), returns a SkillSlashOutcome the caller
//! appends as a SkillBody event so the model reads the body as a directive.

use houyicoder_api::skill::SkillError;
use houyicoder_context::SessionId;
use houyicoder_context::TurnEventKind;

use super::Runner;

/// Parse an @skill:-prefixed input into (skill name, optional args). Returns
/// None when the input lacks the prefix or the name is not valid
/// (^[a-z0-9-]+$), so non-skill input falls through to normal handling.
fn parse_skill_slash(text: &str) -> Option<(String, Option<&str>)> {
    let text = text.trim();
    let rest = text.strip_prefix("@skill:")?;
    let mut split = rest.splitn(2, char::is_whitespace);
    let name = split.next()?;
    if name.is_empty() {
        return None;
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return None;
    }
    let args = split.next().map(str::trim).filter(|s| !s.is_empty());
    Some((name.to_string(), args))
}

/// The outcome of resolving a user input as a skill slash.
#[derive(Debug)]
pub(crate) enum SkillSlashOutcome {
    /// Not an @skill: input (a path, an unknown token, or no registry wired).
    /// The run proceeds with the raw UserInput only.
    NotASkill,
    /// A user-invocable skill: append the prepared body as a durable
    /// SkillBody the model reads as a directive (and that survives a
    /// compaction boundary, unlike a MetaUser which compaction folds). The
    /// untrusted flag marks bodies from non-managed/user sources so the
    /// projection can frame them as data, not trusted instruction.
    Prepared {
        name: String,
        body: String,
        untrusted: bool,
    },
    /// A skill that is not user-invocable. Surface the notice to the user
    /// as a system line and end the turn without a model call — the model
    /// has nothing to do for a refused skill.
    Refused(String),
}

impl Runner {
    /// Resolve an @skill:-prefixed user input as a skill activation. The caller
    /// (run() entry) appends the raw @skill: text as UserInput first, then
    /// handles the outcome: Prepared appends a MetaUser body; Refused
    /// surfaces a system line + skips the model call; NotASkill falls
    /// through to the normal run.
    ///
    /// Gated on user-invocable, not disable-model-invocation: a skill
    /// hidden from the model is reachable via @skill: activation when user-invocable is
    /// true. The body preparation shares the same pure function the Skill
    /// tool uses (the two invocation paths converge there).
    pub(crate) async fn resolve_skill_slash(
        &self,
        session: SessionId,
        text: &str,
    ) -> SkillSlashOutcome {
        let Some(registry) = self.skill_registry.as_ref() else {
            return SkillSlashOutcome::NotASkill;
        };
        let Some((name, args)) = parse_skill_slash(text) else {
            return SkillSlashOutcome::NotASkill;
        };
        let sid = session.to_string();
        // Gate on user-invocable (the @skill: activation path): find returns the
        // descriptor + its flag; the shared prepare_body is ungated so a
        // model-disabled but user-invocable skill is reachable here.
        let desc = match registry.find(&name) {
            None => return SkillSlashOutcome::NotASkill,
            Some(d) => d,
        };
        if !desc.user_invocable {
            registry.record_invocation(&name, true);
            return SkillSlashOutcome::Refused(format!(
                "The skill \"{name}\" cannot be invoked directly by the user. \
                 Ask the assistant to use the {name} skill for you."
            ));
        }
        // Gate on paths: a conditional skill refuses until a file touch
        // activates it. unwrap_or(true) means unwired = feature off = pass.
        let paths = registry.paths_for(&name);
        if !paths.is_empty()
            && !self
                .conditional
                .as_ref()
                .map(|a| a.is_active(&name))
                .unwrap_or(true)
        {
            registry.record_invocation(&name, true);
            return SkillSlashOutcome::Refused(format!(
                "skill {name} is conditional; touch a matching file to activate: {}",
                paths.join(", ")
            ));
        }
        // Determine trust from a single origin lookup: body trust and
        // entitlement trust both derive from the same origin string, so
        // one scan suffices. Fails closed (untrusted) when the skill is
        // absent from the origin snapshot.
        let origin = super::skill_body::skill_origin(&**registry, &name);
        let untrusted = origin
            .as_deref()
            .map(|o| !super::skill_body::is_trusted_origin(o))
            .unwrap_or(true);
        match registry.prepare_body(&name, args, Some(&sid)) {
            Ok(body) => {
                registry.record_invocation(&name, false);
                // Resolve entitlements from frontmatter + profile + grant
                // store (same as the Skill tool path). A non-managed/user
                // source is not trusted for entitlements — frontmatter and
                // the compiled profile are skipped. The trust set converged
                // to body trust, so the untrusted flag above feeds both.
                let origin_str = origin.as_deref().unwrap_or("unknown");
                if let Some(session) = self.sandbox_session.as_ref() {
                    let (mach, allow_launch) = houyicoder_api::skill::grant::resolve_entitlements(
                        self.skill_grants.as_deref(),
                        &name,
                        origin_str,
                        &desc.allowed_mach_services,
                        desc.allow_app_launch,
                        !untrusted,
                    );
                    session.clear_skill_grants();
                    session.set_extra_mach_services(&mach);
                    if allow_launch {
                        session.grant_app_launch();
                    }
                }
                self.set_active_skill(&name);
                // Inject session context into the body (dynamic template
                // placeholders {{userMessages}} + {{sessionMemory}} +
                // {{userDescription}}). Only fires when the body contains
                // at least one placeholder, so non-skillify skills are
                // unaffected.
                let body = if body.contains("{{userMessages}}")
                    || body.contains("{{sessionMemory}}")
                    || body.contains("{{userDescription}}")
                {
                    self.inject_session_context(session, body, args).await
                } else {
                    body
                };
                // Register the skill's frontmatter hooks (invoke-time,
                // session-scoped). The registrar dedups across both
                // invocation paths so a @skill: dispatch followed by a Skill-tool
                // call for the same skill does not stack a second firing copy.
                if let Some(r) = self.registrar.as_ref() {
                    r.register(&**registry, &name);
                }
                SkillSlashOutcome::Prepared {
                    name,
                    body,
                    untrusted,
                }
            }
            Err(SkillError::NotFound(_)) => SkillSlashOutcome::NotASkill,
            Err(other) => {
                registry.record_invocation(&name, true);
                SkillSlashOutcome::Refused(format!("skill invocation failed: {other}"))
            }
        }
    }

    /// Replace {{userMessages}}, {{sessionMemory}}, and {{userDescription}}
    /// placeholders in a skill body with session context. {{userMessages}} is
    /// replaced with the joined text of all UserInput events in the current
    /// session view (full replay, including pre-compact events — intentional,
    /// gives the model the complete user history, not just post-compact);
    /// {{sessionMemory}} is a placeholder string (integration is a
    /// follow-up); {{userDescription}} is replaced with the args the user
    /// passed on the @skill: invocation (or "None provided" if absent).
    async fn inject_session_context(
        &self,
        session: SessionId,
        mut body: String,
        args: Option<&str>,
    ) -> String {
        if body.contains("{{userMessages}}") {
            let messages: Vec<String> = match self.store.current_view(session).await {
                Ok(view) => view
                    .events
                    .iter()
                    .filter_map(|e| match &e.kind {
                        TurnEventKind::UserInput { text } => Some(text.clone()),
                        _ => None,
                    })
                    .collect(),
                Err(_) => Vec::new(),
            };
            let joined = if messages.is_empty() {
                "No user messages in this session.".to_string()
            } else {
                messages.join("\n\n---\n\n")
            };
            body = body.replace("{{userMessages}}", &joined);
        }
        if body.contains("{{sessionMemory}}") {
            body = body.replace("{{sessionMemory}}", "No session memory available.");
        }
        if body.contains("{{userDescription}}") {
            let desc = args.unwrap_or("None provided");
            body = body.replace("{{userDescription}}", desc);
        }
        body
    }
}

#[cfg(test)]
#[path = "slash_tests.rs"]
mod tests;
