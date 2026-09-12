//! Reloads skill definitions after stable filesystem changes.
//!
//! The session guard owns and stops the watcher thread.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use houyicoder_api::skill::SkillRegistry;
use houyicoder_core::agent::{ConditionalSkillActivator, SkillHookRegistrar, SkillReloadGuard};
use houyicoder_skill::lifecycle::{Action, ReloadScheduler, WatchDepth};
#[cfg(target_os = "macos")]
use notify::PollWatcher as PlatformWatcher;
#[cfg(not(target_os = "macos"))]
use notify::RecommendedWatcher as PlatformWatcher;
use notify::{RecursiveMode, Watcher};

use super::skill::SkillRegistryImpl;

/// Maximum idle wait between scheduler checks.
const LOOP_POLL: Duration = Duration::from_millis(100);

/// Collaborators retained by the reload thread.
struct ReloadDeps {
    registry: std::sync::Arc<SkillRegistryImpl>,
    activator: std::sync::Arc<dyn ConditionalSkillActivator>,
    registrar: std::sync::Arc<SkillHookRegistrar>,
    cwd: Option<PathBuf>,
    home: Option<PathBuf>,
}

impl ReloadDeps {
    /// Refresh definitions and dependent runtime state.
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

/// Session-scoped skill reload guard.
pub struct SkillReloader {
    _thread: Option<JoinHandle<()>>,
    shutdown: Option<mpsc::Sender<()>>,
}

impl SkillReloadGuard for SkillReloader {}

impl SkillReloader {
    /// Start reloading existing skill roots. Returns None when no roots exist.
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
            .spawn(move || start_watcher(roots, shutdown_rx, deps))
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
        self.shutdown.take();
        if let Some(thread) = self._thread.take() {
            drop(thread.join());
        }
    }
}

fn start_watcher(roots: Vec<(PathBuf, WatchDepth)>, shutdown_rx: Receiver<()>, deps: ReloadDeps) {
    let (event_tx, event_rx) = mpsc::channel::<Vec<PathBuf>>();
    let config = notify::Config::default().with_poll_interval(Duration::from_millis(500));
    let mut watcher = match PlatformWatcher::new(
        move |res: Result<notify::Event, _>| {
            if let Ok(event) = res {
                drop(event_tx.send(event.paths));
            }
        },
        config,
    ) {
        Ok(watcher) => watcher,
        Err(error) => {
            tracing::warn!(%error, "skill watcher init failed; hot-reload off");
            return;
        }
    };
    for (path, depth) in &roots {
        let mode = match depth {
            WatchDepth::Deep => RecursiveMode::Recursive,
            WatchDepth::Shallow => RecursiveMode::NonRecursive,
        };
        if let Err(error) = watcher.watch(path, mode) {
            tracing::warn!(path = %path.display(), %error, "watch add failed; skipping root");
        }
    }
    run_loop(watcher, event_rx, shutdown_rx, deps);
}

/// Process filesystem events until shutdown.
fn run_loop(
    mut watcher: PlatformWatcher,
    event_rx: Receiver<Vec<PathBuf>>,
    shutdown_rx: Receiver<()>,
    deps: ReloadDeps,
) {
    let mut scheduler = ReloadScheduler::new();
    loop {
        match event_rx.recv_timeout(LOOP_POLL) {
            Ok(paths) => route_events(&paths, &mut scheduler, &mut watcher, &deps),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        if !matches!(shutdown_rx.try_recv(), Err(mpsc::TryRecvError::Empty)) {
            return;
        }
        let now = Instant::now();
        while let Some(action) = scheduler.poll(now) {
            match action {
                Action::CheckStability(paths) => {
                    let Some(stable) = files_settled(&paths, &shutdown_rx) else {
                        return;
                    };
                    scheduler.confirm_stable(now, stable);
                }
                Action::Reload => deps.reload(),
            }
        }
    }
}

/// Route relevant filesystem changes into the reload scheduler.
fn route_events(
    paths: &[PathBuf],
    scheduler: &mut ReloadScheduler,
    watcher: &mut PlatformWatcher,
    deps: &ReloadDeps,
) {
    let now = Instant::now();
    for path in paths {
        if path
            .components()
            .any(|c| c.as_os_str() == ".git" || c.as_os_str() == "node_modules")
        {
            continue;
        }
        if is_skills_dir(path) {
            if watcher.watch(path, RecursiveMode::Recursive).is_err() {
                tracing::warn!(path = %path.display(), "dynamic skills/ watch failed");
            }
            // Cover files created before the recursive watch became active.
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

/// Wait until pending files stabilize or shutdown begins.
fn files_settled(paths: &[PathBuf], shutdown_rx: &Receiver<()>) -> Option<bool> {
    let mut last: Vec<Option<u64>> = paths.iter().map(|p| file_size(p.as_path())).collect();
    let poll = Duration::from_millis(500);
    let window = Duration::from_secs(1);
    let start = Instant::now();
    loop {
        match shutdown_rx.recv_timeout(poll) {
            Err(RecvTimeoutError::Timeout) => {}
            Ok(()) | Err(RecvTimeoutError::Disconnected) => return None,
        }
        let mut changed = false;
        for (i, p) in paths.iter().enumerate() {
            let s = file_size(p);
            if s != last[i] {
                last[i] = s;
                changed = true;
            }
        }
        if !changed && Instant::now().duration_since(start) >= window {
            return Some(true);
        }
        if Instant::now().duration_since(start) >= Duration::from_secs(30) {
            return Some(false);
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

    #[test]
    fn test_drop_stops_watcher() {
        let tmp = std::env::temp_dir().join(format!("skill-reload-drop-{}", std::process::id()));
        drop(std::fs::remove_dir_all(&tmp));
        write_skill(&tmp, "alpha");
        let deps = deps_with(&tmp);
        let guard = SkillReloader::start(
            deps.registry,
            deps.activator,
            deps.registrar,
            deps.cwd,
            deps.home,
        )
        .expect("watch roots exist");
        let started = Instant::now();
        drop(guard);
        assert!(started.elapsed() < Duration::from_secs(2));
        drop(std::fs::remove_dir_all(&tmp));
    }

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
