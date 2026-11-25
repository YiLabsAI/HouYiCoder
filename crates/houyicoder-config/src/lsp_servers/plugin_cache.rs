//! Plugin-cache discovery for language servers. The install record alone is
//! not the answer to what runs: a plugin removed from use stays in the
//! cache, and only the enable map decides whether its servers load. A
//! project-scope record also names the checkout it was installed for, and a
//! declaration pointing outside its plugin directory is refused outright.

use std::collections::BTreeMap;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

use houyicoder_protocol::lsp::ScopedLspServerConfig;
use serde_json::Value;

use crate::read_settings_value;

/// The variable a plugin config uses to name its own install root.
const PLUGIN_ROOT_VAR: &str = "CLAUDE_PLUGIN_ROOT";

/// Every server the enabled, scope-matching plugins declare, named
/// key-prefixed so two plugins can ship one server name.
pub(super) fn servers(
    config_dir: &Path,
    workspace: Option<&Path>,
    trusted: bool,
) -> Vec<ScopedLspServerConfig> {
    let installed = read_settings_value(&config_dir.join("plugins").join("installed_plugins.json"));
    let Some(plugins) = installed.get("plugins").and_then(Value::as_object) else {
        return Vec::new();
    };
    let enabled = enabled_map(config_dir, workspace, trusted);
    let mut all = Vec::new();
    for (key, entries) in plugins {
        if enabled.get(key.as_str()) != Some(&true) {
            continue;
        }
        let Some(entries) = entries.as_array() else {
            continue;
        };
        for entry in entries {
            if !scope_matches(entry, workspace) {
                continue;
            }
            let Some(install) = entry
                .get("installPath")
                .and_then(Value::as_str)
                .map(PathBuf::from)
            else {
                continue;
            };
            for (name, server) in read_plugin_servers(&install) {
                let mut server = server;
                server.name = format!("plugin:{key}:{name}");
                rewrite_plugin_root(&mut server, &install);
                all.push(server);
            }
        }
    }
    all.sort_by(|a, b| a.name.cmp(&b.name));
    all
}

/// The enable map the two settings files spell: the user file first, the
/// workspace file overriding it. An untrusted workspace's file is ignored
/// entirely, so a repository cannot switch on a plugin the user installed
/// for another checkout.
fn enabled_map(
    config_dir: &Path,
    workspace: Option<&Path>,
    trusted: bool,
) -> BTreeMap<String, bool> {
    let mut enabled = BTreeMap::new();
    for (path, allowed) in [
        (Some(config_dir.join("settings.json")), true),
        (
            workspace.map(|w| w.join(".claude").join("settings.json")),
            trusted,
        ),
    ] {
        if !allowed {
            continue;
        }
        let Some(path) = path else {
            continue;
        };
        let Some(map) = read_settings_value(&path)
            .get("enabledPlugins")
            .and_then(Value::as_object)
            .cloned()
        else {
            continue;
        };
        for (key, value) in map {
            if let Some(flag) = value.as_bool() {
                enabled.insert(key, flag);
            }
        }
    }
    enabled
}

/// Whether one install record applies here: a user-scope record always, a
/// project-scope record only in the checkout it names.
fn scope_matches(entry: &Value, workspace: Option<&Path>) -> bool {
    match entry.get("scope").and_then(Value::as_str) {
        Some("user") => true,
        Some("project") => {
            let Some(workspace) = workspace else {
                return false;
            };
            let Some(recorded) = entry.get("projectPath").and_then(Value::as_str) else {
                return false;
            };
            same_path(Path::new(recorded), workspace)
        }
        _ => false,
    }
}

/// Two paths name the same directory, with canonicalization on both sides
/// so a symlinked or relatively spelled checkout still matches. A path that
/// cannot be canonicalized falls back to lexical equality.
fn same_path(left: &Path, right: &Path) -> bool {
    match (dunce::canonicalize(left), dunce::canonicalize(right)) {
        (Ok(a), Ok(b)) => a == b,
        _ => left == right,
    }
}

/// One plugin's servers: the file at its install root plus whatever its
/// manifest declares, later entries winning on a name collision.
fn read_plugin_servers(install: &Path) -> Vec<(String, ScopedLspServerConfig)> {
    let lsp_file = install.join(super::PROJECT_FILE);
    let mut servers = read_root_file(&lsp_file);
    let manifest_path = install.join(".claude-plugin").join("plugin.json");
    let manifest = super::read_json(&manifest_path);
    if let Some(declaration) = manifest.get("lspServers") {
        servers.extend(read_declaration(declaration, install));
    }
    servers
}

/// The servers one root-map file declares, read through the resolver's one
/// parse helper so both sources drop entries the same way and name them.
fn read_root_file(path: &Path) -> Vec<(String, ScopedLspServerConfig)> {
    let value = super::read_json(path);
    let Some(map) = value.as_object() else {
        return Vec::new();
    };
    super::parse_server_entries(map, path)
        .into_iter()
        .map(|server| (server.name.clone(), server))
        .collect()
}

/// The manifest declaration in any of its three spellings: a relative file
/// path, an inline map, or an array mixing the two.
fn read_declaration(declaration: &Value, install: &Path) -> Vec<(String, ScopedLspServerConfig)> {
    let mut servers = Vec::new();
    match declaration {
        Value::String(path) => servers.extend(read_declared_file(path, install)),
        Value::Object(map) => servers.extend(read_inline_map(map, install)),
        Value::Array(items) => {
            for item in items {
                match item {
                    Value::String(path) => servers.extend(read_declared_file(path, install)),
                    Value::Object(map) => servers.extend(read_inline_map(map, install)),
                    _ => {}
                }
            }
        }
        _ => {}
    }
    servers
}

/// One relative declaration path resolved inside the plugin directory, or
/// nothing when it is absolute or climbs out with parent segments.
fn read_declared_file(relative: &str, install: &Path) -> Vec<(String, ScopedLspServerConfig)> {
    let Some(path) = safe_join(install, relative) else {
        tracing::warn!(
            plugin = %install.display(),
            path = relative,
            "a plugin lsp declaration points outside its directory; skipped"
        );
        return Vec::new();
    };
    read_root_file(&path)
}

/// An inline declaration's servers, parsed through the shared helper.
fn read_inline_map(
    map: &serde_json::Map<String, Value>,
    install: &Path,
) -> Vec<(String, ScopedLspServerConfig)> {
    let source = install.join(".claude-plugin").join("plugin.json");
    super::parse_server_entries(map, &source)
        .into_iter()
        .map(|server| (server.name.clone(), server))
        .collect()
}

/// Resolve a declaration path inside the plugin directory. Absolute paths
/// and parent segments are refused before any filesystem call, and the
/// canonicalized result must still sit under the canonicalized root, so a
/// symlink cannot carry the read outside either.
fn safe_join(root: &Path, relative: &str) -> Option<PathBuf> {
    let candidate = Path::new(relative);
    if candidate.is_absolute() {
        return None;
    }
    if candidate
        .components()
        .any(|part| matches!(part, Component::ParentDir))
    {
        return None;
    }
    let joined = root.join(candidate);
    let root_real = dunce::canonicalize(root).ok()?;
    let joined_real = dunce::canonicalize(&joined).ok()?;
    joined_real.starts_with(&root_real).then_some(joined_real)
}

/// Replace the plugin-root variable with the install path in every string
/// the child could receive, so a config written for the plugin layout
/// launches the same program here.
fn rewrite_plugin_root(server: &mut ScopedLspServerConfig, install: &Path) {
    let root = install.to_string_lossy().into_owned();
    let replace = |text: &str| text.replace(&format!("${{{PLUGIN_ROOT_VAR}}}"), &root);
    server.command = replace(&server.command);
    for argument in &mut server.args {
        *argument = replace(argument);
    }
    for value in server.env.values_mut() {
        *value = replace(value);
    }
    if let Some(folder) = server.workspace_folder.as_mut() {
        *folder = replace(folder);
    }
}

#[cfg(test)]
mod tests;
