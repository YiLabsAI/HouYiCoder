//! Consolidated sandbox suites for this crate. Each module is the original
//! test file included verbatim through a path attribute; aggregating into one
//! binary cuts the number of test binaries nextest has to launch without
//! dropping a test. An included module is private to this binary, so an unused
//! item inside one warns where the same item at a test-file root did not.

#[path = "seatbelt.rs"]
mod seatbelt;
#[path = "seatbelt_kernel.rs"]
mod seatbelt_kernel;
