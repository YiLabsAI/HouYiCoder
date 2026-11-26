//! Wiring the language servers into one session: resolve the four
//! configuration scopes, register the servers, and hand back the manager
//! the tools address. Nothing is launched here; the first routed query
//! starts a server.

use std::env;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use futures::executor::block_on;
use houyicoder_api::launcher::StdProcessLauncher;
use houyicoder_config::plugin_dir;
use houyicoder_config::resolve_scoped_lsp_servers;
use houyicoder_config::session_frontend_dir;
use houyicoder_config::settings_path;
use houyicoder_config::settings_path_in;
use houyicoder_config::trust::is_path_trusted;
use houyicoder_protocol::lsp::LspTransportKind;

use crate::lsp::LspConnections;
use crate::lsp::LspTimings;

/// Resolve and register the language servers one session may use. None when
/// the scopes resolve to nothing routable, so a session with no language
/// servers registers no language tool and pays no prompt tokens for it.
pub(super) fn start(home: Option<&Path>, workspace: Option<&Path>) -> Option<Arc<LspConnections>> {
    let settings_file = match home {
        Some(home) => settings_path_in(home),
        None => settings_path(),
    };
    start_with(
        workspace,
        SessionDirs {
            settings_file,
            plugin_dir: Some(plugin_dir(home)),
            frontend_dir: session_frontend_dir(home),
        },
    )
}

/// The directories one resolution reads, separated from the environment so a
/// test can drive the production shape without touching the developer's
/// home. The settings file is included so a production-shaped call (no home)
/// still reads the fixture's settings instead of the process config home.
struct SessionDirs {
    settings_file: PathBuf,
    plugin_dir: Option<PathBuf>,
    frontend_dir: Option<PathBuf>,
}

fn start_with(workspace: Option<&Path>, dirs: SessionDirs) -> Option<Arc<LspConnections>> {
    let settings_file = dirs.settings_file;
    let trusted = workspace.is_none_or(|workspace| is_path_trusted(&settings_file, workspace));
    let lookup = |name: &str| env::var(name).ok();
    let servers = resolve_scoped_lsp_servers(
        &settings_file,
        workspace,
        trusted,
        dirs.plugin_dir.as_deref(),
        dirs.frontend_dir.as_deref(),
        &lookup,
    );
    // A disabled entry routes nothing, an entry naming no suffix is invisible
    // to the session, and this host launches no socket entry, so only an
    // enabled stdio entry with a route justifies a language tool.
    if !servers.iter().any(|server| {
        !server.disabled
            && server.transport == LspTransportKind::Stdio
            && !server.extension_to_language.is_empty()
    }) {
        return None;
    }
    let root: Arc<Path> = Arc::from(
        workspace
            .map(Path::to_path_buf)
            .or_else(|| env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from(".")),
    );
    let manager = Arc::new(LspConnections::new(
        Arc::new(StdProcessLauncher::new()),
        root,
        LspTimings::default(),
    ));
    // Registration only records the slots, and it must finish before the call
    // returns: the tool registers against the routes those slots answer, so a
    // tool that existed before its slots would answer every query with "no
    // server routes this file". The locks are uncontended at this point, so
    // the block is the registration itself, not a wait.
    block_on(manager.start_all(servers));
    Some(manager)
}

#[cfg(test)]
mod tests;
