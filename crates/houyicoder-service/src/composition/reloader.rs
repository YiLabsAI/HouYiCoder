//! Skill hot-reload driver: owns the filesystem watcher and the thread that
//! feeds events through the pure timing policy (in the skill data leaf) and
//! re-discovers + swaps the registry set on a settled change. Lives at the
//! composition root because it depends on the watcher library + threads,
//! which the pure data leaf must not. The engine holds it behind the named
//! SkillReloadGuard trait for the session lifetime only; dropping it stops
//! the watcher and the thread.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use houyicoder_api::skill::SkillRegistry;
use houyicoder_core::agent::{ConditionalSkillActivator, SkillHookRegistrar, SkillReloadGuard};
use houyicoder_skill::lifecycle::{Action, ReloadScheduler, WatchDepth};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};

use super::skill::SkillRegistryImpl;

/// Quiet poll interval for the event loop. The thread blocks on the event
/// channel for this long, then drains the scheduler, so a reload fires
/// promptly after a debounce + stability window even with no new events.
const LOOP_POLL: Duration = Duration::from_millis(100);

/// The shared reload dependencies, bundled so the event loop and the event
/// router take one argument instead of five (the watcher library is not
/// one of them — it is owned by the loop).
struct ReloadDeps {
    registry: std::sync::Arc<SkillRegistryImpl>,
    activator: std::sync::Arc<dyn ConditionalSkillActivator>,
    registrar: std::sync::Arc<SkillHookRegistrar>,
    cwd: Option<PathBuf>,
    home: Option<PathBuf>,
}

impl ReloadDeps {
    /// Re-discover + swap, then refresh the conditional activator and
    /// invalidate + re-register only the changed skills' hooks.
    fn reload(&self) {
        let outcome = self
            .registry
            .reload(self.cwd.as_deref(), self.home.as_deref());
        if !outcome.swapped {
            return;
        }
        self.activator.refresh();
        self.registrar.invalidate(
            &outcome.changed,
            self.registry.as_ref() as &dyn SkillRegistry,
        );
    }
}

/// The driver. Constructed at the composition root; held by the runner for
/// the session. Dropping it drops the shutdown sender, which the thread
/// reads as "stop", so the watcher and thread tear down with the session.
pub struct SkillReloader {
    _thread: Option<JoinHandle<()>>,
    shutdown: Option<mpsc::Sender<()>>,
}

impl SkillReloadGuard for SkillReloader {}

impl SkillReloader {
    /// Watch the skill scan roots and spawn a thread that re-discovers on a
    /// settled change. Returns None (no reload) when no roots exist to
    /// watch — the feature is inert, not erroring, in a brand-new workspace
    /// with no config directory yet. Returns the guard as the named trait so
    /// the caller holds a lifetime handle without depending on this type.
    pub fn start(
        registry: std::sync::Arc<SkillRegistryImpl>,
        activator: std::sync::Arc<dyn ConditionalSkillActivator>,
        registrar: std::sync::Arc<SkillHookRegistrar>,
        cwd: Option<PathBuf>,
        home: Option<PathBuf>,
    ) -> Option<std::sync::Arc<dyn SkillReloadGuard>> {
        let roots = houyicoder_skill::lifecycle::watch_roots(cwd.as_deref(), home.as_deref());
        if roots.is_empty() {
            tracing::info!("no skill watch roots; hot-reload inert this session");
            return None;
        }
        let (event_tx, event_rx) = mpsc::channel::<Vec<PathBuf>>();
        let mut watcher: RecommendedWatcher = match RecommendedWatcher::new(
            move |res: Result<notify::Event, _>| {
                if let Ok(ev) = res
                    && event_tx.send(ev.paths).is_err()
                {
                    // Channel closed: the reloader is gone. Stop sending.
                }
            },
            notify::Config::default(),
        ) {
            Ok(w) => w,
            Err(e) => {
                tracing::warn!(error = %e, "skill watcher init failed; hot-reload off");
                return None;
            }
        };
        for (path, depth) in &roots {
            let mode = match depth {
                WatchDepth::Deep => RecursiveMode::Recursive,
                WatchDepth::Shallow => RecursiveMode::NonRecursive,
            };
            if let Err(e) = watcher.watch(path, mode) {
                tracing::warn!(path = %path.display(), error = %e, "watch add failed; skipping root");
            }
        }
        let (shutdown_tx, shutdown_rx) = mpsc::channel::<()>();
        let deps = ReloadDeps {
            registry,
            activator,
            registrar,
            cwd,
            home,
        };
        let thread = match thread::Builder::new()
            .name("skill-hot-reload".into())
            .spawn(move || run_loop(watcher, event_rx, shutdown_rx, deps))
        {
            Ok(handle) => handle,
            Err(e) => {
                tracing::warn!(error = %e, "skill hot-reload thread spawn failed; hot-reload off");
                return None;
            }
        };
        Some(std::sync::Arc::new(Self {
            _thread: Some(thread),
            shutdown: Some(shutdown_tx),
        }) as std::sync::Arc<dyn SkillReloadGuard>)
    }
}

impl Drop for SkillReloader {
    fn drop(&mut self) {
        // Drop the shutdown sender so the thread reads Disconnected and exits.
        self.shutdown.take();
    }
}

/// The event loop: route events (dynamic-watch skills/ creation vs
/// in-skills SKILL.md changes), drive the scheduler, and act on its
/// decisions (stability check, then reload + refresh + invalidate).
fn run_loop(
    mut watcher: RecommendedWatcher,
    event_rx: Receiver<Vec<PathBuf>>,
    shutdown_rx: Receiver<()>,
    deps: ReloadDeps,
) {
    let mut scheduler = ReloadScheduler::new();
    loop {
        // Drain any pending events (a burst may queue several).
        match event_rx.recv_timeout(LOOP_POLL) {
            Ok(paths) => route_events(&paths, &mut scheduler, &mut watcher, &deps),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        // Check shutdown: Ok(()) (signaled) or Disconnected (sender gone)
        // both mean stop; only Empty means continue.
        if !matches!(shutdown_rx.try_recv(), Err(mpsc::TryRecvError::Empty)) {
            return;
        }
        // Drive the scheduler.
        let now = Instant::now();
        while let Some(action) = scheduler.poll(now) {
            match action {
                Action::CheckStability(paths) => {
                    let stable = files_settled(&paths);
                    scheduler.confirm_stable(now, stable);
                }
                Action::Reload => deps.reload(),
            }
        }
    }
}

/// Route a batch of event paths. A newly-created skills directory (under a
/// shallow family watch) gets a dynamic recursive watch plus an immediate
/// reload to cover files that landed in the race window before the watch was
/// installed. A SKILL.md change inside a skills directory feeds the
/// scheduler (debounce + write-stability). Other paths are ignored.
fn route_events(
    paths: &[PathBuf],
    scheduler: &mut ReloadScheduler,
    watcher: &mut RecommendedWatcher,
    deps: &ReloadDeps,
) {
    let now = Instant::now();
    for path in paths {
        // Skip .git / node_modules churn.
        if path
            .components()
            .any(|c| c.as_os_str() == ".git" || c.as_os_str() == "node_modules")
        {
            continue;
        }
        if is_skills_dir(path) {
            // A skills/ directory appeared under a shallow family watch.
            // Watch it recursively and reload immediately to cover any
            // SKILL.md that already landed before the watch was installed.
            if watcher.watch(path, RecursiveMode::Recursive).is_err() {
                tracing::warn!(path = %path.display(), "dynamic skills/ watch failed");
            }
            deps.reload();
            continue;
        }
        if path.file_name().is_some_and(|n| n == "SKILL.md") {
            scheduler.on_event(now, path.clone());
        }
    }
}

/// Whether a path is a directory named "skills" (the dynamic-watch trigger).
fn is_skills_dir(path: &Path) -> bool {
    path.is_dir() && path.file_name().is_some_and(|n| n == "skills")
}

/// Stat each pending file's size, poll at the stability interval until the
/// size is unchanged for the stability window. Returns true once settled.
fn files_settled(paths: &[PathBuf]) -> bool {
    let mut last: Vec<Option<u64>> = paths.iter().map(|p| file_size(p.as_path())).collect();
    let poll = Duration::from_millis(500);
    let window = Duration::from_secs(1);
    let start = Instant::now();
    loop {
        thread::sleep(poll);
        let mut changed = false;
        for (i, p) in paths.iter().enumerate() {
            let s = file_size(p);
            if s != last[i] {
                last[i] = s;
                changed = true;
            }
        }
        if !changed && Instant::now().duration_since(start) >= window {
            return true;
        }
        if Instant::now().duration_since(start) >= Duration::from_secs(30) {
            // Give up after a long churn; let the next event retry.
            return false;
        }
    }
}

fn file_size(path: &Path) -> Option<u64> {
    std::fs::metadata(path).ok().map(|m| m.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use houyicoder_api::launcher::StdProcessLauncher;
    use houyicoder_api::skill::SkillRegistry;
    use houyicoder_api::trust::TrustState;
    use houyicoder_core::agent::{ConditionalActivation, HookRegistry, SkillHookRegistrar};
    use std::sync::{Arc, RwLock};

    fn write_skill(dir: &Path, name: &str) {
        let skill_dir = dir.join(".houyicoder").join("skills").join(name);
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {name}\n---\nbody\n"),
        )
        .unwrap();
    }

    fn deps_with(tmp: &Path) -> ReloadDeps {
        let registry = Arc::new(SkillRegistryImpl::discover_with_home(Some(tmp), None));
        let activator: Arc<dyn houyicoder_core::agent::ConditionalSkillActivator> =
            Arc::new(ConditionalActivation::new(
                Arc::clone(&registry) as Arc<dyn houyicoder_api::skill::SkillRegistry>,
                tmp.to_path_buf(),
            ));
        let hook_reg = Arc::new(HookRegistry::new());
        let trust = Arc::new(RwLock::new(TrustState::Trusted));
        let registrar = Arc::new(SkillHookRegistrar::new(
            hook_reg,
            trust,
            Arc::new(StdProcessLauncher::new()),
        ));
        ReloadDeps {
            registry,
            activator,
            registrar,
            cwd: Some(tmp.to_path_buf()),
            home: None,
        }
    }

    /// reload re-discovers and swaps so a skill added after construction
    /// surfaces in find. Exercises the reload path (registry.reload +
    /// activator.refresh + registrar.invalidate) without a watcher.
    #[test]
    fn test_reload_picks_new_skill() {
        let tmp = std::env::temp_dir().join(format!("skill-reload-deps-{}", std::process::id()));
        drop(std::fs::remove_dir_all(&tmp));
        write_skill(&tmp, "alpha");
        let deps = deps_with(&tmp);
        assert!(deps.registry.find("alpha").is_some());
        assert!(deps.registry.find("beta").is_none());
        write_skill(&tmp, "beta");
        deps.reload();
        assert!(
            deps.registry.find("beta").is_some(),
            "beta picked up after reload"
        );
        assert!(
            deps.registry.find("alpha").is_some(),
            "alpha survives reload"
        );
        drop(std::fs::remove_dir_all(&tmp));
    }

    /// start returns None (no reload) when no watch roots exist — a
    /// brand-new workspace with no config directory. No thread is spawned.
    #[test]
    fn test_start_no_roots_none() {
        let tmp = std::env::temp_dir().join(format!("skill-reload-noroots-{}", std::process::id()));
        drop(std::fs::remove_dir_all(&tmp));
        std::fs::create_dir_all(&tmp).unwrap();
        let registry = Arc::new(SkillRegistryImpl::discover_with_home(Some(&tmp), None));
        let activator: Arc<dyn houyicoder_core::agent::ConditionalSkillActivator> =
            Arc::new(ConditionalActivation::new(
                Arc::clone(&registry) as Arc<dyn SkillRegistry>,
                tmp.to_path_buf(),
            ));
        let hook_reg = Arc::new(HookRegistry::new());
        let trust = Arc::new(RwLock::new(TrustState::Trusted));
        let registrar = Arc::new(SkillHookRegistrar::new(
            hook_reg,
            trust,
            Arc::new(StdProcessLauncher::new()),
        ));
        let guard = SkillReloader::start(registry, activator, registrar, Some(tmp.clone()), None);
        assert!(guard.is_none(), "no roots -> None (no thread spawned)");
        drop(std::fs::remove_dir_all(&tmp));
    }

    /// is_skills_dir: a directory named "skills" is the dynamic-watch
    /// trigger; a file or a differently-named dir is not.
    #[test]
    fn test_is_skills_dir() {
        let tmp = std::env::temp_dir().join(format!("skill-isd-{}", std::process::id()));
        drop(std::fs::remove_dir_all(&tmp));
        std::fs::create_dir_all(tmp.join("skills")).unwrap();
        std::fs::create_dir_all(tmp.join("other")).unwrap();
        std::fs::write(tmp.join("SKILL.md"), "x").unwrap();
        assert!(is_skills_dir(&tmp.join("skills")));
        assert!(!is_skills_dir(&tmp.join("other")));
        assert!(
            !is_skills_dir(&tmp.join("SKILL.md")),
            "a file is not a skills dir"
        );
        drop(std::fs::remove_dir_all(&tmp));
    }

    /// file_size: Some(bytes) for an existing file, None for a missing one.
    #[test]
    fn test_file_size() {
        let tmp = std::env::temp_dir().join(format!("skill-fs-{}", std::process::id()));
        drop(std::fs::remove_dir_all(&tmp));
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("f"), "hello").unwrap();
        assert_eq!(file_size(&tmp.join("f")), Some(5));
        assert_eq!(file_size(&tmp.join("missing")), None);
        drop(std::fs::remove_dir_all(&tmp));
    }
}
