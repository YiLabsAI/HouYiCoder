//! Applies the permission pane's writable-directory replies. The host answers
//! with the whole list, so the difference against the list the pane already
//! held is what says which directory a gesture acted on.

use crate::state::App;

impl App {
    /// Take the host's writable-directory list and report what changed.
    ///
    /// The list arrives as a whole, so the difference against the previous
    /// one is the only place the pane learns which directory the user's
    /// gesture acted on. Reporting from this side is what lets the line name
    /// a path: the request that started it carried one, but a request is not
    /// an outcome, and a line written before the host answers claims an
    /// effect that may never happen.
    pub(crate) fn apply_permission_dirs(&mut self, dirs: Vec<String>) {
        let permission = &mut self.sessions.current.capabilities.permission;
        let added: Vec<String> = dirs
            .iter()
            .filter(|dir| !permission.dirs_cache.contains(dir))
            .cloned()
            .collect();
        let removed: Vec<String> = permission
            .dirs_cache
            .iter()
            .filter(|dir| !dirs.contains(dir))
            .cloned()
            .collect();
        permission.dirs_cache = dirs;
        for dir in added {
            self.system_line(format!("permission: added writable directory {dir}"));
        }
        for dir in removed {
            self.system_line(format!("permission: removed writable directory {dir}"));
        }
    }
}
