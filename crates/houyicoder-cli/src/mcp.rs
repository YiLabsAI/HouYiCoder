//! The houyi mcp subcommand: list what the scopes resolve to, add or remove
//! one entry, or probe the enabled servers with a real handshake.
//!
//! A CLI subcommand, not the TUI: no alternate screen is up, so print is the
//! correct destination for everything here. The doctor runs the same manager
//! a session runs, so its answer is the one a session would get, not a
//! lighter probe that agrees with nothing.

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
use houyicoder_config::settings_path;
use houyicoder_config::trust::is_path_trusted;
use houyicoder_config::{
    ENV_HOUYICODER_MCP_SERVERS, remove_project_server, remove_user_server,
    resolve_scoped_mcp_servers, upsert_project_server, upsert_user_server,
};
use houyicoder_protocol::mcp::{
    McpConfigScope, McpConnectionState, McpServerStatusView, McpTransportConfig, McpTransportKind,
    ScopedMcpServerConfig,
};
use houyicoder_service::mcp::{McpConnections, McpTimings};
use tokio::runtime::Builder;
use tokio::time::sleep;

use crate::cli_args::McpCommand;

/// How long the doctor waits for one server's handshake before reporting it
/// as unresponsive rather than failed.
const DOCTOR_DEADLINE: Duration = Duration::from_secs(10);

/// Run one mcp verb. Errors print from main's error path; everything the
/// user asked to see prints here.
pub(crate) fn run(action: McpCommand) -> Result<(), Box<dyn Error>> {
    let workspace = workspace();
    let settings = settings_path();
    let env_raw = var(ENV_HOUYICODER_MCP_SERVERS).ok();
    match action {
        McpCommand::List => list(&settings, &workspace, env_raw.as_deref()),
        McpCommand::Add {
            name,
            project,
            url,
            command,
        } => add(&name, project, url, &command, &settings, &workspace),
        McpCommand::Remove { name, project } => remove(&name, project, &settings, &workspace),
        McpCommand::Doctor => doctor(&settings, &workspace, env_raw.as_deref()),
    }
}

/// The workspace the subcommand acts on: the current directory. The session
/// resolves the same way, so the two never disagree about which project file
/// an entry lands in.
fn workspace() -> PathBuf {
    current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// Every server the scopes resolve to in one directory, with the same trust
/// answer and environment spelling a session uses.
fn resolved(
    settings: &Path,
    workspace: &Path,
    env_raw: Option<&str>,
) -> Vec<ScopedMcpServerConfig> {
    let trusted = is_path_trusted(settings, workspace);
    let lookup = |name: &str| var(name).ok();
    resolve_scoped_mcp_servers(settings, Some(workspace), env_raw, trusted, &lookup)
}

/// One transport spelling from the add operands: a URL names the HTTP
/// transport, a command names the stdio one. An empty command yields an
/// empty endpoint, which the caller rejects before writing anything.
fn build_transport(url: Option<String>, command: &[String]) -> McpTransportConfig {
    match url {
        Some(endpoint) => McpTransportConfig::http(endpoint),
        None => {
            let program = command.first().cloned().unwrap_or_default();
            let arguments = command.iter().skip(1).cloned().collect();
            McpTransportConfig::stdio(program, arguments)
        }
    }
}

/// Print every resolved server: name, scope, transport, and the state it
/// would start in. An empty resolution says so rather than printing nothing.
fn list(settings: &Path, workspace: &Path, env_raw: Option<&str>) -> Result<(), Box<dyn Error>> {
    let servers = resolved(settings, workspace, env_raw);
    if servers.is_empty() {
        println!("no external tool servers configured");
        return Ok(());
    }
    let trusted = is_path_trusted(settings, workspace);
    for server in servers {
        let state = if server.disabled {
            if server.scope == McpConfigScope::Project && !trusted {
                "locked (workspace not trusted)"
            } else {
                "disabled"
            }
        } else {
            "enabled"
        };
        println!(
            "{}  {}  {}  {}",
            server.name,
            scope_label(server.scope),
            transport_label(&server.transport),
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
    url: Option<String>,
    command: &[String],
    settings: &Path,
    workspace: &Path,
) -> Result<(), Box<dyn Error>> {
    let transport = build_transport(url, command);
    if transport.endpoint.is_empty() {
        return Err("a server needs a command or an endpoint".into());
    }
    if project {
        upsert_project_server(workspace, name, &transport)?;
        println!("added {name} to the project file");
    } else {
        upsert_user_server(settings, name, &transport)?;
        println!("added {name} to the user settings");
    }
    Ok(())
}

/// Remove one entry from the scope it lives in. A name the scope does not
/// hold is an error, not a silent success: the user asked for a specific
/// entry and deserves to hear it was not there.
fn remove(
    name: &str,
    project: bool,
    settings: &Path,
    workspace: &Path,
) -> Result<(), Box<dyn Error>> {
    if project {
        let held = houyicoder_config::parse_project_servers(workspace)
            .iter()
            .any(|s| s.name == name);
        if !held {
            return Err(format!("no server named {name} is in the project file").into());
        }
        remove_project_server(workspace, name)?;
        println!("removed {name} from the project file");
    } else {
        let held = houyicoder_config::parse_settings_servers(settings)
            .iter()
            .any(|s| s.name == name);
        if !held {
            return Err(format!("no server named {name} is in the user settings").into());
        }
        remove_user_server(settings, name)?;
        println!("removed {name} from the user settings");
    }
    Ok(())
}

/// Probe the enabled servers with the same manager a session runs and
/// report each outcome with the time to its report, its counts, and its
/// diagnostics. A server that never settles inside the deadline is
/// reported as unresponsive rather than left spinning.
fn doctor(settings: &Path, workspace: &Path, env_raw: Option<&str>) -> Result<(), Box<dyn Error>> {
    let servers: Vec<_> = resolved(settings, workspace, env_raw)
        .into_iter()
        .filter(|s| !s.disabled)
        .collect();
    if servers.is_empty() {
        println!("no enabled external tool servers to probe");
        return Ok(());
    }
    let trusted = is_path_trusted(settings, workspace);
    let root: Arc<Path> = Arc::from(workspace.to_path_buf());
    let runtime = Builder::new_multi_thread().enable_all().build()?;
    runtime.block_on(async move {
        let manager = McpConnections::new(
            Arc::new(StdProcessLauncher::new()),
            root,
            McpTimings::default(),
        );
        manager.start_all(servers, trusted).await;
        let total = manager.statuses().await.len();
        let started = Instant::now();
        let deadline = started + DOCTOR_DEADLINE;
        let mut reported: BTreeSet<String> = BTreeSet::new();
        while reported.len() < total && Instant::now() < deadline {
            for status in manager.statuses().await {
                if reported.contains(&status.name) {
                    continue;
                }
                if let Some(line) = settled_line(&manager, &status, started).await {
                    println!("{line}");
                    reported.insert(status.name.clone());
                }
            }
            sleep(Duration::from_millis(50)).await;
        }
        for status in manager.statuses().await {
            if !reported.contains(&status.name) {
                println!(
                    "{}  unresponsive after {}s (still handshaking or wedged)",
                    status.name,
                    DOCTOR_DEADLINE.as_secs()
                );
            }
        }
        manager.shutdown().await;
    });
    Ok(())
}

/// One settled server's report line, or None while it is still pending. A
/// connected server reports its timing, the roots negotiation the handshake
/// completed, and its counts; a failed one reports its reason and the
/// diagnostics it wrote, with the advice that fits each class of failure.
async fn settled_line(
    manager: &McpConnections,
    status: &McpServerStatusView,
    started: Instant,
) -> Option<String> {
    // The poll loop observes the state, so this is the time to the report,
    // carrying up to one poll interval of jitter, not the handshake's own
    // duration.
    let elapsed = started.elapsed();
    match &status.state {
        McpConnectionState::Connected => Some(connected_line(
            &status.name,
            elapsed,
            status.tool_count,
            status.resource_count,
            manager.roots_served(&status.name).await,
        )),
        McpConnectionState::NeedsAuth { reason } => Some(format!(
            "{}  needs auth: {reason}\n  fix: the endpoint rejected the credentials; \
             check the headers and the header helper in the entry",
            status.name
        )),
        McpConnectionState::Failed { error } => {
            let stderr = manager.stderr_tail(&status.name).await;
            let mut line = format!("{}  failed: {error}", status.name);
            for entry in &stderr {
                line.push_str(&format!("\n  {entry}"));
            }
            line.push_str(
                "\n  fix: check the command or endpoint, the arguments, and any variable \
                 references the entry carries",
            );
            Some(line)
        }
        _ => None,
    }
}

/// One connected server's report line. The roots phrase follows what the
/// connection observed — served or never requested — so the diagnosis never
/// claims a negotiation the server did not make.
fn connected_line(
    name: &str,
    elapsed: Duration,
    tools: usize,
    resources: usize,
    roots_served: bool,
) -> String {
    let roots = if roots_served {
        "roots served"
    } else {
        "roots not requested"
    };
    format!(
        "{name}  connected after {}  {roots}  {tools} tools  {resources} resources",
        report_duration(elapsed)
    )
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
fn scope_label(scope: McpConfigScope) -> &'static str {
    match scope {
        McpConfigScope::User => "user",
        McpConfigScope::Project => "project",
        McpConfigScope::Env => "env",
    }
}

/// The transport's one-word kind as the list prints it.
fn transport_label(transport: &McpTransportConfig) -> &'static str {
    match transport.kind {
        McpTransportKind::Stdio => "stdio",
        McpTransportKind::Http => "http",
    }
}

#[cfg(test)]
mod tests;
