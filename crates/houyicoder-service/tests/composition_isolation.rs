//! Regression gates for structural test isolation: a runner built with
//! default options must not write anything under the sessions root, no
//! matter which crate links the composition root or how the test is
//! invoked. Lives in the integration tier so the service crate is linked
//! as a plain dependency - the same linkage any other crate's test gets.
//! The default-build gate is mutation-verified (reverting the default
//! stores to disk turns it red); the disk gate guards the opposite
//! direction (the disk opt-in must actually persist).

use houyicoder_context::{EventId, TurnEvent, TurnEventKind};

/// The default-build gate asserts on THIS build's sid dir rather than the
/// root's global entry count. A count-based assert is a global probe: a
/// concurrent writer (a dogfood session in another terminal) trips it with
/// a false accusation pointing at build_runner, while the real writer is
/// elsewhere. The sid assert is immune to concurrent writers, stays red
/// under the mutation (the mutated default writes exactly this sid's dir),
/// and the failure message names the true culprit.
#[test]
fn test_default_build_off_disk() {
    let root = houyicoder_service::composition::session_log_root();
    let bundle = houyicoder_service::composition::build_runner(
        houyicoder_service::composition::BuildRunnerOptions::default(),
    );
    // The sidecar write happens synchronously inside build_runner; dropping
    // the bundle flushes nothing further.
    let sid = bundle.session.to_string();
    drop(bundle);
    assert!(
        !root.join(&sid).exists(),
        "default build_runner wrote {sid} into the real sessions root {}",
        root.display()
    );
}

/// The mirror direction: the disk opt-in must actually persist. Builds a
/// runner with the disk preset at an owned temp root, appends one durable
/// event, and asserts both the build-time sidecar and the event log are on
/// disk. Wires the wrong store at the disk preset (an in-memory one) and
/// this turns red, so the production entries' persistence is guarded by
/// behavior, not by reading the call sites.
#[tokio::test]
async fn test_disk_options_write_durable() {
    let root = std::env::temp_dir().join(format!(
        "houyi-disk-isolation-{}-{}",
        std::process::id(),
        line!()
    ));
    drop(std::fs::remove_dir_all(&root));
    std::fs::create_dir_all(&root).expect("mkdir root");
    let bundle = houyicoder_service::composition::build_runner(
        houyicoder_service::composition::BuildRunnerOptions::disk_at(root.clone(), None, None),
    );
    let sid_dir = root.join(bundle.session.to_string());
    assert!(
        sid_dir.join("session.json").is_file(),
        "disk preset must write the sidecar at build time under {}",
        root.display()
    );
    let store = bundle.runner.store();
    store
        .append(TurnEvent {
            id: EventId::new(),
            session: bundle.session,
            ts: 0,
            prev_hash: None,
            kind: TurnEventKind::UserInput {
                text: "durable".to_string(),
            },
        })
        .await
        .expect("append");
    let body = std::fs::read_to_string(sid_dir.join("log.jsonl")).unwrap_or_default();
    assert!(
        body.contains("durable"),
        "disk preset must flush the appended event to the session log (got {} bytes)",
        body.len()
    );
    drop(store);
    drop(bundle);
    drop(std::fs::remove_dir_all(&root));
}

/// The dream's cross-session scan root must be None on an in-memory build.
/// Asserts THROUGH the production path (build_runner -> assemble ->
/// wire_background_memory -> store facade -> backend), not the backend in
/// isolation - the chain is exactly where the original read-side leak lived,
/// so every hop (backend override, facade forward, wire pass-through, dream
/// wiring) must stay intact for this to pass. A default build must never
/// read the real home.
#[test]
fn test_memory_skips_scan_root() {
    let project = temp_project_dir();
    let mut options = houyicoder_service::composition::BuildRunnerOptions::default();
    options.project = Some(project.to_string_lossy().into_owned());
    let bundle = houyicoder_service::composition::build_runner(options);
    assert!(
        bundle.runner.dream_session_log_root().is_none(),
        "in-memory build must expose no cross-session scan root (got {:?})",
        bundle.runner.dream_session_log_root(),
    );
    drop(bundle);
    drop(std::fs::remove_dir_all(&project));
}

/// The disk build must derive its scan root from its own backend (the same
/// root it persists to), not from a hardcoded session_log_root(). The
/// mirror of the no-scan-root gate: a drift in the derivation chain (delete
/// the facade forward, delete the backend override, hardcode None in wire)
/// turns this red instead of degrading silently to "no cross-session scan".
#[test]
fn test_disk_derives_scan_root() {
    let sessions = std::env::temp_dir().join(format!(
        "houyi-scan-disk-{}-{}",
        std::process::id(),
        line!()
    ));
    drop(std::fs::remove_dir_all(&sessions));
    std::fs::create_dir_all(&sessions).expect("mkdir sessions root");
    let project = temp_project_dir();
    let bundle = houyicoder_service::composition::build_runner(
        houyicoder_service::composition::BuildRunnerOptions::disk_at(
            sessions.clone(),
            Some(project.to_string_lossy().into_owned()),
            None,
        ),
    );
    assert_eq!(
        bundle.runner.dream_session_log_root(),
        Some(sessions.as_path()),
        "disk build must derive its scan root from its own backend, not a hardcoded root"
    );
    drop(bundle);
    drop(std::fs::remove_dir_all(&sessions));
    drop(std::fs::remove_dir_all(&project));
}

/// A throwaway project dir with a workspace manifest so assemble's workspace
/// branch wires the dream (the no-manifest branch skips it). The dream is
/// the seam the scan-root gates read, so the project must resolve for the
/// gate to exercise the production path.
fn temp_project_dir() -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!(
        "houyi-scan-proj-{}-{}",
        std::process::id(),
        line!()
    ));
    std::fs::create_dir_all(&d).expect("mkdir project");
    std::fs::write(d.join("Cargo.toml"), "[workspace]\nmembers = []\n").expect("manifest");
    d
}
