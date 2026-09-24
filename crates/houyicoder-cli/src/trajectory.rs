//! The trajectory pane's read layer.
//!
//! The pane draws a window of the session's durable log: this module owns the
//! bytes-to-rows path that produces it. The page reads and their state machine
//! live in the reader submodule, the projection of durable events into the
//! pane's own view types lives in the view submodule, and the tests that drive
//! both live under the tests family.

mod reader;
#[cfg(test)]
mod tests;
mod turns;
mod view;

pub(crate) use reader::SessionLogTrajectory;
