//! Hot-reload timing + watch-path policy, kept pure: no OS, no threads, no
//! IO. A driver owns the watcher and thread, feeds events here, and acts on
//! the emitted actions. Splitting the policy out makes the debounce +
//! write-stability timing unit-testable without a filesystem.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::discover::{CONFIG_DIR_FAMILIES, MANAGED_DIR, find_git_root, walk_up_to_root};

/// Collapse burst file-change events into a single reload after this quiet
/// gap. Without it, each file in a burst fires its own reload.
const RELOAD_DEBOUNCE: Duration = Duration::from_millis(300);

/// Wait for a changed file to stop changing for this long before reloading,
/// so a half-written body does not produce a transient parse failure that
/// drops the skill from the set.
const WRITE_STABILITY: Duration = Duration::from_secs(1);

/// How deep to watch a directory. Owned here so the type does not name the
/// watcher library, which would drag an external dependency into this
/// pure-data layer. The driver maps Deep to a recursive watch and Shallow
/// to a non-recursive one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchDepth {
    /// Watch only this directory's immediate children. Used for a config
    /// family directory so a skills subdirectory creation surfaces without
    /// recursing into sibling trees that churn independently.
    Shallow,
    /// Watch this directory recursively. Used only for a real skills
    /// directory, where nested skill-name/SKILL.md layouts must surface.
    Deep,
}

/// A reload decision the driver acts on. CheckStability is emitted when the
/// debounce quiet gap elapses; the driver stats the pending files and
/// reports back, after which Reload is emitted once write-stability holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// The debounce gap closed; the driver should check whether the pending
    /// files have stopped changing.
    CheckStability(Vec<PathBuf>),
    /// Write-stability held; the driver should re-discover and swap.
    Reload,
}

/// Pure timing state machine: debounce burst events, then require
/// write-stability before allowing a reload. Instant is a parameter so
/// tests drive time without sleeping. The driver owns the IO (stat for
/// stability); the scheduler only decides when to ask for it and when to
/// fire.
#[derive(Debug)]
pub struct ReloadScheduler {
    state: State,
}

#[derive(Debug)]
enum State {
    Idle,
    Debouncing {
        deadline: Instant,
        paths: HashSet<PathBuf>,
    },
    AwaitingStability {
        paths: Vec<PathBuf>,
        stable_since: Option<Instant>,
    },
}

impl Default for ReloadScheduler {
    fn default() -> Self {
        Self::new()
    }
}

impl ReloadScheduler {
    /// New idle scheduler (no pending reload).
    pub fn new() -> Self {
        Self { state: State::Idle }
    }

    /// Record a file-change event. Extends the debounce deadline and folds
    /// the path into the pending set. An event during stability checking
    /// restarts the debounce because the file is still churning.
    pub fn on_event(&mut self, now: Instant, path: PathBuf) {
        match &mut self.state {
            State::Idle => {
                let mut paths = HashSet::new();
                paths.insert(path);
                self.state = State::Debouncing {
                    deadline: now + RELOAD_DEBOUNCE,
                    paths,
                };
            }
            State::Debouncing { deadline, paths } => {
                *deadline = now + RELOAD_DEBOUNCE;
                paths.insert(path);
            }
            State::AwaitingStability { paths, .. } => {
                let mut merged: HashSet<PathBuf> = paths.iter().cloned().collect();
                merged.insert(path);
                self.state = State::Debouncing {
                    deadline: now + RELOAD_DEBOUNCE,
                    paths: merged,
                };
            }
        }
    }

    /// Advance the state machine. Returns CheckStability when the debounce
    /// gap closes, Reload when write-stability holds, else None. A returned
    /// Reload clears the scheduler back to idle.
    pub fn poll(&mut self, now: Instant) -> Option<Action> {
        let action = match &self.state {
            State::Idle => None,
            State::Debouncing { deadline, paths } if now >= *deadline => {
                Some(Action::CheckStability(paths.iter().cloned().collect()))
            }
            State::Debouncing { .. } => None,
            State::AwaitingStability {
                stable_since: None, ..
            } => None,
            State::AwaitingStability {
                stable_since: Some(since),
                ..
            } if now >= *since + WRITE_STABILITY => Some(Action::Reload),
            State::AwaitingStability { .. } => None,
        };
        match &action {
            Some(Action::CheckStability(paths)) => {
                self.state = State::AwaitingStability {
                    paths: paths.clone(),
                    stable_since: None,
                };
            }
            Some(Action::Reload) => {
                self.state = State::Idle;
            }
            None => {}
        }
        action
    }

    /// Report whether the pending files have stopped changing. A stable
    /// report starts the stability window; a still-changing report resets it
    /// so a file written in several bursts only reloads after the final
    /// burst settles. No-op outside the stability-check state.
    pub fn confirm_stable(&mut self, now: Instant, stable: bool) {
        if let State::AwaitingStability { stable_since, .. } = &mut self.state
            && stable
        {
            if stable_since.is_none() {
                *stable_since = Some(now);
            }
        } else if let State::AwaitingStability { stable_since, .. } = &mut self.state
            && !stable
        {
            *stable_since = None;
        }
    }
}

/// Decide whether a reload should swap the new skill set in, based only on
/// counts and whether the watch roots were readable. The guard survives a
/// transient read failure (a checkout mid-way, a briefly unreadable
/// directory): nothing while roots are fine is a legitimate empty set (the
/// user deleted the last skill), but a nothing or partial result while a
/// root is unreadable is suspect and must not wipe the armed set.
pub fn should_swap(old_len: usize, new_len: usize, roots_readable: bool) -> bool {
    if roots_readable {
        return true;
    }
    // A root unreadable: a shrink is suspect (partial read failure); a
    // non-shrink is fine to swap.
    new_len >= old_len
}

/// Enumerate the directories a driver should watch so it covers exactly the
/// paths discovery scans, plus the nearest existing ancestor of each skills
/// directory so a skills directory created mid-session surfaces. Returns
/// (path, depth) pairs: a real skills directory is watched Deep; a config
/// family directory (when the skills subdirectory does not yet exist) is
/// watched Shallow so its creation surfaces without recursing into sibling
/// trees that churn independently. A family directory that does not exist
/// at either level is skipped (a brand-new workspace with no config
/// directory is not covered until restart). Deduplicates by canonical path.
pub fn watch_roots(cwd: Option<&Path>, home: Option<&Path>) -> Vec<(PathBuf, WatchDepth)> {
    let mut out: Vec<(PathBuf, WatchDepth)> = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();

    // Managed path is itself a skills directory.
    let managed = Path::new(MANAGED_DIR);
    if managed.is_dir() {
        push_root(&mut out, &mut seen, managed.to_path_buf(), WatchDepth::Deep);
    }

    // Project: walk cwd up to git root, each directory x each family.
    if let Some(cwd_raw) = cwd {
        let cwd = dunce::canonicalize(cwd_raw).unwrap_or_else(|_| cwd_raw.to_path_buf());
        let git_root = find_git_root(&cwd);
        for dir in walk_up_to_root(&cwd, git_root.as_deref()) {
            for family in CONFIG_DIR_FAMILIES {
                let family_dir = dir.join(family);
                let skills_dir = family_dir.join("skills");
                if skills_dir.is_dir() {
                    push_root(&mut out, &mut seen, skills_dir, WatchDepth::Deep);
                } else if family_dir.is_dir() {
                    push_root(&mut out, &mut seen, family_dir, WatchDepth::Shallow);
                }
            }
        }
    }

    // User: home x each family.
    if let Some(home) = home {
        for family in CONFIG_DIR_FAMILIES {
            let family_dir = home.join(family);
            let skills_dir = family_dir.join("skills");
            if skills_dir.is_dir() {
                push_root(&mut out, &mut seen, skills_dir, WatchDepth::Deep);
            } else if family_dir.is_dir() {
                push_root(&mut out, &mut seen, family_dir, WatchDepth::Shallow);
            }
        }
    }

    out
}

/// Push a (path, depth) entry, deduplicating by canonical path so
/// overlapping walk-up roots do not double-register.
fn push_root(
    out: &mut Vec<(PathBuf, WatchDepth)>,
    seen: &mut HashSet<PathBuf>,
    path: PathBuf,
    depth: WatchDepth,
) {
    let canon = dunce::canonicalize(&path).unwrap_or_else(|_| path.clone());
    if seen.insert(canon) {
        out.push((path, depth));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn t0() -> Instant {
        Instant::now()
    }

    /// A single event fires CheckStability only after the debounce gap, and
    /// Reload only after the stability window.
    #[test]
    fn test_reload_after_stability() {
        let mut s = ReloadScheduler::new();
        let start = t0();
        s.on_event(start, PathBuf::from("a/SKILL.md"));
        assert!(s.poll(start + Duration::from_millis(100)).is_none());
        let act = s.poll(start + RELOAD_DEBOUNCE + Duration::from_millis(1));
        assert!(matches!(act, Some(Action::CheckStability(_))));
        s.confirm_stable(start + RELOAD_DEBOUNCE + Duration::from_millis(2), true);
        assert!(
            s.poll(start + RELOAD_DEBOUNCE + Duration::from_millis(500))
                .is_none()
        );
        let act = s.poll(start + RELOAD_DEBOUNCE + WRITE_STABILITY + Duration::from_millis(10));
        assert_eq!(act, Some(Action::Reload));
        assert!(
            s.poll(start + RELOAD_DEBOUNCE + WRITE_STABILITY + Duration::from_secs(2))
                .is_none()
        );
    }

    /// Burst events extend the deadline so only the last quiet gap fires.
    #[test]
    fn test_burst_extends_deadline() {
        let mut s = ReloadScheduler::new();
        let start = t0();
        s.on_event(start, PathBuf::from("a/SKILL.md"));
        s.on_event(
            start + RELOAD_DEBOUNCE - Duration::from_millis(10),
            PathBuf::from("b/SKILL.md"),
        );
        assert!(
            s.poll(start + RELOAD_DEBOUNCE + Duration::from_millis(1))
                .is_none()
        );
        let act = s.poll(start + RELOAD_DEBOUNCE * 2 + Duration::from_millis(1));
        match act {
            Some(Action::CheckStability(paths)) => assert_eq!(paths.len(), 2),
            other => panic!("expected CheckStability, got {other:?}"),
        }
    }

    /// A still-changing file resets the stability window so a multi-burst
    /// write only reloads after the final burst settles.
    #[test]
    fn test_changing_resets_stability() {
        let mut s = ReloadScheduler::new();
        let start = t0();
        s.on_event(start, PathBuf::from("a/SKILL.md"));
        drop(s.poll(start + RELOAD_DEBOUNCE + Duration::from_millis(1)));
        s.confirm_stable(start + RELOAD_DEBOUNCE + Duration::from_millis(2), true);
        s.confirm_stable(start + RELOAD_DEBOUNCE + Duration::from_millis(600), false);
        s.confirm_stable(start + RELOAD_DEBOUNCE + Duration::from_millis(700), true);
        assert!(
            s.poll(start + RELOAD_DEBOUNCE + Duration::from_millis(800))
                .is_none()
        );
        let act = s.poll(start + RELOAD_DEBOUNCE + WRITE_STABILITY + Duration::from_millis(800));
        assert_eq!(act, Some(Action::Reload));
    }

    /// An event during stability checking restarts debounce.
    #[test]
    fn test_event_restarts_debounce() {
        let mut s = ReloadScheduler::new();
        let start = t0();
        s.on_event(start, PathBuf::from("a/SKILL.md"));
        drop(s.poll(start + RELOAD_DEBOUNCE + Duration::from_millis(1)));
        s.on_event(
            start + RELOAD_DEBOUNCE + Duration::from_millis(100),
            PathBuf::from("b/SKILL.md"),
        );
        assert!(
            s.poll(start + RELOAD_DEBOUNCE + Duration::from_millis(200))
                .is_none()
        );
        let act = s.poll(start + RELOAD_DEBOUNCE * 2 + Duration::from_millis(101));
        match act {
            Some(Action::CheckStability(paths)) => assert_eq!(paths.len(), 2),
            other => panic!("expected CheckStability, got {other:?}"),
        }
    }

    /// Readable roots: swap regardless of count (empty is legitimate — the
    /// user deleted the last skill).
    #[test]
    fn test_readable_roots_swap() {
        assert!(should_swap(5, 0, true), "delete all when roots readable");
        assert!(should_swap(5, 3, true));
        assert!(should_swap(0, 0, true));
    }

    /// Unreadable root: a shrink is suspect (partial read failure) — keep;
    /// a non-shrink is fine to swap.
    #[test]
    fn test_unreadable_keeps_shrink() {
        assert!(
            !should_swap(5, 0, false),
            "total loss on unreadable root: keep"
        );
        assert!(
            !should_swap(5, 3, false),
            "partial shrink on unreadable root: keep"
        );
        assert!(should_swap(5, 5, false), "no shrink: swap");
        assert!(should_swap(3, 5, false), "growth: swap");
    }

    /// A real skills directory is Deep; a family directory without a skills
    /// subdirectory is Shallow; a family directory that does not exist is
    /// skipped.
    #[test]
    fn test_watch_roots_depth() {
        let tmp = std::env::temp_dir().join(format!("skill-watch-{}", std::process::id()));
        drop(std::fs::remove_dir_all(&tmp));
        std::fs::create_dir_all(tmp.join(".houyicoder").join("skills")).unwrap();
        std::fs::create_dir_all(tmp.join(".claude")).unwrap();
        let roots = watch_roots(Some(&tmp), None);
        let deep: Vec<_> = roots
            .iter()
            .filter(|(_, d)| *d == WatchDepth::Deep)
            .map(|(p, _)| p.clone())
            .collect();
        let shallow: Vec<_> = roots
            .iter()
            .filter(|(_, d)| *d == WatchDepth::Shallow)
            .map(|(p, _)| p.clone())
            .collect();
        assert!(
            deep.iter().any(|p| p.ends_with("skills")),
            "skills dir watched Deep: {deep:?}"
        );
        assert!(
            shallow.iter().any(|p| p.ends_with(".claude")),
            "family dir without skills watched Shallow: {shallow:?}"
        );
        assert!(
            !roots.iter().any(|(p, _)| p.ends_with(".agents")),
            "absent family skipped"
        );
        drop(std::fs::remove_dir_all(&tmp));
    }

    /// Overlapping walk-up roots do not double-register the same canonical
    /// skills directory.
    #[test]
    fn test_watch_roots_dedup() {
        let tmp = std::env::temp_dir().join(format!("skill-watch-dedup-{}", std::process::id()));
        drop(std::fs::remove_dir_all(&tmp));
        std::fs::create_dir_all(tmp.join(".houyicoder").join("skills")).unwrap();
        let roots = watch_roots(Some(&tmp), None);
        let skills_entries: Vec<_> = roots
            .iter()
            .filter(|(p, _)| p.ends_with("skills"))
            .map(|(p, _)| p.clone())
            .collect();
        assert_eq!(
            skills_entries.len(),
            1,
            "deduped to one: {skills_entries:?}"
        );
        drop(std::fs::remove_dir_all(&tmp));
    }
}
