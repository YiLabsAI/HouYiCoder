//! Lifetime guard for a hot-reload driver. The engine holds one of these as
//! an erased, named handle so a driver constructed at the composition root
//! (which depends on the watcher library) lives as long as the runner and is
//! torn down with it, without the engine layer depending on the driver's
//! concrete type. Dropping the guard stops the watcher and its thread.

/// A handle whose drop stops a skill hot-reload driver (watcher + thread).
/// The engine holds it for the session lifetime only; reading it is not
/// needed — the driver reloads the registry through its own handle, this
/// guard just keeps the driver alive.
pub trait SkillReloadGuard: Send + Sync {}
