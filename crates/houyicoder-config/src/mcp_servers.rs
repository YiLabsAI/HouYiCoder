//! MCP server configuration: the three scopes a server can come from, the
//! merge order between them, variable expansion inside their values, and the
//! atomic writes that keep a caller's edits from clobbering a hand-edited
//! file.
//!
//! Precedence is user over project over env: a server the user configured
//! wins over one a checked-in file names, which in turn wins over one an
//! environment variable injects. A project-scope server whose transport is a
//! child process stays disabled until the workspace is trusted, so opening a
//! cloned repository cannot start a process the user never approved.

use std::collections::BTreeMap;
use std::fs::read_to_string;
use std::path::Path;

use houyicoder_protocol::mcp::{
    McpConfigScope, McpTransportConfig, McpTransportKind, ScopedMcpServerConfig,
};
use serde_json::Map;
use serde_json::Value;

use crate::SettingsWriteError;
use crate::read_settings_value;
use crate::update_settings;

/// The filename of the project-scope server list, at the workspace root.
pub const PROJECT_FILE: &str = ".mcp.json";

/// The settings key that holds the user-scope server list.
pub const SETTINGS_KEY: &str = "mcpServers";

/// Read every scope's servers, merge them by precedence, gate them on
/// workspace trust, and expand the variable references in the entries that
/// will actually run.
///
/// Precedence is user over project over env. Order is by server name, so two
/// reads of the same inputs expose the same sequence. An untrusted workspace
/// disables every project-scope entry before expansion, so the host's secrets
/// are never even resolved into a body the renderer could show or a
/// handshake could send: an HTTP entry with a key reference would otherwise
/// leak it to the endpoint the repository names.
pub fn resolve_scoped_mcp_servers(
    settings_path: &Path,
    workspace: Option<&Path>,
    env_raw: Option<&str>,
    trusted: bool,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> Vec<ScopedMcpServerConfig> {
    let mut by_name: BTreeMap<String, ScopedMcpServerConfig> = BTreeMap::new();
    for (scope, entries) in [
        (McpConfigScope::Env, parse_env_servers(env_raw)),
        (
            McpConfigScope::Project,
            workspace.map(parse_project_servers).unwrap_or_default(),
        ),
        (McpConfigScope::User, parse_settings_servers(settings_path)),
    ] {
        for mut server in entries {
            server.scope = scope;
            by_name.insert(server.name.clone(), server);
        }
    }
    apply_trust_gate(by_name.into_values().collect(), trusted)
        .into_iter()
        .map(|mut server| {
            if !server.disabled {
                expand_entry(&mut server, lookup);
            }
            server
        })
        .collect()
}

/// Expand every variable reference in one enabled entry: the address, each
/// argument, each environment value, and each header value.
fn expand_entry(server: &mut ScopedMcpServerConfig, lookup: &dyn Fn(&str) -> Option<String>) {
    server.transport.endpoint = expand_references(&server.transport.endpoint, lookup);
    for argument in &mut server.transport.arguments {
        *argument = expand_references(argument, lookup);
    }
    for value in server.transport.environment.values_mut() {
        *value = expand_references(value, lookup);
    }
    for value in server.transport.headers.values_mut() {
        *value = expand_references(value, lookup);
    }
}

/// Read one scope's servers from the user settings file. The key is a map
/// from server name to its body, the same shape the standard config file
/// uses. A missing file or key yields nothing.
pub fn parse_settings_servers(path: &Path) -> Vec<ScopedMcpServerConfig> {
    let value = read_settings_value(path);
    let Some(map) = value.get(SETTINGS_KEY).and_then(Value::as_object) else {
        return Vec::new();
    };
    map.iter().filter_map(parse_server_entry).collect()
}

/// Read the project-scope servers from the workspace's mcp.json. A missing
/// or malformed file yields nothing: a broken file must not brick the
/// session, and the warning that names it goes to the log.
pub fn parse_project_servers(workspace: &Path) -> Vec<ScopedMcpServerConfig> {
    let path = workspace.join(PROJECT_FILE);
    let text = match read_to_string(&path) {
        Ok(text) => text,
        Err(_) => return Vec::new(),
    };
    let value: Value = match serde_json::from_str(&text) {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(
                file = %path.display(),
                %error,
                "the project mcp file is not valid json; no project server wired"
            );
            return Vec::new();
        }
    };
    let Some(map) = value.get(SETTINGS_KEY).and_then(Value::as_object) else {
        return Vec::new();
    };
    map.iter().filter_map(parse_server_entry).collect()
}

/// Parse the env spelling: a JSON array of objects, each naming its own
/// server under a name field, or under the file stem of the program it
/// names when the older spelling leaves the name out.
pub fn parse_env_servers(raw: Option<&str>) -> Vec<ScopedMcpServerConfig> {
    let Some(body) = raw.map(str::trim).filter(|text| !text.is_empty()) else {
        return Vec::new();
    };
    let values: Vec<Value> = match serde_json::from_str(body) {
        Ok(values) => values,
        Err(error) => {
            tracing::warn!(%error, "the mcp server env var is not valid json; no env server wired");
            return Vec::new();
        }
    };
    // The env array names each server in its own body, so an entry reads
    // through the same parser a map entry does, keyed by its name field.
    values
        .iter()
        .filter_map(|body| {
            // The older spelling named no server, only a program; the
            // program's file stem is the name it was reachable under.
            let name = body
                .get("name")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| {
                    body.get("program")
                        .and_then(Value::as_str)
                        .and_then(|program| {
                            Path::new(program)
                                .file_stem()
                                .map(|stem| stem.to_string_lossy().to_string())
                        })
                })?;
            parse_server_entry((&name, body))
        })
        .collect()
}

/// One entry of a server map: the key is the name, the value the transport
/// body. An entry that fails to parse is skipped with a warning naming it,
/// so one malformed server cannot hide the rest.
fn parse_server_entry((name, body): (&String, &Value)) -> Option<ScopedMcpServerConfig> {
    let transport: McpTransportConfig = match serde_json::from_value(body.clone()) {
        Ok(transport) => transport,
        Err(error) => {
            tracing::warn!(server = %name, %error, "the mcp server entry is malformed; skipped");
            return None;
        }
    };
    if transport.endpoint.is_empty() {
        tracing::warn!(server = %name, "the mcp server entry names no command or url; skipped");
        return None;
    }
    Some(ScopedMcpServerConfig {
        name: name.clone(),
        scope: McpConfigScope::Env,
        transport,
        disabled: body
            .get("disabled")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        tool_timeout_ms: body
            .get("toolTimeoutMs")
            .and_then(Value::as_u64)
            .unwrap_or(60_000),
    })
}

/// Expand the variable spellings a server body allows: literal values, and
/// the two reference forms a settings file uses, which resolve against the
/// process environment. A reference to an unset variable without a default
/// expands to nothing, matching the settings-file convention, so a stale
/// reference never reaches the child as its own text.
pub fn expand_references(value: &str, lookup: &dyn Fn(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(open) = rest.find("${") {
        out.push_str(&rest[..open]);
        let after = &rest[open + 2..];
        let Some(close) = after.find('}') else {
            out.push_str(&rest[open..]);
            return out;
        };
        let reference = &after[..close];
        // Split on the first dash-colon, which is the standard default
        // separator: the part before is the variable name, the part after
        // is the fallback when the variable is unset.
        let (name, default) = match reference.split_once(":-") {
            Some((name, default)) => (name, Some(default)),
            None => (reference, None),
        };
        let resolved = match default {
            Some(default) => lookup(name).unwrap_or_else(|| default.to_string()),
            None => lookup(name).unwrap_or_default(),
        };
        out.push_str(&resolved);
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    out
}

/// Mark every project-scope server disabled when the workspace is not
/// trusted, whatever its transport. A cloned repository must not choose the
/// endpoints this host talks to: an HTTP entry can name an attacker's
/// address, carry a header referencing a host secret, and return
/// instructions that steer the model, which a stdio-only gate would let
/// through.
pub fn apply_trust_gate(
    servers: Vec<ScopedMcpServerConfig>,
    trusted: bool,
) -> Vec<ScopedMcpServerConfig> {
    if trusted {
        return servers;
    }
    servers
        .into_iter()
        .map(|mut server| {
            if server.scope == McpConfigScope::Project {
                server.disabled = true;
            }
            server
        })
        .collect()
}

/// Remove one server from the user settings file. A server the caller wants
/// gone stays gone across reloads.
pub fn remove_user_server(path: &Path, name: &str) -> Result<(), SettingsWriteError> {
    update_settings(path, |value| remove_named_server(value, name), 8)
}

/// Upsert one server into the user settings file. The entry is written in
/// the standard file shape so a hand-edit and a programmatic write agree.
pub fn upsert_user_server(
    path: &Path,
    name: &str,
    transport: &McpTransportConfig,
) -> Result<(), SettingsWriteError> {
    let body = transport_body(transport);
    update_settings(
        path,
        |value| {
            let Some(obj) = value.as_object_mut() else {
                return;
            };
            let servers = obj
                .entry(SETTINGS_KEY)
                .or_insert_with(|| Value::Object(Map::new()));
            if let Some(map) = servers.as_object_mut() {
                map.insert(name.to_string(), body.clone());
            }
        },
        8,
    )
}

/// Record one named user server's disabled flag, so a toggle made from the
/// /mcp pane survives the next session. An entry the file does not hold is
/// an error rather than a silent no-write: the caller told the user the
/// toggle applied, so the file and the session must be shown to disagree.
pub fn set_user_server_disabled(
    path: &Path,
    name: &str,
    disabled: bool,
) -> Result<(), SettingsWriteError> {
    update_settings(
        path,
        |value| {
            if let Some(entry) = value
                .get_mut(SETTINGS_KEY)
                .and_then(Value::as_object_mut)
                .and_then(|map| map.get_mut(name))
                .and_then(Value::as_object_mut)
            {
                entry.insert("disabled".into(), Value::Bool(disabled));
            }
        },
        8,
    )?;
    // The mutator runs under CAS retries, so whether it changed anything is
    // read back from the file the write produced rather than tracked in a
    // closure the signature cannot carry.
    let applied = crate::settings_merge::read_settings_value(path)
        .get(SETTINGS_KEY)
        .and_then(Value::as_object)
        .and_then(|map| map.get(name))
        .and_then(Value::as_object)
        .and_then(|entry| entry.get("disabled"))
        == Some(&Value::Bool(disabled));
    if applied {
        Ok(())
    } else {
        Err(SettingsWriteError::Io(format!(
            "no server named {name} is in the settings file"
        )))
    }
}

/// Serialize one transport in the standard file shape.
fn transport_body(transport: &McpTransportConfig) -> Value {
    let mut map = Map::new();
    match transport.kind {
        McpTransportKind::Stdio => {
            map.insert("command".into(), Value::String(transport.endpoint.clone()));
            if !transport.arguments.is_empty() {
                map.insert(
                    "args".into(),
                    Value::Array(
                        transport
                            .arguments
                            .iter()
                            .map(|arg| Value::String(arg.clone()))
                            .collect(),
                    ),
                );
            }
            if !transport.environment.is_empty() {
                map.insert(
                    "env".into(),
                    serde_json::to_value(&transport.environment).unwrap(),
                );
            }
        }
        McpTransportKind::Http => {
            // The explicit kind keeps a written HTTP entry readable by a
            // reader that only checks the type field, and keeps a URL-only
            // body from being re-read under the wrong transport.
            map.insert("type".into(), Value::String("http".into()));
            map.insert("url".into(), Value::String(transport.endpoint.clone()));
            if !transport.headers.is_empty() {
                map.insert(
                    "headers".into(),
                    serde_json::to_value(&transport.headers).unwrap(),
                );
            }
            if let Some(helper) = &transport.headers_helper {
                map.insert("headersHelper".into(), Value::String(helper.clone()));
            }
        }
    }
    Value::Object(map)
}

/// Drop one named server from a settings value. When the last server leaves,
/// the empty container is removed too, so a file with no servers reads as
/// the object it was before any was added.
fn remove_named_server(value: &mut Value, name: &str) {
    let Some(obj) = value.as_object_mut() else {
        return;
    };
    let Some(servers) = obj.get_mut(SETTINGS_KEY) else {
        return;
    };
    if let Some(map) = servers.as_object_mut() {
        map.remove(name);
        if map.is_empty() {
            obj.remove(SETTINGS_KEY);
        }
    }
}

#[cfg(test)]
mod tests;
