//! Tests for the plugin-cache discovery: the enable map decides which
//! cached plugins load, the scope decides where they apply, and the
//! declaration never leaves the plugin directory.

use std::env::temp_dir;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::id;

use super::*;
use houyicoder_protocol::lsp::LspTransportKind;

fn scratch(tag: &str) -> PathBuf {
    let dir = temp_dir().join(format!("houyi-lsp-cache-{tag}-{}", id()));
    drop(fs::remove_dir_all(&dir));
    fs::create_dir_all(&dir).expect("scratch");
    dir
}

fn write(path: &Path, text: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("parent");
    }
    fs::write(path, text).expect("write file");
}

/// Install one plugin under the cache with the given install root and
/// return the installed record entry text.
fn install(
    root: &Path,
    key: &str,
    scope: &str,
    project: Option<&Path>,
    install_marker: &str,
) -> PathBuf {
    let install_root = root.join("cache").join(install_marker);
    fs::create_dir_all(&install_root).expect("install root");
    write(
        &install_root.join(".lsp.json"),
        r#"{"rust": {"command": "rust-analyzer", "extensionToLanguage": {".rs": "rust"}}}"#,
    );
    // The record is built by the serializer, never by formatting a path into
    // JSON text: a Windows path's backslashes are not valid escapes, and the
    // parse of such a document fails where the file write succeeded.
    let mut entry = serde_json::json!({
        "scope": scope,
        "installPath": install_root.to_string_lossy(),
    });
    if let Some(path) = project {
        entry["projectPath"] = serde_json::Value::String(path.to_string_lossy().into_owned());
    }
    let installed = root.join("frontend/plugins/installed_plugins.json");
    let existing = fs::read_to_string(&installed).unwrap_or_else(|_| r#"{"plugins":{}}"#.into());
    let mut value: serde_json::Value = serde_json::from_str(&existing).expect("installed json");
    value["plugins"][key] = serde_json::json!([entry]);
    write(
        &installed,
        &serde_json::to_string_pretty(&value).expect("encode"),
    );
    install_root
}

/// Only a plugin the settings explicitly enable loads, and each loaded
/// server carries its plugin-key prefix.
#[test]
fn test_enable_filter() {
    let root = scratch("enable");
    install(&root, "ra@market", "user", None, "ra");
    install(&root, "off@market", "user", None, "off");
    let frontend = root.join("frontend");
    write(
        &frontend.join("settings.json"),
        r#"{"enabledPlugins": {"ra@market": true, "off@market": false}}"#,
    );
    let servers = servers(&frontend, None, true);
    assert_eq!(servers.len(), 1);
    assert_eq!(servers[0].name, "plugin:ra@market:rust");
    drop(fs::remove_dir_all(&root));
}

/// A plugin nobody listing enables does not load: the install record alone
/// is not consent.
#[test]
fn test_missing_enable_skips() {
    let root = scratch("missing-enable");
    install(&root, "ra@market", "user", None, "ra");
    let frontend = root.join("frontend");
    write(&frontend.join("settings.json"), "{}");
    assert!(servers(&frontend, None, true).is_empty());
    drop(fs::remove_dir_all(&root));
}

/// A project-scope plugin loads only in the checkout it names, and a
/// project-scope setting cannot load anything without one.
#[test]
fn test_project_scope_match() {
    let root = scratch("scope");
    let work = root.join("work");
    fs::create_dir_all(&work).expect("workspace");
    install(&root, "ra@market", "project", Some(&work), "ra");
    let frontend = root.join("frontend");
    write(
        &frontend.join("settings.json"),
        r#"{"enabledPlugins": {"ra@market": true}}"#,
    );
    assert_eq!(servers(&frontend, Some(&work), true).len(), 1);
    let elsewhere = root.join("elsewhere");
    fs::create_dir_all(&elsewhere).expect("other workspace");
    assert!(servers(&frontend, Some(&elsewhere), true).is_empty());
    assert!(servers(&frontend, None, true).is_empty());
    drop(fs::remove_dir_all(&root));
}

/// The workspace settings can enable a plugin, and an untrusted workspace
/// cannot: its enable map is ignored entirely while the user map stands.
#[test]
fn test_workspace_enable_needs_trust() {
    let root = scratch("ws-enable");
    install(&root, "ra@market", "user", None, "ra");
    let frontend = root.join("frontend");
    let work = root.join("work");
    write(&frontend.join("settings.json"), "{}");
    write(
        &work
            .join(super::super::FRONTEND_HOME_DIR)
            .join("settings.json"),
        r#"{"enabledPlugins": {"ra@market": true}}"#,
    );
    assert!(
        servers(&frontend, Some(&work), false).is_empty(),
        "an untrusted workspace cannot switch a plugin on"
    );
    assert_eq!(servers(&frontend, Some(&work), true).len(), 1);
    drop(fs::remove_dir_all(&root));
}

/// The manifest declares servers in all three spellings: a relative file
/// path, an inline map, and an array mixing both. A path that climbs out of
/// the plugin directory is refused.
#[test]
fn test_manifest_declarations() {
    let root = scratch("manifest");
    let install_root = install(&root, "mix@market", "user", None, "mix");
    write(
        &install_root.join("extra.json"),
        r#"{"ts": {"command": "typescript-language-server", "extensionToLanguage": {".ts": "typescript"}}}"#,
    );
    write(
        &install_root
            .join(super::COMPAT_MANIFEST_DIR)
            .join("plugin.json"),
        r#"{"lspServers": [
            "extra.json",
            {"inline": {"command": "inline-ls", "extensionToLanguage": {".py": "python"}}},
            "../escape.json"
        ]}"#,
    );
    write(
        &root.join("escape.json"),
        r#"{"escape": {"command": "evil", "extensionToLanguage": {".x": "x"}}}"#,
    );
    let frontend = root.join("frontend");
    write(
        &frontend.join("settings.json"),
        r#"{"enabledPlugins": {"mix@market": true}}"#,
    );
    let servers = servers(&frontend, None, true);
    let names: Vec<&str> = servers.iter().map(|s| s.name.as_str()).collect();
    assert!(names.contains(&"plugin:mix@market:rust"), "{names:?}");
    assert!(names.contains(&"plugin:mix@market:ts"), "{names:?}");
    assert!(names.contains(&"plugin:mix@market:inline"), "{names:?}");
    assert!(
        !names.iter().any(|name| name.contains("escape")),
        "a declaration outside the plugin directory never loads: {names:?}"
    );
    drop(fs::remove_dir_all(&root));
}

/// The plugin-root variable expands to the install path in the command, its
/// arguments, and the environment before the entry leaves this module.
#[test]
fn test_plugin_root_expansion() {
    let root = scratch("root-var");
    let install_root = install(&root, "ra@market", "user", None, "ra");
    write(
        &install_root.join(".lsp.json"),
        r#"{"rust": {
            "command": "${CLAUDE_PLUGIN_ROOT}/bin/ra",
            "args": ["--root=${CLAUDE_PLUGIN_ROOT}"],
            "env": {"RA_ROOT": "${CLAUDE_PLUGIN_ROOT}"},
            "extensionToLanguage": {".rs": "rust"}}}"#,
    );
    let frontend = root.join("frontend");
    write(
        &frontend.join("settings.json"),
        r#"{"enabledPlugins": {"ra@market": true}}"#,
    );
    let servers = servers(&frontend, None, true);
    let install_text = install_root.to_string_lossy().into_owned();
    assert_eq!(servers[0].command, format!("{install_text}/bin/ra"));
    assert_eq!(servers[0].args, vec![format!("--root={install_text}")]);
    assert_eq!(servers[0].env.get("RA_ROOT"), Some(&install_text));
    drop(fs::remove_dir_all(&root));
}

/// An absolute declaration is refused before any filesystem call, and a
/// symlink inside the plugin directory that resolves outside it is refused
/// after canonicalization: the two escape shapes the containment check
/// exists for.
#[test]
fn test_declarations_cannot_escape() {
    let root = scratch("escape");
    let install_root = install(&root, "mix@market", "user", None, "mix");
    let outside = root.join("outside.json");
    write(
        &outside,
        r#"{"evil": {"command": "evil", "extensionToLanguage": {".x": "x"}}}"#,
    );
    #[cfg(unix)]
    {
        symlink(&outside, install_root.join("linked.json")).expect("symlink");
    }
    write(
        &install_root
            .join(super::COMPAT_MANIFEST_DIR)
            .join("plugin.json"),
        &serde_json::json!({"lspServers": [
            outside.to_string_lossy(),
            "linked.json"
        ]})
        .to_string(),
    );
    let frontend = root.join("frontend");
    write(
        &frontend.join("settings.json"),
        r#"{"enabledPlugins": {"mix@market": true}}"#,
    );
    let servers = servers(&frontend, None, true);
    assert!(
        !servers.iter().any(|server| server.name.contains("evil")),
        "neither an absolute path nor a symlink out of the root loads"
    );
    drop(fs::remove_dir_all(&root));
}

/// Entries the on-disk format spells read faithfully: the timeout fields
/// keep their declared values and the transport parses, so an existing
/// plugin payload does not silently fall back to defaults.
#[test]
fn test_reference_spelling_reads() {
    let root = scratch("reference");
    let install_root = install(&root, "ra@market", "user", None, "ra");
    write(
        &install_root.join(super::super::PROJECT_FILE),
        r#"{"rust": {
            "command": "rust-analyzer",
            "startupTimeout": 60000,
            "shutdownTimeout": 5000,
            "extensionToLanguage": {".rs": "rust"}}}"#,
    );
    let frontend = root.join("frontend");
    write(
        &frontend.join("settings.json"),
        r#"{"enabledPlugins": {"ra@market": true}}"#,
    );
    let servers = servers(&frontend, None, true);
    assert_eq!(servers[0].startup_timeout_ms, 60_000);
    assert_eq!(servers[0].shutdown_timeout_ms, 5_000);
    assert_eq!(servers[0].transport, LspTransportKind::Stdio);
    drop(fs::remove_dir_all(&root));
}

/// An official marketplace plugin whose cache directory carries only its
/// license and readme is still found: its servers live in the marketplace
/// manifest's plugins array, both as an inline declaration and as a source
/// subdirectory the manifest names.
#[test]
fn test_marketplace_fallback() {
    let root = scratch("marketplace");
    let frontend = root.join("frontend");
    let cache = frontend.join("plugins/cache/market/ra/1.0.0");
    fs::create_dir_all(&cache).expect("cache dir");
    write(&cache.join("LICENSE"), "MIT");
    write(&cache.join("README.md"), "probe");
    let installed = frontend.join("plugins/installed_plugins.json");
    write(
        &installed,
        &serde_json::to_string_pretty(&serde_json::json!({
            "version": 2,
            "plugins": {
                "ra@market": [{
                    "scope": "user",
                    "installPath": cache.to_string_lossy(),
                }],
            },
        }))
        .expect("encode"),
    );
    let market = frontend.join("plugins/marketplaces/market");
    let manifest_dir = market.join(super::COMPAT_MANIFEST_DIR);
    fs::create_dir_all(&manifest_dir).expect("manifest dir");
    let source = market.join("plugins/ra");
    write(
        &source.join(".lsp.json"),
        r#"{"ra-src": {"command": "rust-analyzer-src", "extensionToLanguage": {".rs": "rust"}}}"#,
    );
    write(
        &manifest_dir.join("marketplace.json"),
        &serde_json::to_string_pretty(&serde_json::json!({
            "name": "market",
            "plugins": [{
                "name": "ra",
                "source": "./plugins/ra",
                "lspServers": {
                    "ra-inline": {
                        "command": "rust-analyzer-inline",
                        "extensionToLanguage": {".rs": "rust"},
                    },
                },
            }],
        }))
        .expect("encode"),
    );
    write(
        &frontend.join("plugins/known_marketplaces.json"),
        &serde_json::to_string_pretty(&serde_json::json!({
            "market": {
                "installLocation": market.to_string_lossy(),
            },
        }))
        .expect("encode"),
    );
    write(
        &frontend.join("settings.json"),
        r#"{"enabledPlugins": {"ra@market": true}}"#,
    );
    let servers = servers(&frontend, None, true);
    let names: Vec<&str> = servers.iter().map(|s| s.name.as_str()).collect();
    assert!(
        names.contains(&"plugin:ra@market:ra-inline"),
        "the marketplace inline declaration is found: {names:?}"
    );
    assert!(
        names.contains(&"plugin:ra@market:ra-src"),
        "the source subdirectory file is found: {names:?}"
    );
    drop(fs::remove_dir_all(&root));
}
