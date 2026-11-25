//! Language server configuration: the sources one session resolves, merged
//! by precedence and gated on workspace trust. Four sources feed one
//! table, highest precedence first: the workspace's lsp file, the user
//! settings file, the product's plugin directory, and a compatible
//! frontend's plugin cache. A project entry from an untrusted workspace
//! resolves disabled and locked before anything is expanded, so a
//! repository cannot launch a command by shipping a config file.

use std::collections::BTreeMap;
use std::fs::read_dir;
use std::fs::read_to_string;
use std::path::Path;

use houyicoder_protocol::lsp::{LspConfigScope, ScopedLspServerConfig};
use serde_json::Value;

use crate::read_settings_value;
use crate::update_settings;
use crate::update_shared_settings;
use crate::{SettingsWriteError, expand_references};

mod plugin_cache;

/// The workspace file a project scope reads.
pub const PROJECT_FILE: &str = ".lsp.json";

/// The settings key the user scope reads.
pub const SETTINGS_KEY: &str = "lspServers";

/// Read every source's servers, merge them by precedence, refuse a project
/// entry from an untrusted workspace, and expand the variable references in
/// the entries that will actually run. Order is by server name, so two
/// reads of the same inputs expose the same sequence.
pub fn resolve_scoped_lsp_servers(
    settings_path: &Path,
    workspace: Option<&Path>,
    trusted: bool,
    plugin_dir: Option<&Path>,
    plugin_config_dir: Option<&Path>,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> Vec<ScopedLspServerConfig> {
    let mut by_name: BTreeMap<String, ScopedLspServerConfig> = BTreeMap::new();
    let project = workspace.map(parse_project_servers).unwrap_or_default();
    let plugins = plugin_dir.map(load_native_plugins).unwrap_or_default();
    let cached = plugin_config_dir
        .map(|dir| plugin_cache::servers(dir, workspace, trusted))
        .unwrap_or_default();
    for (scope, servers) in [
        (LspConfigScope::ClaudePlugin, cached),
        (LspConfigScope::Plugin, plugins),
        (LspConfigScope::User, parse_settings_servers(settings_path)),
        (LspConfigScope::Project, project),
    ] {
        for mut server in servers {
            server.scope = scope;
            if scope == LspConfigScope::Project && !trusted {
                // A repository cannot launch a command, and it cannot deny
                // one either: its entry never replaces a higher-precedence
                // entry of the same name.
                if by_name.contains_key(&server.name) {
                    tracing::warn!(
                        server = %server.name,
                        "an untrusted workspace names a server the user already configured; the workspace entry is ignored"
                    );
                    continue;
                }
                server.disabled = true;
                server.locked = true;
            }
            by_name.insert(server.name.clone(), server);
        }
    }
    by_name
        .into_values()
        .map(|mut server| {
            if !server.disabled {
                expand_scoped_entry(&mut server, lookup);
            }
            server
        })
        .collect()
}

/// Read one JSON body whose root is the server map (the project and plugin
/// file shape), keyed by server name. An entry that does not parse is a
/// named warning, never a silent drop.
fn read_root_server_map(value: &Value, source: &Path) -> Vec<ScopedLspServerConfig> {
    let Some(map) = value.as_object() else {
        return Vec::new();
    };
    parse_server_entries(map, source)
}

/// Read the settings shape, whose root object carries the map under the
/// settings key.
fn read_settings_server_map(value: &Value, source: &Path) -> Vec<ScopedLspServerConfig> {
    let Some(map) = value.get(SETTINGS_KEY).and_then(Value::as_object) else {
        return Vec::new();
    };
    parse_server_entries(map, source)
}

/// Parse one server map, naming every entry it has to drop.
pub(super) fn parse_server_entries(
    map: &serde_json::Map<String, Value>,
    source: &Path,
) -> Vec<ScopedLspServerConfig> {
    let mut servers = Vec::new();
    for (name, body) in map {
        match serde_json::from_value::<ScopedLspServerConfig>(body.clone()) {
            Ok(mut server) => {
                server.name = name.clone();
                normalize_extensions(&mut server.extension_to_language);
                servers.push(server);
            }
            Err(error) => tracing::warn!(
                file = %source.display(),
                server = name,
                %error,
                "an lsp server entry did not parse; skipped"
            ),
        }
    }
    servers
}

/// The routing keys normalize to one spelling: lowercase with the leading
/// dot, so a hand-written key routes the same as a generated one.
fn normalize_extensions(map: &mut BTreeMap<String, String>) {
    if map.is_empty() {
        return;
    }
    let normalized: BTreeMap<String, String> = map
        .iter()
        .map(|(key, language)| {
            let extension = key.trim().trim_start_matches('.').to_ascii_lowercase();
            (format!(".{extension}"), language.clone())
        })
        .collect();
    *map = normalized;
}

/// Read the project scope from the workspace's lsp file, whose root object
/// is the server map. Kept crate-private: the resolver owns the trust gate,
/// and a caller reaching these entries directly would bypass it.
pub(crate) fn parse_project_servers(workspace: &Path) -> Vec<ScopedLspServerConfig> {
    let path = workspace.join(PROJECT_FILE);
    read_root_server_map(&read_json(&path), &path)
}

/// Read the user scope from the user settings file.
pub fn parse_settings_servers(path: &Path) -> Vec<ScopedLspServerConfig> {
    read_settings_server_map(&read_json(path), path)
}

/// Read every plugin under the product's own plugin directory. Each plugin
/// is one subdirectory holding a file named by the project file constant;
/// plugins are read in sorted order so a name collision resolves the same
/// way twice.
fn load_native_plugins(plugins_dir: &Path) -> Vec<ScopedLspServerConfig> {
    let Ok(entries) = read_dir(plugins_dir) else {
        return Vec::new();
    };
    let mut dirs: Vec<_> = entries.filter_map(Result::ok).map(|e| e.path()).collect();
    dirs.sort();
    let mut all = Vec::new();
    for dir in dirs {
        let file = dir.join(PROJECT_FILE);
        for mut server in read_root_server_map(&read_json(&file), &file) {
            server.name = format!("plugin:{}:{}", plugin_label(&dir), server.name);
            all.push(server);
        }
    }
    all
}

/// Read one JSON file, warning when the bytes are not valid JSON: a broken
/// file must not brick the session, and it must not vanish in silence
/// either.
fn read_json(path: &Path) -> Value {
    match read_to_string(path) {
        Ok(text) => match serde_json::from_str(&text) {
            Ok(value) => value,
            Err(error) => {
                tracing::warn!(file = %path.display(), %error, "the lsp file is not valid json");
                Value::Object(Default::default())
            }
        },
        Err(_) => Value::Object(Default::default()),
    }
}

/// The plugin's directory name, the readable half of a scoped server name.
fn plugin_label(dir: &Path) -> String {
    dir.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Expand every variable reference in one entry: the command, each
/// argument, each environment value, and the workspace folder.
fn expand_scoped_entry(
    server: &mut ScopedLspServerConfig,
    lookup: &dyn Fn(&str) -> Option<String>,
) {
    server.command = expand_references(&server.command, lookup);
    for argument in &mut server.args {
        *argument = expand_references(argument, lookup);
    }
    for value in server.env.values_mut() {
        *value = expand_references(value, lookup);
    }
    if let Some(folder) = server.workspace_folder.as_mut() {
        *folder = expand_references(folder, lookup);
    }
}

/// Upsert one server into the user settings file, read back, and verify the
/// entry landed. A file whose root is not an object takes no entry, and the
/// caller is told rather than handed a silent success.
pub fn upsert_user_lsp_server(
    path: &Path,
    server: &ScopedLspServerConfig,
) -> Result<(), SettingsWriteError> {
    update_settings(path, |value| insert_settings_entry(value, server), 8)?;
    verify_landed(path, server, false)
}

/// Upsert one server into the workspace's project file at the shared file
/// mode, read back, and verify the entry landed.
pub fn upsert_project_lsp_server(
    workspace: &Path,
    server: &ScopedLspServerConfig,
) -> Result<(), SettingsWriteError> {
    let path = workspace.join(PROJECT_FILE);
    update_shared_settings(&path, |value| insert_root_entry(value, server), 8)?;
    verify_landed(&path, server, true)
}

/// Remove one server from the user settings file.
pub fn remove_user_lsp_server(path: &Path, name: &str) -> Result<(), SettingsWriteError> {
    update_settings(path, |value| remove_settings_named(value, name), 8)
}

/// Remove one server from the workspace's project file.
pub fn remove_project_lsp_server(workspace: &Path, name: &str) -> Result<(), SettingsWriteError> {
    let path = workspace.join(PROJECT_FILE);
    update_shared_settings(&path, |value| remove_root_named(value, name), 8)
}

/// Record one named user server's disabled flag, so a toggle survives the
/// next session. An entry the file does not hold is an error rather than a
/// silent no-write.
pub fn set_user_lsp_disabled(
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
    let applied = read_settings_value(path)
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

/// Place one entry in the root server map (the project and plugin file
/// shape). A root that is not an object holds no map; the caller's
/// read-back turns that into the error it is.
fn insert_root_entry(value: &mut Value, server: &ScopedLspServerConfig) {
    let Some(map) = value.as_object_mut() else {
        return;
    };
    if let Ok(body) = serde_json::to_value(server) {
        map.insert(server.name.clone(), body);
    }
}

/// Place one entry in the settings file's server map.
fn insert_settings_entry(value: &mut Value, server: &ScopedLspServerConfig) {
    let Some(obj) = value.as_object_mut() else {
        return;
    };
    let Ok(body) = serde_json::to_value(server) else {
        return;
    };
    let servers = obj
        .entry(SETTINGS_KEY)
        .or_insert_with(|| Value::Object(Default::default()));
    if let Some(map) = servers.as_object_mut() {
        map.insert(server.name.clone(), body);
    }
}

/// Drop one named server from the root server map.
fn remove_root_named(value: &mut Value, name: &str) {
    if let Some(map) = value.as_object_mut() {
        map.remove(name);
    }
}

/// Drop one named server from the settings file's server map. An empty map
/// leaves the settings file as the object it was before any entry existed.
fn remove_settings_named(value: &mut Value, name: &str) {
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

/// Confirm one upsert landed: the file reads back the named server with the
/// same command. root selects which file shape the read uses.
fn verify_landed(
    path: &Path,
    server: &ScopedLspServerConfig,
    root: bool,
) -> Result<(), SettingsWriteError> {
    let value = read_settings_value(path);
    let entry = if root {
        value.get(&server.name).cloned()
    } else {
        value
            .get(SETTINGS_KEY)
            .and_then(Value::as_object)
            .and_then(|map| map.get(&server.name))
            .cloned()
    };
    // The whole entry must read back, not just its command: a write that
    // lost the arguments or the environment is a write that did not land.
    let landed = entry.is_some_and(|body| body == serde_json::to_value(server).unwrap_or_default());
    if landed {
        Ok(())
    } else {
        Err(SettingsWriteError::Io(format!(
            "the server map in {} did not take the {} entry",
            path.display(),
            server.name
        )))
    }
}

/// The language id one file's suffix routes to in one server's table, if
/// any. The suffix is compared case-insensitively with the leading dot the
/// table keys carry.
pub fn language_for_extension<'a>(
    server: &'a ScopedLspServerConfig,
    extension: &str,
) -> Option<&'a str> {
    let key = format!(
        ".{}",
        extension.trim_start_matches('.').to_ascii_lowercase()
    );
    server.extension_to_language.get(&key).map(String::as_str)
}

/// The scope's label as the CLI list and the pane print it.
pub fn scope_label(scope: LspConfigScope) -> &'static str {
    match scope {
        LspConfigScope::User => "user",
        LspConfigScope::Project => "project",
        LspConfigScope::Plugin => "plugin",
        LspConfigScope::ClaudePlugin => "claude-plugin",
    }
}

#[cfg(test)]
mod tests;
