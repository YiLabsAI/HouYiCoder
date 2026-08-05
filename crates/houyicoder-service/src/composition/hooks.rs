//! Hook registry wiring for the composition root.
//!
//! Split out of the composition module on size grounds, same reasoning as the
//! memory wiring beside it: whole concerns move out so the composition root
//! stays under the per-file gate. Nothing outside the composition root consumes
//! this, so the seam is local.

use super::*;

/// Build a hook registry from resolved specs. Each spec's event-name strings
/// parse against the runtime event enum at this composition site (the config
/// crate stays a serde-only leaf with no agent-layer dependency). A spec with
/// any unknown event is skipped with a stderr warning naming the spec and the
/// bad name, so a typo is visible without bricking the run. Returns None when
/// no spec yields a registered hook, so the caller skips the with_hooks step
/// entirely and the runner runs unchanged.
///
/// Source attribution: the env var a user sets in their own shell is a
/// user-level source, so hooks built here are tagged User (trusted). This
/// matters for the trust filter that lands later: a user-set env var cannot
/// travel with a cloned repository, so the clone-and-open threat model that
/// trust prompt defends against does not arise for this source. A future
/// project-level hook source read from a repository file MUST gate execution
/// on the trust prompt before any spawn; that is why the source tag is a
/// parameter here, not a hard-coded Project.
pub(super) fn build_hook_registry(
    specs: &[houyicoder_config::HookSpec],
    launcher: Arc<dyn ProcessLauncher>,
) -> Option<Arc<HookRegistry>> {
    if specs.is_empty() {
        return None;
    }
    let mut registry = HookRegistry::new();
    for spec in specs {
        let mut events = Vec::with_capacity(spec.events.len());
        let mut bad = None;
        for ev in &spec.events {
            match parse_event(ev) {
                Some(e) => events.push(e),
                None => {
                    bad = Some(ev.as_str());
                    break;
                }
            }
        }
        if let Some(name) = bad {
            tracing::warn!("hook {:?} ignored: unknown event {name:?}", spec.name);
            continue;
        }
        if events.is_empty() {
            continue;
        }
        let hook = CommandHook::new(
            spec.name.clone(),
            events,
            spec.program.clone(),
            spec.args.clone(),
            launcher.clone(),
            HookSource::User,
        );
        registry.register(Arc::new(hook));
    }
    if registry.is_empty() {
        None
    } else {
        Some(Arc::new(registry))
    }
}

#[cfg(test)]
mod hook_tests {
    use super::*;
    use houyicoder_config::HookSpec;

    fn launcher() -> Arc<dyn ProcessLauncher> {
        Arc::new(houyicoder_api::launcher::StdProcessLauncher::new())
    }

    fn spec(name: &str, events: &[&str], program: &str) -> HookSpec {
        HookSpec {
            name: name.into(),
            events: events.iter().map(|s| (*s).into()).collect(),
            program: program.into(),
            args: Vec::new(),
        }
    }

    #[test]
    fn test_build_hooks_empty() {
        assert!(build_hook_registry(&[], launcher()).is_none());
    }

    #[test]
    fn test_build_hooks_valid() {
        let specs = vec![spec("lint", &["PreToolUse"], "true")];
        let reg = build_hook_registry(&specs, launcher()).expect("one valid hook");
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn test_build_hooks_skips_bad() {
        // A spec with one known and one unknown event is skipped entirely;
        // a second valid spec still registers.
        let specs = vec![
            spec("bad", &["PreToolUse", "Nope"], "true"),
            spec("ok", &["PostToolUse"], "true"),
        ];
        let reg = build_hook_registry(&specs, launcher()).expect("second spec registers");
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn test_build_hooks_all_bad() {
        let specs = vec![spec("bad", &["DefinitelyNotAnEvent"], "true")];
        assert!(build_hook_registry(&specs, launcher()).is_none());
    }

    #[test]
    fn test_build_hooks_no_events() {
        // A spec with an empty events list registers nothing (a hook that
        // fires on no event is a no-op; the typo is surfaced at registration,
        // not at fire time).
        let specs = vec![spec("idle", &[], "true")];
        assert!(build_hook_registry(&specs, launcher()).is_none());
    }
}
