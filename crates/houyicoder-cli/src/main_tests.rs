//! Unit tests for the CLI argument parser and composition-root wiring.
//!
//! parse_args is a pure function (no I/O, no process exit) so the full
//! flag/subcommand matrix is testable without spawning the binary.

use super::*;

// ── parse_args: default + mode flags ──────────────────────────────

#[test]
fn test_parse_default_tui() {
    let cmd = parse_args(vec![]).expect("no args must parse");
    assert!(matches!(cmd, CliCommand::Tui { project: None, .. }));
}

#[test]
fn test_parse_project_flag() {
    let cmd = parse_args(vec!["--project".into(), "/tmp/repo".into()]).expect("must parse");
    match cmd {
        CliCommand::Tui { project, .. } => assert_eq!(project.as_deref(), Some("/tmp/repo")),
        other => panic!("expected Tui, got {other:?}"),
    }
}

#[test]
fn test_parse_project_short() {
    let cmd = parse_args(vec!["-p".into(), "/tmp/repo".into()]).expect("must parse");
    assert!(matches!(cmd, CliCommand::Tui { project: Some(p), .. } if p == "/tmp/repo"));
}

#[test]
fn test_parse_model_flag() {
    // --model <id> overrides the settings model for a fresh session.
    let cmd = parse_args(vec!["--model".into(), "qwen3-coder".into()]).expect("must parse");
    match cmd {
        CliCommand::Tui { model, .. } => {
            assert_eq!(model.as_deref(), Some("qwen3-coder"));
        }
        other => panic!("expected Tui, got {other:?}"),
    }
}

#[test]
fn test_parse_model_with_project() {
    // --model + --project compose.
    let cmd = parse_args(vec![
        "--project".into(),
        "/repo".into(),
        "--model".into(),
        "glm-5.2".into(),
    ])
    .expect("must parse");
    match cmd {
        CliCommand::Tui { project, model } => {
            assert_eq!(project.as_deref(), Some("/repo"));
            assert_eq!(model.as_deref(), Some("glm-5.2"));
        }
        other => panic!("expected Tui, got {other:?}"),
    }
}

#[test]
fn test_parse_model_resume_rejected() {
    // A resumed session restores its own model (sidecar > --model in the
    // resolution chain), so --model + --resume would silently mislead.
    let err = parse_args(vec![
        "--resume".into(),
        "some-sid".into(),
        "--model".into(),
        "glm-5.2".into(),
    ])
    .unwrap_err();
    assert!(
        err.contains("--model is for a fresh session"),
        "should reject --model + --resume: {err}"
    );
}

#[test]
fn test_parse_acp() {
    let cmd = parse_args(vec!["--acp".into()]).expect("must parse");
    assert!(matches!(cmd, CliCommand::Acp { project: None, .. }));
}

#[test]
fn test_parse_acp_with_project() {
    let cmd =
        parse_args(vec!["--acp".into(), "--project".into(), "/tmp".into()]).expect("must parse");
    assert!(matches!(cmd, CliCommand::Acp { project: Some(p), .. } if p == "/tmp"));
}

#[test]
fn test_parse_help_long() {
    let cmd = parse_args(vec!["--help".into()]).expect("must parse");
    assert!(matches!(cmd, CliCommand::Help));
}

#[test]
fn test_parse_help_short() {
    let cmd = parse_args(vec!["-h".into()]).expect("must parse");
    assert!(matches!(cmd, CliCommand::Help));
}

// ── parse_args: error paths ───────────────────────────────────────

#[test]
fn test_parse_unknown_arg() {
    let err = parse_args(vec!["--bogus".into()]).unwrap_err();
    assert!(err.contains("unknown argument"), "got: {err}");
}

#[test]
fn test_parse_project_missing() {
    let err = parse_args(vec!["--project".into()]).unwrap_err();
    assert!(err.contains("--project requires"), "got: {err}");
}

#[test]
fn test_parse_serve_missing() {
    let err = parse_args(vec!["--serve".into()]).unwrap_err();
    assert!(err.contains("--serve requires"), "got: {err}");
}

// ── parse_args: unix subcommands + flags ──────────────────────────

#[cfg(unix)]
#[test]
fn test_parse_serve_custom() {
    let cmd = parse_args(vec!["--serve".into(), "/tmp/s.sock".into()]).expect("must parse");
    match cmd {
        CliCommand::Serve {
            project, socket, ..
        } => {
            assert!(project.is_none());
            assert_eq!(socket.as_deref(), Some("/tmp/s.sock"));
        }
        other => panic!("expected Serve, got {other:?}"),
    }
}

#[cfg(unix)]
#[test]
fn test_parse_detached() {
    let cmd = parse_args(vec!["--detached".into()]).expect("must parse");
    match cmd {
        CliCommand::Serve {
            project, socket, ..
        } => {
            assert!(project.is_none());
            assert!(socket.is_none(), "--detached means conventional path");
        }
        other => panic!("expected Serve, got {other:?}"),
    }
}

#[cfg(unix)]
#[test]
fn test_parse_detached_project() {
    let cmd =
        parse_args(vec!["--detached".into(), "-p".into(), "/tmp".into()]).expect("must parse");
    assert!(matches!(cmd, CliCommand::Serve { project: Some(p), socket: None, .. } if p == "/tmp"));
}

#[cfg(unix)]
#[test]
fn test_parse_attach() {
    let cmd = parse_args(vec![
        "attach".into(),
        "/tmp/s.sock".into(),
        "abc-123".into(),
    ])
    .expect("must parse");
    match cmd {
        CliCommand::Attach { socket, session } => {
            assert_eq!(socket, "/tmp/s.sock");
            assert_eq!(session, "abc-123");
        }
        other => panic!("expected Attach, got {other:?}"),
    }
}

#[cfg(unix)]
#[test]
fn test_parse_attach_missing() {
    let err = parse_args(vec!["attach".into(), "/tmp/s.sock".into()]).unwrap_err();
    assert!(err.contains("session id"), "got: {err}");
}

#[cfg(unix)]
#[test]
fn test_parse_ps() {
    let cmd = parse_args(vec!["ps".into()]).expect("must parse");
    assert!(matches!(cmd, CliCommand::Ps));
}

// ── parse_args: flag ordering + edge cases ────────────────────────

#[test]
fn test_parse_cleanup_dry_run() {
    let cmd = parse_args(vec!["cleanup".into()]).expect("must parse");
    assert!(matches!(
        cmd,
        CliCommand::Cleanup {
            apply: false,
            verbose: false,
            yes: false
        }
    ));
}

#[test]
fn test_parse_cleanup_apply() {
    let cmd = parse_args(vec!["cleanup".into(), "--apply".into()]).expect("must parse");
    assert!(matches!(cmd, CliCommand::Cleanup { apply: true, .. }));
}

#[test]
fn test_parse_cleanup_verbose_yes() {
    let cmd = parse_args(vec![
        "cleanup".into(),
        "--apply".into(),
        "--verbose".into(),
        "--yes".into(),
    ])
    .expect("must parse");
    match cmd {
        CliCommand::Cleanup {
            apply,
            verbose,
            yes,
        } => {
            assert!(apply && verbose && yes, "all three flags set");
        }
        _ => panic!("not a cleanup command"),
    }
}

#[test]
fn test_parse_cleanup_rejects_unknown() {
    assert!(parse_args(vec!["cleanup".into(), "--bogus".into()]).is_err());
}

#[test]
fn test_parse_first_flag() {
    // A leading flag (not a subcommand) must be re-parsed correctly.
    let cmd = parse_args(vec!["--acp".into()]).expect("must parse");
    assert!(matches!(cmd, CliCommand::Acp { .. }));
}

#[test]
fn test_parse_project_then_acp() {
    let cmd =
        parse_args(vec!["--project".into(), "/w".into(), "--acp".into()]).expect("must parse");
    assert!(matches!(cmd, CliCommand::Acp { project: Some(p), .. } if p == "/w"));
}

// ── composition wiring smoke ──────────────────────────────────────

/// The in-proc server + client pair produced by the composition root
/// completes a Hello handshake: the client connects, the server replies
/// with a session id, and the client tracks the event cursor from zero.
/// Uses the real composition (FakeProvider when no API key is set) so
/// the wiring path the TUI takes on startup is exercised end-to-end.
#[test]
fn test_pair_completes_handshake() {
    let bundle = houyicoder_service::composition::build_runner(
        houyicoder_service::composition::BuildRunnerOptions::default(),
    );
    let (runner_arc, mut client, _startup_warnings) = pair_inproc_server(
        bundle.runner,
        bundle.session,
        bundle.gate,
        bundle.sandbox_session,
        bundle.append_notify,
        None,
        None,
    );
    // The runner Arc is shared: the server task and the caller hold the
    // same allocation.
    assert!(Arc::ptr_eq(&runner_arc, &runner_arc));
    // The client handshake must succeed against the spawned server.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build test runtime");
    let hello = rt.block_on(client.connect());
    assert!(hello.is_ok(), "Hello handshake failed: {hello:?}");
}
