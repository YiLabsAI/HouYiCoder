//! Desktop terminal integration surface.
//!
//! The terminal module owns the boundary a terminal session runs against:
//! mode ownership, the hangup probe, termination registration, and the
//! output capabilities a renderer gates its escape sequences on. Not
//! initialized when the host runs without a real terminal.

pub mod terminal;
