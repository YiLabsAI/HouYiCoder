//! Consolidated contract and end-to-end suites for this crate. Each module is
//! the original test file included verbatim through a path attribute;
//! aggregating into one binary cuts the number of test binaries nextest has to
//! launch without dropping a test. The reward baseline and the recall
//! benchmark keep their own targets so the verify filter can exclude them by
//! name and a benchmark run can select either alone.
//! An included module is private to this binary, so an unused item inside one
//! warns where the same item at a test-file root did not.

#[path = "budget_pressure_gate.rs"]
mod budget_pressure_gate;
#[path = "compaction_pipeline.rs"]
mod compaction_pipeline;
#[path = "loop_with_sandbox.rs"]
mod loop_with_sandbox;
#[path = "memory_loop.rs"]
mod memory_loop;
#[path = "observability_loop.rs"]
mod observability_loop;
#[path = "recall_quality.rs"]
mod recall_quality;
#[path = "runner_assembly.rs"]
mod runner_assembly;
#[path = "tools_with_sandbox.rs"]
mod tools_with_sandbox;
