//! Desktop integration surface: the terminal boundary a session runs
//! against and the native clipboard a copy can also use.
//!
//! The terminal module owns mode ownership, the hangup probe, termination
//! registration, and the output capabilities a renderer gates its escape
//! sequences on. Neither module initializes when the host runs without a
//! desktop.

pub mod clipboard;
pub mod terminal;
