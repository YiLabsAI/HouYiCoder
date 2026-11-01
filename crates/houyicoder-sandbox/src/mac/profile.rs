//! Derivation of the Seatbelt profile and the path admission checks that
//! decide what the fence will let a command reach.

use std::env::var;
use std::path::{Path, PathBuf};

use houyicoder_api::sandbox::normalize_tool_path;
use houyicoder_context::SandboxError;

use crate::{ProfileSpec, render};

use super::MacSeatbeltSession;

impl MacSeatbeltSession {
    /// The seatbelt profile string for the next exec: the pre-rendered
    /// empty-additional profile when no dirs are added AND no narrow is
    /// active, or a fresh render that includes the runtime-added dirs in the
    /// allow-back. When narrowed, the profile binds the WORKTREE (not the
    /// original workspace) + appends the .git/config read-allow (git needs
    /// repo config; the write deny still holds). Split out of exec so the
    /// re-derivation is unit-testable without spawning sandbox-exec. Each
    /// sandbox-exec is a fresh process, so mutating the profile between execs
    /// applies the new dirs (a running sandbox cannot be relaxed, but none is
    /// running between execs).
    pub(super) fn current_profile(&self) -> String {
        let read_only = self.dirs.read_only();
        let read_write = self.dirs.read_write();
        let mach = self
            .extra_mach_services
            .lock()
            .expect("extra mach services lock")
            .clone();
        let allow_app_launch = *self.allow_app_launch.lock().expect("allow app launch lock");
        let narrow_ws = self
            .narrow_workspace
            .lock()
            .expect("narrow workspace lock")
            .clone();
        let narrow_git = self
            .narrow_git_common
            .lock()
            .expect("narrow git common lock")
            .clone();
        if read_only.is_empty()
            && read_write.is_empty()
            && narrow_ws.is_none()
            && mach.is_empty()
            && !allow_app_launch
        {
            return self.profile.clone();
        }
        let home = var("HOME").unwrap_or_else(|_| "/Users/unknown".into());
        let read_refs: Vec<&str> = read_only
            .iter()
            .map(|path| path.to_str().unwrap_or(""))
            .collect();
        let write_refs: Vec<&str> = read_write
            .iter()
            .map(|path| path.to_str().unwrap_or(""))
            .collect();
        let mach_refs: Vec<&str> = mach.iter().map(String::as_str).collect();
        // When narrowed, the profile binds the worktree; else the workspace.
        let ws = narrow_ws.as_ref().unwrap_or(&self.workspace);
        let mut p = render(
            &ProfileSpec::new(ws, &self.tmpdir.to_string_lossy(), &home, &self.tag)
                .with_additional(&write_refs)
                .with_read_only(&read_refs)
                .with_mach_services(&mach_refs)
                .with_app_launch(allow_app_launch)
                .with_network(self.network.clone()),
        );
        if let Some(git_common) = narrow_git.as_ref() {
            // Amendment: read-allow .git/config (git needs repo config) +
            // the worktree metadata dir. Appended AFTER mandatory_deny so
            // last-match-wins re-allows the read; the write deny still holds
            // (blocks credential-helper insertion; the network fence blocks
            // exfil even if config pointed at a helper).
            p.push_str(&format!(
                "(allow file-read* (literal \"{}\") (subpath \"{}\"))\n",
                git_common.join("config").to_string_lossy(),
                git_common.join("worktrees").to_string_lossy(),
            ));
        }
        p
    }

    pub(super) fn resolve_path(
        &self,
        path: &str,
        include_read_only: bool,
    ) -> Result<PathBuf, SandboxError> {
        let workspace = self.effective_workspace();
        let supplied = Path::new(path);
        let base = if supplied.is_absolute() {
            supplied.to_path_buf()
        } else {
            workspace.join(path)
        };
        let canonical = normalize_tool_path(&base).unwrap_or(base);
        if canonical.starts_with(&workspace) || canonical.starts_with(&self.tmpdir) {
            return Ok(canonical);
        }
        if self.dirs.allows_write(&canonical) {
            return Ok(canonical);
        }
        if include_read_only && self.dirs.allows_read(&canonical) {
            return Ok(canonical);
        }
        Err(SandboxError::PathTraversal(format!(
            "path escapes workspace + authorized dirs: {path}"
        )))
    }

    /// The current effective workspace root: the narrow worktree when
    /// narrowed, else the original workspace.
    pub(super) fn effective_workspace(&self) -> PathBuf {
        self.narrow_workspace
            .lock()
            .expect("narrow workspace lock")
            .clone()
            .unwrap_or_else(|| self.workspace.clone())
    }
}
