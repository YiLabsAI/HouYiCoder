//! Integration test: the composition root builds a usable runner regardless
//! of env state. Lives in tests/ (integration tier) because build_runner
//! wires the REAL provider when an API key is set -- a unit test must mock
//! the bottom (no real reqwest), so the real-wiring check belongs here, not
//! in lib. make check-full runs this; make check (unit, --lib) does not.
use houyicoder_service::composition::{BuildRunnerOptions, build_runner};
use std::env::temp_dir;
use std::fs::create_dir_all;
use std::fs::remove_dir_all;
use std::process::id;

#[test]
fn test_build_runner_returns_usable() {
    // build_runner must not panic regardless of env state. When a key is
    // set it wires a real provider; otherwise FakeProvider. Either way the
    // runner and session are usable. The options are in-memory on both
    // stores, and the home is a temp dir, so this test writes nothing under
    // the developer's home.
    let home = temp_dir().join(format!("houyi-build-runner-home-{}", id()));
    create_dir_all(&home).expect("temp home");
    let mut options = BuildRunnerOptions::default();
    options.home = Some(home.clone());
    let bundle = build_runner(options);
    assert!(!bundle.session.to_string().is_empty());
    let _store = bundle.runner.store();
    // Drop the assembly (and its reload watch) before removing its home.
    drop(bundle);
    drop(remove_dir_all(&home));
}
