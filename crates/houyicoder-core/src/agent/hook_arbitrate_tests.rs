//! Arbitration tests: deny-wins, ask-over-feedback, merge, composite
//! verdict (observations + triggers survive alongside blocking).

use super::registry::HookRegistry;
use super::*;

#[test]
fn test_arbitrate_deny_wins_over() {
    let results = vec![
        Ok(HookVerdict::Allow),
        Ok(HookVerdict::Deny("danger".into())),
        Ok(HookVerdict::Allow),
    ];
    match arbitrate(results).primary {
        HookVerdict::Deny(r) => assert_eq!(r, "danger"),
        other => panic!("expected Deny, got {other:?}"),
    }
}

#[test]
fn test_arbitrate_ask_over_feedback() {
    let results = vec![
        Ok(HookVerdict::Feedback("rewrite".into())),
        Ok(HookVerdict::Ask("confirm?".into())),
    ];
    match arbitrate(results).primary {
        HookVerdict::Ask(q) => assert_eq!(q, "confirm?"),
        other => panic!("expected Ask, got {other:?}"),
    }
}

#[test]
fn test_arbitrate_feedback_merges() {
    let results = vec![
        Ok(HookVerdict::Feedback("rule A".into())),
        Ok(HookVerdict::Feedback("rule B".into())),
    ];
    match arbitrate(results).primary {
        HookVerdict::Feedback(s) => assert!(s.contains("rule A") && s.contains("rule B")),
        other => panic!("expected Feedback, got {other:?}"),
    }
}

#[test]
fn test_arbitrate_inject_merges() {
    let results = vec![
        Ok(HookVerdict::Inject("ctx1".into())),
        Ok(HookVerdict::Inject("ctx2".into())),
    ];
    match arbitrate(results).primary {
        HookVerdict::Inject(s) => assert!(s.contains("ctx1") && s.contains("ctx2")),
        other => panic!("expected Inject, got {other:?}"),
    }
}

#[test]
fn test_arbitrate_trigger_when_sole() {
    let results = vec![Ok(HookVerdict::Trigger(HookEvent::PreCompact))];
    let av = arbitrate(results);
    match av.primary {
        HookVerdict::Trigger(HookEvent::PreCompact) => {}
        other => panic!("expected Trigger, got {other:?}"),
    }
    assert_eq!(av.triggers.len(), 1);
}

#[test]
fn test_arbitrate_observe_when_sole() {
    let results = vec![Ok(HookVerdict::Observe("noted".into()))];
    match arbitrate(results).primary {
        HookVerdict::Observe(s) => assert_eq!(s, "noted"),
        other => panic!("expected Observe, got {other:?}"),
    }
}

#[test]
fn test_arbitrate_default_allow() {
    let results = vec![Ok(HookVerdict::Allow), Ok(HookVerdict::Allow)];
    assert!(matches!(arbitrate(results).primary, HookVerdict::Allow));
}

#[test]
fn test_arbitrate_empty_allows() {
    assert!(matches!(arbitrate(vec![]).primary, HookVerdict::Allow));
}

#[test]
fn test_arbitrate_error_denies() {
    let results = vec![
        Ok(HookVerdict::Allow),
        Err(HookError::Timeout {
            hook_name: "h".into(),
            limit_ms: 5000,
        }),
    ];
    assert!(matches!(arbitrate(results).primary, HookVerdict::Deny(_)));
}

#[test]
fn test_arbitrate_deny_over_ask() {
    let results = vec![
        Ok(HookVerdict::Feedback("r".into())),
        Ok(HookVerdict::Ask("q".into())),
        Ok(HookVerdict::Deny("d".into())),
    ];
    assert!(matches!(arbitrate(results).primary, HookVerdict::Deny(_)));
}

// ========================================================================
// Composite verdict: observations + triggers survive alongside blocking.
// ========================================================================

#[test]
fn test_composite_observes_alongside_deny() {
    let results = vec![
        Ok(HookVerdict::Deny("blocked".into())),
        Ok(HookVerdict::Observe("hook A saw something".into())),
        Ok(HookVerdict::Observe("hook B noted too".into())),
        Ok(HookVerdict::Trigger(HookEvent::PreCompact)),
    ];
    let av = arbitrate(results);
    // Primary is Deny (blocking), but observations and triggers survive.
    match &av.primary {
        HookVerdict::Deny(r) => assert_eq!(r, "blocked"),
        other => panic!("expected Deny primary, got {other:?}"),
    }
    assert_eq!(
        av.observations.len(),
        2,
        "observations must not be dropped when deny is primary"
    );
    assert!(
        av.observations[0].contains("hook A") || av.observations[1].contains("hook A"),
        "first observation preserved"
    );
    assert!(
        av.observations[0].contains("hook B") || av.observations[1].contains("hook B"),
        "second observation preserved"
    );
    assert_eq!(
        av.triggers.len(),
        1,
        "triggers must not be dropped when deny is primary"
    );
    assert_eq!(av.triggers[0], HookEvent::PreCompact);
}

#[test]
fn test_multiple_triggers_returned() {
    let results = vec![
        Ok(HookVerdict::Trigger(HookEvent::PreCompact)),
        Ok(HookVerdict::Trigger(HookEvent::PostCompact)),
        Ok(HookVerdict::Trigger(HookEvent::FileChanged)),
    ];
    let av = arbitrate(results);
    assert_eq!(av.triggers.len(), 3, "all triggers must be returned");
    assert!(av.triggers.contains(&HookEvent::PreCompact));
    assert!(av.triggers.contains(&HookEvent::PostCompact));
    assert!(av.triggers.contains(&HookEvent::FileChanged));
    // Primary is the first trigger when triggers are the sole signal.
    match &av.primary {
        HookVerdict::Trigger(ev) => assert_eq!(*ev, HookEvent::PreCompact),
        other => panic!("expected Trigger primary, got {other:?}"),
    }
    assert!(av.observations.is_empty());
}

// ========================================================================
// Multi-hook registry dispatch + deny-wins via arbitrate.
// ========================================================================

#[test]
fn test_registry_deny_wins_arbitration() {
    let allow_hook = make_hook(vec![HookEvent::PreToolUse], HookVerdict::Allow);
    let deny_hook = make_hook(
        vec![HookEvent::PreToolUse],
        HookVerdict::Deny("forbidden".into()),
    );
    let reg = HookRegistry::new();
    reg.register(allow_hook);
    reg.register(deny_hook);
    assert_eq!(reg.len(), 2);

    let ctx = session_ctx(HookEvent::PreToolUse, HookPayload::Setup);
    let results: Vec<_> = reg.dispatch(&ctx).into_iter().map(|o| o.result).collect();
    assert_eq!(results.len(), 2);
    let av = arbitrate(results);
    match &av.primary {
        HookVerdict::Deny(r) => assert_eq!(r, "forbidden"),
        other => panic!("expected Deny, got {other:?}"),
    }
}

#[test]
fn test_registry_error_hook_denies() {
    let ok_hook = make_hook(vec![HookEvent::PreToolUse], HookVerdict::Allow);
    let err_hook = Arc::new(ErrorHook {
        name: "boom-hook".into(),
        events: vec![HookEvent::PreToolUse],
        error: HookError::GuestPanic {
            hook_name: "boom".into(),
            backtrace: String::new(),
        },
        source: HookSource::User,
    });
    let reg = HookRegistry::new();
    reg.register(ok_hook);
    reg.register(err_hook);
    let ctx = session_ctx(HookEvent::PreToolUse, HookPayload::Setup);
    let results: Vec<_> = reg.dispatch(&ctx).into_iter().map(|o| o.result).collect();
    assert_eq!(results.len(), 2);
    assert!(results[0].is_ok());
    assert!(results[1].is_err());
    assert!(matches!(arbitrate(results).primary, HookVerdict::Deny(_)));
}

// ========================================================================
// Edge cases: multiple Observe sole signal, Inject alongside Deny,
// Feedback alongside Deny.
// ========================================================================

#[test]
fn test_multiple_observe_joined() {
    let results = vec![
        Ok(HookVerdict::Observe("first note".into())),
        Ok(HookVerdict::Observe("second note".into())),
        Ok(HookVerdict::Observe("third note".into())),
    ];
    let av = arbitrate(results);
    // When Observe is the sole signal, observations are joined with "; "
    // into the primary verdict.
    match &av.primary {
        HookVerdict::Observe(s) => {
            assert!(s.contains("first note"));
            assert!(s.contains("second note"));
            assert!(s.contains("third note"));
            // Joining uses "; " separator.
            assert!(s.contains("; "));
        }
        other => panic!("expected Observe primary, got {other:?}"),
    }
    // Observations vec also carries all entries.
    assert_eq!(av.observations.len(), 3);
    assert!(av.triggers.is_empty());
}

#[test]
fn test_inject_alongside_deny_subsumed() {
    // When Deny is primary, Inject content is collected into the inject
    // Vec during arbitration but is NOT surfaced in the primary verdict.
    // The host sees Deny (blocking) and may inspect the inject Vec for
    // context. This test documents that behavior: Inject is subsumed by
    // Deny in the primary, and the inject content is not in the output
    // when Deny is primary.
    let results = vec![
        Ok(HookVerdict::Deny("blocked".into())),
        Ok(HookVerdict::Inject("extra context".into())),
    ];
    let av = arbitrate(results);
    // Primary is Deny; Inject is subsumed (not surfaced in primary).
    match &av.primary {
        HookVerdict::Deny(r) => assert_eq!(r, "blocked"),
        other => panic!("expected Deny primary, got {other:?}"),
    }
    // Observations and triggers are empty (Inject is not an observation).
    assert!(av.observations.is_empty());
    assert!(av.triggers.is_empty());
}

#[test]
fn test_feedback_alongside_deny_collected() {
    // When Deny is primary, Feedback signals are collected during
    // arbitration but are NOT surfaced in the primary verdict. The host
    // acts on Deny (terminal, no retry). Feedback would only matter if
    // Deny were absent (retryable quality loop). This test documents that
    // Feedback alongside Deny does not appear in the primary output.
    let results = vec![
        Ok(HookVerdict::Deny("security block".into())),
        Ok(HookVerdict::Feedback("rewrite suggestion".into())),
    ];
    let av = arbitrate(results);
    // Primary is Deny; Feedback is not surfaced (Deny is terminal).
    match &av.primary {
        HookVerdict::Deny(r) => assert_eq!(r, "security block"),
        other => panic!("expected Deny primary, got {other:?}"),
    }
    // Feedback content is not carried in observations or triggers.
    assert!(av.observations.is_empty());
    assert!(av.triggers.is_empty());
}
