//! agent::skill_listing — the per-turn skill-discovery attachment.
//!
//! The turn-entry step that lists model-invocable skills as a user-role
//! system-reminder attachment at the conversation tail (the same seam as
//! memory-recall), plus the budget-capped listing formatter. Progressive
//! disclosure: the attachment carries only descriptions; the full body
//! loads on demand when the model calls the Skill tool.
//!
//! The step scans the assembled context for a surviving listing event and skips
//! when one exists. Compaction gives a listing the Summarized disposition,
//! so a folded listing drops out of the view — the scan naturally empties
//! and the listing re-surfaces after a compaction with no provider-side
//! clear (the same natural-reset pattern memory-recall uses).

use houyicoder_api::skill::SkillDescriptor;
use houyicoder_context::{SessionEvent, SessionId};

use super::append::new_event;
use super::{RunError, Runner, model_window, selection};
use std::hash::DefaultHasher;
use std::hash::{Hash, Hasher};

/// Rough chars-per-token rate for converting a token budget to a char
/// budget. An estimate; the listing is truncated to fit, not billed.
/// The budget is 1% of the context window (the ecosystem convention:
/// discovery only), computed in char_budget as window x this / 100.
const CHARS_PER_TOKEN: u32 = 4;

/// Fallback char budget when the context window is unknown (1% of a
/// 200k-token window at 4 chars/token).
const DEFAULT_CHAR_BUDGET: usize = 8_000;

/// Per-entry hard cap on the combined description + when-to-use string.
/// The listing is for discovery; verbose entries waste turn-1 cache
/// tokens without improving the match rate.
const MAX_LISTING_DESC_CHARS: usize = 250;

/// Below this per-entry length, descriptions drop to names-only so a
/// tight budget still lists every skill name.
const MIN_DESC_LENGTH: usize = 20;

/// The char budget for the listing. 1% of the context window (tokens x
/// chars/token), or the default when the window is unknown.
fn char_budget(context_window_tokens: u32) -> usize {
    if context_window_tokens == 0 {
        return DEFAULT_CHAR_BUDGET;
    }
    // window x CHARS_PER_TOKEN x SKILL_BUDGET_CONTEXT_PERCENT == window / 25
    (context_window_tokens as usize * CHARS_PER_TOKEN as usize) / 100
}

/// The per-entry description: description plus when-to-use when present,
/// capped at MAX_LISTING_DESC_CHARS with a trailing ellipsis on overflow.
fn entry_description(desc: &str, when_to_use: Option<&str>) -> String {
    let combined = match when_to_use.filter(|w| !w.is_empty()) {
        Some(w) => format!("{desc} - {w}"),
        None => desc.to_string(),
    };
    let char_count = combined.chars().count();
    if char_count > MAX_LISTING_DESC_CHARS {
        let keep = MAX_LISTING_DESC_CHARS.saturating_sub(3);
        let truncated: String = combined.chars().take(keep).collect();
        format!("{truncated}...")
    } else {
        combined
    }
}

/// Format the model-invocable skill listing within a char budget. Three
/// tiers: full descriptions if they fit; descriptions truncated to a
/// uniform per-entry length if not; names-only when the budget is too
/// tight for even short descriptions. Empty when there are no skills.
///
/// The budget is a heuristic (byte length, not display width) — the
/// listing truncates rather than overflows, and skill descriptions are
/// overwhelmingly ASCII, so byte length is a safe proxy.
pub fn format_skill_listing(descriptors: &[SkillDescriptor], context_window_tokens: u32) -> String {
    if descriptors.is_empty() {
        return String::new();
    }
    let budget = char_budget(context_window_tokens);

    let full: Vec<String> = descriptors
        .iter()
        .map(|d| {
            let desc = entry_description(&d.description, d.when_to_use.as_deref());
            let cost = if d.body_token_estimate > 0 {
                format!(" (~{} tok)", d.body_token_estimate)
            } else {
                String::new()
            };
            format!("- {}: {}{}", d.name, desc, cost)
        })
        .collect();
    let full_total: usize =
        full.iter().map(|s| s.len()).sum::<usize>() + full.len().saturating_sub(1);

    if full_total <= budget {
        return full.join("\n");
    }

    // Names + "- " + ": " + the separator: per-entry overhead beyond the
    // description text. Compute what is left for descriptions.
    let names_overhead: usize = descriptors.iter().map(|d| d.name.len() + 4).sum::<usize>()
        + descriptors.len().saturating_sub(1);
    let available = budget.saturating_sub(names_overhead);
    let max_desc = available / descriptors.len();

    if max_desc < MIN_DESC_LENGTH {
        // Extreme case: drop descriptions, keep names so every skill is
        // at least discoverable.
        return descriptors
            .iter()
            .map(|d| format!("- {}", d.name))
            .collect::<Vec<_>>()
            .join("\n");
    }

    descriptors
        .iter()
        .map(|d| {
            let desc = entry_description(&d.description, d.when_to_use.as_deref());
            let desc = if desc.chars().count() > max_desc {
                let keep = max_desc.saturating_sub(3);
                let truncated: String = desc.chars().take(keep).collect();
                format!("{truncated}...")
            } else {
                desc
            };
            format!("- {}: {}", d.name, desc)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Fingerprint the descriptor set a listing announces. Two listings with
/// the same hash need no re-announce. The hash covers only the
/// listing-relevant fields (name, description, when-to-use, token
/// estimate), so a body-only edit that leaves the listing text unchanged
/// does not flip it. The SkillListing event field defaults to 0 in old
/// logs (pre-content_hash), which then re-inject once to upgrade.
fn listing_content_hash(descs: &[SkillDescriptor]) -> u64 {
    let mut h = DefaultHasher::new();
    for d in descs {
        d.name.hash(&mut h);
        d.description.hash(&mut h);
        d.when_to_use.hash(&mut h);
        d.body_token_estimate.hash(&mut h);
    }
    h.finish()
}

impl Runner {
    /// Append a skill-discovery listing attachment for this turn when no
    /// up-to-date listing survives in the assembled context. The listing carries
    /// descriptions only (progressive disclosure); the Skill tool loads
    /// full bodies on demand. No-op when no registry is wired, when the
    /// registry has no model-invocable skills, or when a surviving listing's
    /// content hash already matches the current set (the model has an
    /// up-to-date discovery view, so re-announcing is pure cache cost). A
    /// mismatch (the set changed since the surviving listing) or a folded
    /// listing re-injects full.
    pub(crate) async fn inject_skill_listing(&self, session: SessionId) -> Result<(), RunError> {
        let Some(registry) = &self.skill_registry else {
            return Ok(());
        };
        let view = self.store.current_view(session).await?;
        // Scan the assembled (manifest-applied) view, not the raw log: a
        // Summarized listing folded by compaction drops out of the view,
        // so the scan naturally empties and the listing re-surfaces.
        let filtered = match view.manifest.as_ref() {
            Some(m) => selection::apply_manifest(&view.events, m, Some(self.store.backend())),
            None => view.events.clone(),
        };
        let descriptors = registry.list_model_invocable();
        // Hide paths-gated skills until activated; unwrap_or(true) means
        // unwired = feature off = show all, not fail-closed.
        let descriptors: Vec<_> = descriptors
            .into_iter()
            .filter(|d| {
                if registry.paths_for(&d.name).is_empty() {
                    return true;
                }
                self.conditional
                    .as_ref()
                    .map(|a| a.is_active(&d.name))
                    .unwrap_or(true)
            })
            .collect();
        if descriptors.is_empty() {
            return Ok(());
        }
        // Hash over the filtered set so an activation flips it + re-announces.
        let chash = listing_content_hash(&descriptors);
        // Skip when the NEWEST surviving listing already reflects the
        // current set. An older non-matching listing is irrelevant (a
        // newer one supersedes it); a stale newest (set changed since) or
        // no survivor at all re-injects full.
        let already_current = filtered.iter().rev().find_map(|e| match &e.event {
            SessionEvent::SkillListing { content_hash, .. } => Some(*content_hash),
            _ => None,
        }) == Some(chash);
        if already_current {
            return Ok(());
        }
        let window = model_window::resolve_context_window(&self.config.model);
        let text = format_skill_listing(&descriptors, window);
        if text.is_empty() {
            return Ok(());
        }
        let bytes = text.len() as u32;
        self.store
            .append(new_event(
                session,
                SessionEvent::SkillListing {
                    text,
                    bytes,
                    content_hash: chash,
                },
            ))
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
