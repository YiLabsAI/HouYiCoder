//! Promoting a running foreground child to the background.

use super::*;

/// Promoting a running foreground child flips the mode its ending is
/// published with and releases the waiting spawn call. A second promotion is
/// a no-op: the spawn call is no longer waiting, and the ending already
/// notifies, so reporting another transition would tell the caller it freed
/// a turn it did not.
#[test]
fn test_background_child_promotes_once() {
    let (runtime, _store, _parent_sid) = runtime_with_text_child("ok");
    let runner = Arc::new(Runner::new(
        runtime.store.clone(),
        Arc::new(FakeProvider::text("ok")),
        ToolRegistry::new(),
        RunnerConfig::default(),
    ));
    let flag = Arc::new(AtomicBool::new(false));
    let promote = Arc::new(Notify::new());
    runtime.register_child(
        "c1",
        &runner,
        CancellationToken::new(),
        Some(Arc::clone(&flag)),
        Some(Arc::clone(&promote)),
    );
    assert!(
        runtime.background_child("c1"),
        "a live foreground child promotes"
    );
    assert!(
        flag.load(Ordering::Acquire),
        "the ending now notifies the parent"
    );
    assert!(
        !runtime.background_child("c1"),
        "a second promotion reports no transition"
    );
    assert!(
        !runtime.background_child("unknown"),
        "an unknown child reports no transition"
    );
    // A child registered without a promotion channel (already background, or
    // a caller with no bus) cannot be promoted.
    runtime.register_child("c2", &runner, CancellationToken::new(), None, None);
    assert!(
        !runtime.background_child("c2"),
        "no flag means no promotion"
    );
}
