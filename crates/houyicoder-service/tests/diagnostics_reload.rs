//! The load-bearing claim behind installing one subscriber at the
//! composition root: a filter changed while the process runs reaches the
//! engine crate, not merely the crate that changed it.
//!
//! This is the defect the sink replaces, written as a test. Two hand-rolled
//! loggers preceded it. The engine's read its path once from the environment
//! and fixed it for the life of the process; the interface's could be
//! switched at runtime by a command. So a switch flipped mid-session enabled
//! the interface half and left the engine half off -- and because the
//! interface half kept writing, the file looked healthy. A reader who found
//! no engine lines in it would conclude the engine never ran, which is a
//! worse position than having no log at all: absent evidence that reads as
//! evidence of absence.
//!
//! One test, one binary, because install sets the process-wide default
//! subscriber and a second install in the same process is refused by design.

use std::sync::Arc;

use crate::common;
use houyicoder_core::agent::Runner;
use houyicoder_service::diagnostics;
use std::env::temp_dir;
use std::fs::read_to_string;
use std::fs::remove_dir_all;
use std::process::id;
use tracing_subscriber::filter::LevelFilter;

/// A level raised through the handle takes effect in the engine layer
/// without a restart, and a level of off suppresses the same call site.
///
/// Asserts on the file the operator actually reads, not on the handle's
/// return value. A handle that reports success while the engine stays silent
/// is the exact failure this guards, so the handle cannot be the witness.
///
/// Abort is the call site under test because it is reachable synchronously
/// from a test and logs on a path with no provider or network behind it.
#[test]
fn test_reload_reaches_core() {
    let dir = temp_dir().join(format!("houyi-diag-{}", id()));
    let path = dir.join("debug.log");
    drop(remove_dir_all(&dir));
    let handle = diagnostics::install(&path).expect("install the diagnostic sink");

    let (runner, _session) = common::stub_runner();
    let runner: Arc<Runner> = runner;

    // Installed disabled: the engine's call site must produce nothing, so a
    // normal run leaves no file traffic behind.
    runner.abort();
    let quiet = read_to_string(&path).expect("the log file exists");
    assert!(
        !quiet.contains("abort"),
        "a disabled sink must not record the engine, got: {quiet}"
    );

    // Raised at runtime, as a debug command would. No restart, and the
    // engine was never told.
    handle
        .set_level(LevelFilter::DEBUG)
        .expect("raise the level");
    runner.abort();
    let loud = read_to_string(&path).expect("the log file exists");
    assert!(
        loud.contains("abort"),
        "a level raised at runtime must reach houyicoder-core, got: {loud}"
    );

    // And back off again, so the switch is a switch rather than a latch.
    handle.disable().expect("disable");
    let before = read_to_string(&path).expect("the log file exists");
    runner.abort();
    let after = read_to_string(&path).expect("the log file exists");
    assert_eq!(
        before, after,
        "a disabled sink must stop recording the engine"
    );

    drop(remove_dir_all(&dir));
}
