//! Integration test: the composition root builds a usable runner regardless
//! of env state. Lives in tests/ (integration tier) because build_runner
//! wires the REAL provider when an API key is set -- a unit test must mock
//! the bottom (no real reqwest), so the real-wiring check belongs here, not
//! in lib. make check-full runs this; make check (unit, --lib) does not.
use houyicoder_service::composition::{BuildRunnerOptions, build_runner};

#[test]
fn test_build_runner_returns_usable() {
    // build_runner must not panic regardless of env state. When a key is
    // set it wires a real provider; otherwise FakeProvider. Either way the
    // runner and session are usable. The default options are in-memory on
    // both stores by construction (persistence is opt-in via
    // BuildRunnerOptions::disk), so this test writes nothing to the real
    // sessions dir under HOME.
    let bundle = build_runner(BuildRunnerOptions::default());
    assert!(!bundle.session.to_string().is_empty());
    let _store = bundle.runner.store();
}
