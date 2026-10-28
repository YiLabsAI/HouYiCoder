//! Managed Git worktrees: the session that isolates a turn, the creation
//! record that makes a tree this product's object, the lease a live session
//! holds while it works in one, the identity check that decides whether a
//! directory is that tree, the preview a removal is decided from, and the
//! record of a removal that stopped with the directory still there.

pub mod changes;
mod cleanup;
pub mod controller;
pub mod git_discard;
pub mod identify;
pub mod preview;
pub mod record;
pub mod session;
