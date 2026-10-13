//! The row sets the draw pass publishes and the input paths read.
//!
//! One module because the types are the same data at three lifetimes: the row
//! set a view builds for the frame it is drawing, the row set the slot cache
//! keeps across frames until its key goes stale, and the keys of the last
//! visible window a click or key resolves a screen row against.

pub(crate) mod display_rows_cache;
pub(crate) mod last_row_keys;
pub(crate) mod row_set;
pub(crate) mod slots_key;

#[cfg(test)]
mod tests;
