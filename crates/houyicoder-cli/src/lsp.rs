//! The houyi lsp subcommand: list what the scopes resolve to, add or remove
//! one entry, or launch the enabled servers with a real handshake.
//!
//! A CLI subcommand, not the TUI: no alternate screen is up, so print is the
//! correct destination for everything here. The doctor runs the same manager
//! a session runs, so its answer is the one a session would get, not a
//! lighter probe that agrees with nothing.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::env::current_dir;
use std::env::var;
use std::error::Error;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use houyicoder_api::launcher::StdProcessLauncher;
use houyicoder_config::frontend_config_dir;
use houyicoder_config::settings_path;
use houyicoder_config::trust::is_path_trusted;
use houyicoder_config::user_home_dir;
use houyicoder_config::{
    remove_project_lsp_server, remove_user_lsp_server, resolve_scoped_lsp_servers,
    upsert_project_lsp_server, upsert_user_lsp_server,
};
use houyicoder_protocol::lsp::{
    LspConfigScope, LspServerState, LspServerStatusView, ScopedLspServerConfig,
};
use houyicoder_service::lsp::{LspConnections, LspTimings};
use tokio::runtime::Builder;
use tokio::time::sleep;

use crate::cli_args::LspCommand;

/// How long the doctor waits for one server's handshake and indexing pass
/// before reporting it as unresponsive rather than failed.
const DOCTOR_DEADLINE: Duration = Duration::from_secs(20);

/// Run one lsp verb. Errors print from main's error path; everything the
/// user asked to see prints here.
pub(crate) fn run(action: LspCommand) -> Result<(), Box<dyn Error>> {
    let workspace = workspace();
    let settings = settings_path();
    match action {
        LspCommand::List => {
            let home = user_home_dir();
            let frontend = frontend_config_dir(home.as_deref());
            list(&settings, &workspace, home.as_deref(), frontend.as_deref())
        }
        LspCommand::Add {
            name,
            project,
            extensions,
            command,
        } => add(&name, project, &extensions, &command, &settings, &workspace),
        LspCommand::Remove { name, project } => remove(&name, project, &settings, &workspace),
        LspCommand::Doctor => {
            let home = user_home_dir();
            let frontend = frontend_config_dir(home.as_deref());
            doctor(&settings, &workspace, home.as_deref(), frontend.as_deref())
        }
    }
}

/// The workspace the subcommand acts on: the current directory. The session
/// resolves the same way, so the two never disagree about which project file
/// an entry lands in.
fn workspace() -> PathBuf {
    current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// Every server the scopes resolve to in one directory, with the same trust
/// answer and plugin directories a session reads.
fn resolved(
    settings: &Path,
    workspace: &Path,
    home: Option<&Path>,
    frontend: Option<&Path>,
) -> Vec<ScopedLspServerConfig> {
    let trusted = is_path_trusted(settings, workspace);
    let plugin_dir: Option<PathBuf> = home
        .as_ref()
        .map(|home| home.join(".houyicoder").join("plugins"));
    let lookup = |name: &str| var(name).ok();
    resolve_scoped_lsp_servers(
        settings,
        Some(workspace),
        trusted,
        plugin_dir.as_deref(),
        frontend,
        &lookup,
    )
}

/// Print every resolved server: name, scope, owned suffixes, and the state
/// it would start in. An empty resolution says so rather than printing
/// nothing.
fn list(
    settings: &Path,
    workspace: &Path,
    home: Option<&Path>,
    frontend: Option<&Path>,
) -> Result<(), Box<dyn Error>> {
    let servers = resolved(settings, workspace, home, frontend);
    if servers.is_empty() {
        println!("no language servers configured");
        return Ok(());
    }
    for server in servers {
        let state = if server.disabled {
            if server.locked {
                "locked (workspace not trusted)"
            } else {
                "disabled"
            }
        } else {
            "enabled"
        };
        let extensions = server
            .extensions()
            .iter()
            .map(|ext| format!(".{ext}"))
            .collect::<Vec<_>>()
            .join(",");
        println!(
            "{}  {}  {}  {}",
            server.name,
            scope_label(server.scope),
            extensions,
            state
        );
    }
    Ok(())
}

/// Where one add lands: the project file in one directory, or the user
/// settings. The printed line carries which scope the entry went to.
fn add(
    name: &str,
    project: bool,
    extensions: &[(String, String)],
    command: &[String],
    settings: &Path,
    workspace: &Path,
) -> Result<(), Box<dyn Error>> {
    let program = command.first().cloned().unwrap_or_default();
    if program.is_empty() {
        return Err("a language server needs a command".into());
    }
    if extensions.is_empty() {
        return Err("a language server needs at least one --ext <suffix>=<language>".into());
    }
    let mapping = extensions
        .iter()
        .cloned()
        .collect::<BTreeMap<String, String>>();
    let mut config = ScopedLspServerConfig::stdio(program, mapping);
    config.name = name.to_string();
    config.scope = if project {
        LspConfigScope::Project
    } else {
        LspConfigScope::User
    };
    config.args = command[1..].to_vec();
    if project {
        upsert_project_lsp_server(workspace, &config)?;
        println!("added {name} to the project file");
    } else {
        upsert_user_lsp_server(settings, &config)?;
        println!("added {name} to the user settings");
    }
    Ok(())
}

/// Remove one entry from the scope it lives in. A name the requested scope
/// does not hold is an error, not a silent success: a same-named entry from
/// another scope would otherwise make the command report a removal it never
/// made.
fn remove(
    name: &str,
    project: bool,
    settings: &Path,
    workspace: &Path,
) -> Result<(), Box<dyn Error>> {
    let home = user_home_dir();
    let frontend = frontend_config_dir(home.as_deref());
    let wanted = if project {
        LspConfigScope::Project
    } else {
        LspConfigScope::User
    };
    let held = resolved(settings, workspace, home.as_deref(), frontend.as_deref())
        .iter()
        .any(|server| server.name == name && server.scope == wanted);
    if !held {
        return Err(format!(
            "no language server named {name} is in the {} scope",
            scope_label(wanted)
        )
        .into());
    }
    if project {
        remove_project_lsp_server(workspace, name)?;
        println!("removed {name} from the project file");
    } else {
        remove_user_lsp_server(settings, name)?;
        println!("removed {name} from the user settings");
    }
    Ok(())
}

/// Launch each enabled server through the same manager a session runs and
/// report each outcome with the time to its report and its open-document
/// counts. A server that never settles inside the deadline is reported as
/// unresponsive rather than left spinning.
fn doctor(
    settings: &Path,
    workspace: &Path,
    home: Option<&Path>,
    frontend: Option<&Path>,
) -> Result<(), Box<dyn Error>> {
    let servers: Vec<_> = resolved(settings, workspace, home, frontend)
        .into_iter()
        .filter(|server| !server.disabled)
        .collect();
    if servers.is_empty() {
        println!("no enabled language servers to probe");
        return Ok(());
    }
    let root: Arc<Path> = Arc::from(workspace.to_path_buf());
    let runtime = Builder::new_multi_thread().enable_all().build()?;
    runtime.block_on(async move {
        let manager = Arc::new(LspConnections::new(
            Arc::new(StdProcessLauncher::new()),
            root,
            LspTimings::default(),
        ));
        let names: Vec<String> = servers.iter().map(|server| server.name.clone()).collect();
        manager.start_all(servers).await;
        // The doctor is an explicit ask to launch, so each server starts now
        // rather than on its first routed query. The launches run
        // concurrently, and the probe's clock starts after they settle: a
        // slow handshake during the launch phase is not the wait the deadline
        // is meant to bound. A launch failure is reported by the poll below,
        // with the server's stderr and the advice, so it is not printed here.
        let mut launches = Vec::with_capacity(names.len());
        for name in &names {
            let manager = Arc::clone(&manager);
            let name = name.clone();
            launches.push(tokio::spawn(async move {
                drop(manager.restart(&name).await);
            }));
        }
        for launch in launches {
            drop(launch.await);
        }
        let started = Instant::now();
        let deadline = started + DOCTOR_DEADLINE;
        let mut reported: BTreeSet<String> = BTreeSet::new();
        while reported.len() < names.len() && Instant::now() < deadline {
            for status in manager.statuses().await {
                if reported.contains(&status.name) {
                    continue;
                }
                if let Some(line) = settled_line(&status, started.elapsed()) {
                    println!("{line}");
                    reported.insert(status.name.clone());
                }
            }
            sleep(Duration::from_millis(50)).await;
        }
        for status in manager.statuses().await {
            if !reported.contains(&status.name) && !names.contains(&status.name) {
                continue;
            }
            if !reported.contains(&status.name) {
                println!(
                    "{}  unresponsive after {}s (still handshaking or indexing)",
                    status.name,
                    DOCTOR_DEADLINE.as_secs()
                );
            }
        }
        manager.shutdown().await;
    });
    Ok(())
}

/// One settled server's report line, or None while it is still starting or
/// indexing. A running server reports its timing and its counts; a failed
/// one reports its reason, the stderr tail, and the advice that fits.
fn settled_line(status: &LspServerStatusView, elapsed: Duration) -> Option<String> {
    // The poll loop observes the state, so this is the time to the report,
    // carrying up to one poll interval of jitter, not the handshake's own
    // duration.
    match &status.state {
        LspServerState::Running => Some(format!(
            "{}  running after {}  {} {}  {} {}",
            status.name,
            report_duration(elapsed),
            status.open_documents,
            count_noun(status.open_documents, "document"),
            status.diagnostic_count,
            count_noun(status.diagnostic_count, "diagnostic")
        )),
        LspServerState::Failed { error } => {
            let mut line = format!("{}  failed: {error}", status.name);
            for entry in &status.recent_stderr {
                line.push_str(&format!("\n  {entry}"));
            }
            line.push_str(
                "\n  fix: check the command, its arguments, and the extensions the entry \
                 routes",
            );
            Some(line)
        }
        _ => None,
    }
}

/// One count's noun in the number the count carries.
fn count_noun(count: usize, noun: &str) -> String {
    if count == 1 {
        noun.to_string()
    } else {
        format!("{noun}s")
    }
}

/// One elapsed span as a person reads it: milliseconds under a second,
/// seconds above, one decimal either way.
fn report_duration(elapsed: Duration) -> String {
    let millis = elapsed.as_millis();
    if millis < 1_000 {
        format!("{millis} ms")
    } else {
        format!("{:.1} s", elapsed.as_secs_f64())
    }
}

/// The scope's label as the list prints it.
fn scope_label(scope: LspConfigScope) -> &'static str {
    match scope {
        LspConfigScope::User => "user",
        LspConfigScope::Project => "project",
        LspConfigScope::Plugin => "plugin",
        LspConfigScope::ClaudePlugin => "plugin cache",
    }
}

#[cfg(test)]
mod tests;
