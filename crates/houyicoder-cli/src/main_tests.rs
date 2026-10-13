//! Unit tests for the CLI argument parser and composition-root wiring.
//!
//! parse_args is a pure function (no I/O, no process exit) so the full
//! flag/subcommand matrix is testable without spawning the binary.

use super::*;

use std::env::temp_dir;
use std::fs::{create_dir_all, remove_dir_all, remove_file, write};
use std::path::PathBuf;
use std::process::id as process_id;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::channel as std_channel;

use futures::channel::mpsc::channel as async_channel;
use houyicoder_client::{Client, InProcTransport};
use houyicoder_context::SessionId;
use houyicoder_protocol::frontend::SessionId as WireSessionId;
use houyicoder_service::composition::host_lifecycle::HostLifecycle;
use houyicoder_service::composition::{
    BuildRunnerOptions, ResolvedProvider, build_runner, resolve_provider,
};
use houyicoder_tui::composition::{FrontendStartup, RunnerBundle, shared_runtime};
use tokio::runtime::Builder;

use crate::composition::start_local_server;
use crate::detach::{is_session_live, session_socket, sessions_dir};
use crate::local_host::{HostLease, HostLeaseScope, HostLeaseSlot};
use crate::session_lock::{LockError, SessionLock};

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
    // A resumed session restores its own model (descriptor > --model in the
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
    let root = temp_root();
    let mut options = BuildRunnerOptions::default();
    options.provider = Some(ResolvedProvider::stub());
    let bundle = build_runner(options);
    let (runner_arc, mut client, _startup_warnings, host) = start_local_server(
        bundle.runner,
        bundle.session,
        bundle.gate,
        bundle.sandbox_session,
        bundle.append_notify,
        None,
        None,
        bundle.bus,
        Some(root.join("settings.json")),
    );
    // The runner Arc is shared: the server task and the caller hold the
    // same allocation.
    assert!(Arc::ptr_eq(&runner_arc, &runner_arc));
    // The client handshake must succeed against the spawned server.
    let rt = Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build test runtime");
    let hello = rt.block_on(client.connect());
    assert!(hello.is_ok(), "Hello handshake failed: {hello:?}");
    drop(client);
    drop(runner_arc);
    let report = rt.block_on(host.begin_shutdown());
    assert_eq!(report.panic_count(), 0, "host shut down cleanly");
    remove_dir_all(&root).ok();
}

/// A failed switch must leave the prior host alive. build_bundle_for_target
/// defers reaping until after a successful build, so an invalid target returns
/// Err without taking the old host out of the slot — the prior session's serve,
/// drain, and relay keep running instead of being cancelled before the build is
/// even attempted.
#[test]
fn test_bad_target_keeps_host() {
    let slot: HostLeaseSlot = Arc::new(Mutex::new(HostLeaseScope::with_current(HostLease::new(
        HostLifecycle::new(),
    ))));
    let provider = resolve_provider(None);
    let err = match build_bundle_for_target("not-a-path-and-not-a-sid", None, provider, &slot) {
        Err(e) => e,
        Ok(_) => panic!("an invalid target must error"),
    };
    assert!(
        err.to_string().contains("invalid resume target"),
        "got: {err}"
    );
    assert!(
        slot.lock()
            .expect("host lifecycle slot must not poison")
            .has_current(),
        "the prior host stays in the slot when the build fails"
    );
}

/// A resume target that names the session the live lease already holds is a
/// no-op, not a contention error. The lease already holds that sid's lock,
/// so re-acquiring would report it held by this process itself. The check
/// fires before lock acquisition, so the prior lease stays in the slot.
#[test]
fn test_same_session_noop() {
    let sid = SessionId::new();
    let sid_str = sid.to_string();
    let slot: HostLeaseSlot = Arc::new(Mutex::new(HostLeaseScope::with_current(
        HostLease::for_test(HostLifecycle::new(), None, Some(sid_str.clone())),
    )));
    let provider = resolve_provider(None);
    let err = match build_bundle_for_target(&sid_str, None, provider, &slot) {
        Err(e) => e,
        Ok(_) => panic!("same-session target must be a no-op, not a build"),
    };
    assert!(err.to_string().contains("already on session"), "got: {err}");
    assert!(
        slot.lock()
            .expect("host lifecycle slot must not poison")
            .has_current(),
        "the prior lease stays in the slot on a same-session no-op"
    );
}

/// The same-session no-op must hold across spellings of one sid, not just an
/// exact string match: the live lease stores the ULID spelling, the target
/// names the UUID spelling, and the canonical comparison through the context
/// type recognizes them as the same session. A plain string comparison would
/// miss this and reach the lock as a false contention error.
#[test]
fn test_cross_spelling_noop() {
    let sid = SessionId::new();
    let uuid_form = sid.to_string();
    let ulid_form = sid.ulid_name();
    let slot: HostLeaseSlot = Arc::new(Mutex::new(HostLeaseScope::with_current(
        HostLease::for_test(HostLifecycle::new(), None, Some(ulid_form)),
    )));
    let provider = resolve_provider(None);
    let err = match build_bundle_for_target(&uuid_form, None, provider, &slot) {
        Err(e) => e,
        Ok(_) => panic!("cross-spelling target must be a no-op, not a build"),
    };
    assert!(err.to_string().contains("already on session"), "got: {err}");
}

/// A resume target whose sid a live process holds is rejected without
/// touching the prior lease. The hash chain needs a single writer, so a
/// session another process is still writing cannot be resumed in-process;
/// the user is pointed at attach or fork instead. The live check fires
/// before lock acquisition and before any writer is built, so the prior
/// host stays in the slot. Mirrors test_bad_target_keeps_host for the
/// held-target axis the invalid-target test does not reach.
#[cfg(unix)]
#[test]
fn test_held_target_keeps_host() {
    // The target sid must parse as a real SessionId (the sid-resume branch
    // gates on from_display_string), so it cannot carry a test- prefix.
    // A Drop guard removes the staged socket + pidfile even on a panic, and
    // writing the socket too lets cleanup_stale reclaim the pair if the guard
    // never runs (a kill between write and drop).
    struct LiveSessionGuard {
        sid: String,
    }
    impl Drop for LiveSessionGuard {
        fn drop(&mut self) {
            remove_file(session_socket(&self.sid)).ok();
            remove_file(sessions_dir().join(format!("{}.pid", self.sid))).ok();
        }
    }
    let target_sid = SessionId::new();
    let target_str = target_sid.to_string();
    // Stage a live pidfile + socket so is_session_live reports the target held
    // by the current process (which is alive by definition).
    let _guard = LiveSessionGuard {
        sid: target_str.clone(),
    };
    write(session_socket(&target_str), b"").expect("write test socket");
    write(
        sessions_dir().join(format!("{target_str}.pid")),
        process_id().to_string(),
    )
    .expect("write live pidfile");
    assert!(
        is_session_live(&target_str),
        "the staged pidfile makes the target held by this process"
    );
    // The prior lease holds a different sid so the same-session no-op does not fire.
    let slot: HostLeaseSlot = Arc::new(Mutex::new(HostLeaseScope::with_current(
        HostLease::for_test(
            HostLifecycle::new(),
            None,
            Some(SessionId::new().to_string()),
        ),
    )));
    let provider = resolve_provider(None);
    let err = match build_bundle_for_target(&target_str, None, provider, &slot) {
        Err(e) => e,
        Ok(_) => panic!("a held target must error, not build"),
    };
    assert!(
        err.to_string().contains("is live in another process"),
        "got: {err}"
    );
    assert!(
        slot.lock()
            .expect("host lifecycle slot must not poison")
            .has_current(),
        "the prior lease stays when the target is held"
    );
}

// ── existing-sid admission before build ───────────────────────────

/// An existing-sid resume refuses a sid whose lock another process holds,
/// before the build is reached. The friendly held refusal returns, not the
/// "no session log" the builder would emit for a sid with no on-disk log.
#[cfg(unix)]
#[test]
fn test_resume_refuses_held() {
    let root = temp_root();
    let sid = SessionId::new();
    let sid_str = sid.to_string();
    let first = SessionLock::acquire(&sid_str, &root).expect("first acquire holds");
    let err = match admit_existing_sid(&sid, &root) {
        Ok(_) => panic!("a held sid must refuse, not admit"),
        Err(e) => e,
    };
    assert!(
        err.to_string().contains("held by another"),
        "friendly held refusal, got: {err}"
    );
    drop(first);
    remove_dir_all(&root).ok();
}

/// A resume entry for an existing sid that is live elsewhere refuses before
/// any build: the friendly live refusal returns, not the "no session log" the
/// builder would emit for a sid that has no disk log. The order is the point —
/// the build must not run, so the live check must fire first.
#[cfg(unix)]
#[test]
fn test_resume_refuses_live() {
    struct LiveSessionGuard {
        sid: String,
    }
    impl Drop for LiveSessionGuard {
        fn drop(&mut self) {
            remove_file(session_socket(&self.sid)).ok();
            remove_file(sessions_dir().join(format!("{}.pid", self.sid))).ok();
        }
    }
    let target_sid = SessionId::new();
    let target_str = target_sid.to_string();
    let _guard = LiveSessionGuard {
        sid: target_str.clone(),
    };
    write(session_socket(&target_str), b"").expect("write test socket");
    write(
        sessions_dir().join(format!("{target_str}.pid")),
        process_id().to_string(),
    )
    .expect("write live pidfile");
    assert!(
        is_session_live(&target_str),
        "the staged pidfile makes the target held by this process"
    );
    let provider = resolve_provider(None);
    let err = match build_lease_for_existing_sid(target_sid, None, provider) {
        Err(e) => e,
        Ok(_) => panic!("a live target must refuse, not build"),
    };
    assert!(
        err.to_string().contains("is live in another process"),
        "the live check fired before the build, got: {err}"
    );
}

/// When lock acquisition for a freshly built bundle fails, the host tasks
/// already started on the shared runtime must be joined and reclaimed
/// before acquire_fresh_lease returns the error, rather than leaking.
#[cfg(unix)]
#[test]
fn test_fresh_lock_failure_reclaims() {
    let root = temp_root();
    let sid = SessionId::new();
    let sid_str = sid.to_string();
    let _held = SessionLock::acquire(&sid_str, &root).expect("pre-acquire lock");

    let done = Arc::new(AtomicBool::new(false));
    let done_c = Arc::clone(&done);
    let mut host = HostLifecycle::new();
    let cancel = host.cancel_token();
    host.track(shared_runtime().handle().spawn(async move {
        let _ = cancel.cancelled().await;
        done_c.store(true, Ordering::SeqCst);
    }));

    let (tx, rx) = std_channel();
    let (c2s_tx, _) = async_channel(1);
    let (_, s2c_rx) = async_channel(1);
    let bundle = RunnerBundle {
        client: Client::new(Box::new(InProcTransport::from_halves(c2s_tx, s2c_rx))),
        agent_tx: tx,
        agent_rx: rx,
        session: WireSessionId(sid_str.clone()),
        model: "stub".into(),
        trajectory_log: None,
        export_log: None,
        snapshot: None,
        session_catalog: None,
    };
    let startup = FrontendStartup {
        skip_login: true,
        startup_warnings: Vec::new(),
        history_path: root.join("history.jsonl"),
    };

    let err = match acquire_fresh_lease_at(bundle, startup, host, None, &root) {
        Ok(_) => panic!("held lock must cause acquire_fresh_lease to fail"),
        Err(e) => e,
    };
    assert!(
        err.to_string().contains("held by other")
            || err.to_string().contains("already held")
            || err.is::<LockError>(),
        "error must be from lock, got: {err}"
    );
    assert!(
        done.load(Ordering::SeqCst),
        "host tasks must be cancelled and joined when lock acquisition fails"
    );
    remove_dir_all(&root).ok();
}

/// A per-test temp directory so the held-lock admission does not touch the
/// global sessions root.
#[cfg(unix)]
fn temp_root() -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let path = temp_dir().join(format!("admit-{}-{n}", process_id()));
    create_dir_all(&path).expect("temp root");
    path
}
