//! Direct coverage of append_hook_signals across every verdict arm. Split
//! from fire_tests.rs to keep that file under the file-size gate.

use std::sync::{Arc, Mutex};

use houyicoder_api::live::LiveEvent;
use houyicoder_context::{SessionId, TurnEvent, TurnEventKind};
use houyicoder_protocol::llm::{CompletionResponse, OutputItem, Usage};

use crate::agent::ToolRegistry;
use crate::agent::hook::{HookError, HookEvent, HookOutcome, HookVerdict};
use crate::agent::tests::runner_with;
use crate::provider::test_support::FakeProvider;

#[tokio::test]
#[expect(clippy::too_many_lines, reason = "long by design, kept whole")]
async fn test_append_signals_cover_verdicts() {
    // Drive append_hook_signals directly with synthetic outcomes so every
    // verdict arm (Allow/Trigger + the 5 string verdicts + Err) is covered
    // + the per-arm mapping (reason, triggered_event, error_kind) is pinned.
    // Allow is skipped (no HookSignal); the rest each land one signal.
    use houyicoder_context::{HookErrorKind, HookEventKind, HookVerdictKind};
    let runner = runner_with(
        Arc::new(FakeProvider::new(vec![CompletionResponse {
            output: vec![OutputItem::Text {
                text: "done".into(),
            }],
            usage: Usage::default(),
            model: "test".into(),
        }])),
        ToolRegistry::new(),
    );
    let session = SessionId::new();
    let outcomes = vec![
        HookOutcome {
            hook_name: "a".into(),
            result: Ok(HookVerdict::Allow),
        },
        HookOutcome {
            hook_name: "d".into(),
            result: Ok(HookVerdict::Deny("no".into())),
        },
        HookOutcome {
            hook_name: "f".into(),
            result: Ok(HookVerdict::Feedback("fix".into())),
        },
        HookOutcome {
            hook_name: "o".into(),
            result: Ok(HookVerdict::Observe("seen".into())),
        },
        HookOutcome {
            hook_name: "i".into(),
            result: Ok(HookVerdict::Inject("x".into())),
        },
        HookOutcome {
            hook_name: "k".into(),
            result: Ok(HookVerdict::Ask("q".into())),
        },
        HookOutcome {
            hook_name: "t".into(),
            result: Ok(HookVerdict::Trigger(HookEvent::PostCompact)),
        },
        HookOutcome {
            hook_name: "e".into(),
            result: Err(HookError::Timeout {
                hook_name: "e".into(),
                limit_ms: 5,
            }),
        },
    ];
    runner
        .append_hook_signals(
            session,
            HookEvent::PreToolUse,
            Some("recordable"),
            &outcomes,
        )
        .await;
    let snap = runner.store().trajectory_snapshot(session);
    let signals: Vec<&TurnEvent> = snap
        .iter()
        .filter(|ev| matches!(ev.kind, TurnEventKind::HookSignal { .. }))
        .collect();
    // 8 outcomes minus the Allow skip = 7 signals.
    assert_eq!(signals.len(), 7, "Allow is skipped, the rest land");
    let find = |name: &str| -> &TurnEvent {
        signals
            .iter()
            .find(|ev| matches!(&ev.kind, TurnEventKind::HookSignal { hook_name, .. } if hook_name == name))
            .copied()
            .expect("signal for hook")
    };
    // The Trigger arm carries triggered_event; the others do not.
    match &find("t").kind {
        TurnEventKind::HookSignal {
            verdict,
            triggered_event,
            ..
        } => {
            assert_eq!(*verdict, HookVerdictKind::Trigger);
            assert_eq!(*triggered_event, Some(HookEventKind::PostCompact));
        }
        _ => unreachable!(),
    }
    // The Err arm: effective verdict is fail-closed Deny (single source),
    // error_kind is Some(Timeout), and it is NOT a policy Deny.
    match &find("e").kind {
        TurnEventKind::HookSignal {
            verdict,
            error,
            hook_name,
            ..
        } => {
            assert_eq!(
                *verdict,
                HookVerdictKind::Deny,
                "fail-closed effective verdict"
            );
            assert_eq!(*error, Some(HookErrorKind::Timeout));
            assert_eq!(hook_name, "e");
        }
        _ => unreachable!(),
    }
    // The 5 string verdicts map to their kinds with reason carried.
    for (name, kind) in [
        ("d", HookVerdictKind::Deny),
        ("f", HookVerdictKind::Feedback),
        ("o", HookVerdictKind::Observe),
        ("i", HookVerdictKind::Inject),
        ("k", HookVerdictKind::Ask),
    ] {
        match &find(name).kind {
            TurnEventKind::HookSignal {
                verdict, reason, ..
            } => {
                assert_eq!(*verdict, kind, "verdict kind for hook {name}");
                assert!(!reason.is_empty(), "reason carried for hook {name}");
            }
            _ => unreachable!(),
        }
    }
}

/// An Observe verdict and a hook error both surface a system line through
/// the live sink so the user sees them, not just the durable trajectory.
/// The sink is the user-visible channel; the trajectory is the audit record.
/// Both fire for the same outcome — the user is told, and the run is
/// recorded. Allow and the string-only verdicts (Deny/Feedback/Inject/Ask)
/// do not fire a system line: Deny/Feedback/Inject/Ask are acted on by the
/// pipeline (the model sees the reason in its tool result), so a duplicate
/// system line would be noise.
#[tokio::test]
async fn test_observe_error_warn_user() {
    let captured = Arc::new(Mutex::new(Vec::<String>::new()));
    let cap = captured.clone();
    let sink: houyicoder_api::live::LiveSink = Arc::new(move |ev: &LiveEvent| {
        if let LiveEvent::SystemLine { text } = ev {
            cap.lock().unwrap().push(text.clone());
        }
    });
    let mut runner = runner_with(
        Arc::new(FakeProvider::new(vec![CompletionResponse {
            output: vec![OutputItem::Text {
                text: "done".into(),
            }],
            usage: Usage::default(),
            model: "test".into(),
        }])),
        ToolRegistry::new(),
    );
    runner.set_live_sink(sink);
    let session = SessionId::new();
    let outcomes = vec![
        HookOutcome {
            hook_name: "observer".into(),
            result: Ok(HookVerdict::Observe("something noted".into())),
        },
        HookOutcome {
            hook_name: "broken".into(),
            result: Err(HookError::Timeout {
                hook_name: "broken".into(),
                limit_ms: 100,
            }),
        },
        // Allow does not fire a system line.
        HookOutcome {
            hook_name: "silent".into(),
            result: Ok(HookVerdict::Allow),
        },
    ];
    runner
        .append_hook_signals(session, HookEvent::PreToolUse, Some("tool"), &outcomes)
        .await;
    let lines = captured.lock().unwrap().clone();
    assert_eq!(lines.len(), 2, "Observe + Err fire; Allow does not");
    assert!(
        lines
            .iter()
            .any(|l| l.contains("observer") && l.contains("something noted")),
        "Observe system line names the hook + note: {lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.contains("broken") && l.contains("failed")),
        "error system line names the hook + failure: {lines:?}"
    );
}
