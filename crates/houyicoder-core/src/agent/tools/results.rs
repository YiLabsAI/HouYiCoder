//! One session's stored results, folded from its log and answered in bounded
//! pages.
//!
//! A tool that prints more than the model reads leaves the rest in a block the
//! log names. This module folds the log once, keeps a per-session index of the
//! results it found, and answers listing, reading, and searching from bounded
//! window reads. The index is derived data: dropping it costs one fold and
//! never a record, and every name it answers with is rebuilt from the log.

mod catalog;
mod cursor;
mod entry;
mod fold;
mod search;
mod windows;

pub use catalog::SessionResultCatalog;

#[cfg(test)]
mod tests;
