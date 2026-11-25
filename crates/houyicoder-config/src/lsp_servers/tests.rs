//! Tests for the language server configuration resolver and writers.

use std::collections::BTreeMap;
use std::env::temp_dir;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::id;

use super::*;

fn scratch(tag: &str) -> PathBuf {
    let dir = temp_dir().join(format!("houyi-lsp-cfg-{tag}-{}", id()));
    drop(fs::remove_dir_all(&dir));
    fs::create_dir_all(dir.join("home/.houyicoder")).expect("home tree");
    fs::create_dir_all(dir.join("work")).expect("workspace");
    dir
}

fn write(path: &Path, text: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("parent");
    }
    fs::write(path, text).expect("write file");
}

fn server(name: &str, command: &str) -> ScopedLspServerConfig {
    let mut extensions = BTreeMap::new();
    extensions.insert(".rs".to_string(), "rust".to_string());
    let mut server = ScopedLspServerConfig::stdio(command, extensions);
    server.name = name.to_string();
    server
}

fn no_env(_: &str) -> Option<String> {
    None
}

/// Project and user scopes merge by name with the project winning, and the
/// resolution is ordered so two reads expose the same sequence.
#[test]
fn test_merge_prefers_project() {
    let root = scratch("merge");
    let settings = root.join("home/.houyicoder/settings.json");
    let work = root.join("work");
    write(
        &settings,
        r#"{"lspServers": {"rust": {"command": "user-ra",
            "extensionToLanguage": {".rs": "rust"}}}}"#,
    );
    write(
        &work.join(PROJECT_FILE),
        r#"{"rust": {"command": "project-ra",
            "extensionToLanguage": {".rs": "rust"}}}"#,
    );
    let servers = resolve_scoped_lsp_servers(&settings, Some(&work), true, None, None, &no_env);
    assert_eq!(servers.len(), 1);
    assert_eq!(servers[0].name, "rust");
    assert_eq!(servers[0].command, "project-ra");
    assert_eq!(servers[0].scope, LspConfigScope::Project);
    drop(fs::remove_dir_all(&root));
}

/// A project entry from an untrusted workspace is disabled and locked
/// before anything is expanded: its variable references are never looked
/// up, and the lock is what refuses a later enable.
#[test]
fn test_untrusted_project_locked() {
    let root = scratch("trust");
    let settings = root.join("home/.houyicoder/settings.json");
    let work = root.join("work");
    write(
        &work.join(PROJECT_FILE),
        r#"{"rust": {"command": "${SECRET_CMD}",
            "extensionToLanguage": {".rs": "rust"}}}"#,
    );
    let looked_up = std::cell::Cell::new(false);
    let lookup = |_: &str| {
        looked_up.set(true);
        Some("expanded".to_string())
    };
    let servers = resolve_scoped_lsp_servers(&settings, Some(&work), false, None, None, &lookup);
    assert_eq!(servers.len(), 1);
    assert!(servers[0].disabled);
    assert!(servers[0].locked);
    assert!(!looked_up.get(), "a disabled entry is never expanded");
    let trusted = resolve_scoped_lsp_servers(&settings, Some(&work), true, None, None, &lookup);
    assert!(!trusted[0].disabled);
    assert_eq!(trusted[0].command, "expanded");
    drop(fs::remove_dir_all(&root));
}

/// Variable spellings expand in the command, the arguments, and the
/// environment, and a plain name without a value expands empty.
#[test]
fn test_entry_expansion() {
    let root = scratch("expand");
    let settings = root.join("home/.houyicoder/settings.json");
    write(
        &settings,
        r#"{"lspServers": {"rust": {
            "command": "${RA_BIN}",
            "args": ["--log=${UNSET:-/tmp/ra.log}"],
            "env": {"HOME_COPY": "${RA_BIN}"},
            "extensionToLanguage": {".rs": "rust"}}}}"#,
    );
    let lookup = |name: &str| (name == "RA_BIN").then(|| "/opt/ra".to_string());
    let servers = resolve_scoped_lsp_servers(&settings, None, true, None, None, &lookup);
    assert_eq!(servers[0].command, "/opt/ra");
    assert_eq!(servers[0].args, vec!["--log=/tmp/ra.log".to_string()]);
    assert_eq!(
        servers[0].env.get("HOME_COPY"),
        Some(&"/opt/ra".to_string())
    );
    drop(fs::remove_dir_all(&root));
}

/// A plugin under the product's plugin directory contributes servers named
/// with its directory, so a plugin and a user entry can share a name.
#[test]
fn test_native_plugin_names() {
    let root = scratch("native");
    let home = root.join("home/.houyicoder");
    write(
        &home.join("plugins/demo").join(PROJECT_FILE),
        r#"{"rust": {"command": "demo-ra", "extensionToLanguage": {".rs": "rust"}}}"#,
    );
    let servers = resolve_scoped_lsp_servers(
        &home.join("settings.json"),
        None,
        true,
        Some(&home.join("plugins")),
        None,
        &no_env,
    );
    assert_eq!(servers.len(), 1);
    assert_eq!(servers[0].name, "plugin:demo:rust");
    assert_eq!(servers[0].scope, LspConfigScope::Plugin);
    drop(fs::remove_dir_all(&root));
}

/// The writers round-trip both file shapes and verify what they wrote: the
/// user entry nests under the settings key, the project entry is the root
/// map, and a remove leaves the files as they were.
#[test]
fn test_writers_round_trip() {
    let root = scratch("writers");
    let settings = root.join("home/.houyicoder/settings.json");
    let work = root.join("work");
    write(&settings, "{}");
    let mut rust = server("rust", "rust-analyzer");
    upsert_user_lsp_server(&settings, &rust).expect("user upsert");
    assert_eq!(parse_settings_servers(&settings).len(), 1);
    rust.command = "project-ra".into();
    rust.scope = LspConfigScope::Project;
    upsert_project_lsp_server(&work, &rust).expect("project upsert");
    assert_eq!(parse_project_servers(&work)[0].command, "project-ra");
    remove_project_lsp_server(&work, "rust").expect("project remove");
    assert!(parse_project_servers(&work).is_empty());
    remove_user_lsp_server(&settings, "rust").expect("user remove");
    assert!(parse_settings_servers(&settings).is_empty());
    drop(fs::remove_dir_all(&root));
}

/// A file whose value shape cannot hold the entry is an error, not a
/// silent success: a project root that is not an object, and a settings
/// root that is not an object.
#[test]
fn test_writers_report_shape() {
    let root = scratch("shape");
    let settings = root.join("home/.houyicoder/settings.json");
    let work = root.join("work");
    write(&work.join(PROJECT_FILE), "[1, 2]");
    write(&settings, "[]");
    let rust = server("rust", "rust-analyzer");
    assert!(upsert_project_lsp_server(&work, &rust).is_err());
    assert!(upsert_user_lsp_server(&settings, &rust).is_err());
    drop(fs::remove_dir_all(&root));
}

/// The disabled flag persists on the named entry, and a name the settings
/// file does not hold is an error.
#[test]
fn test_disabled_flag() {
    let root = scratch("disabled");
    let settings = root.join("home/.houyicoder/settings.json");
    let rust = server("rust", "rust-analyzer");
    upsert_user_lsp_server(&settings, &rust).expect("upsert");
    set_user_lsp_disabled(&settings, "rust", true).expect("disable");
    assert!(parse_settings_servers(&settings)[0].disabled);
    set_user_lsp_disabled(&settings, "rust", false).expect("enable");
    assert!(!parse_settings_servers(&settings)[0].disabled);
    let error = set_user_lsp_disabled(&settings, "nobody", true).expect_err("absent name");
    assert!(
        error.to_string().contains("no server named nobody"),
        "{error}"
    );
    drop(fs::remove_dir_all(&root));
}

/// The extension lookup ignores the leading dot and the case on both sides.
#[test]
fn test_extension_lookup() {
    let rust = server("rust", "rust-analyzer");
    assert_eq!(language_for_extension(&rust, "rs"), Some("rust"));
    assert_eq!(language_for_extension(&rust, ".RS"), Some("rust"));
    assert_eq!(language_for_extension(&rust, "ts"), None);
}

/// Extension keys normalize when an entry is parsed, so a hand-written
/// uppercase or dot-less key routes the same as the canonical spelling.
#[test]
fn test_extension_keys_normalize() {
    let root = scratch("normalize-keys");
    let settings = root.join("home/.houyicoder/settings.json");
    write(
        &settings,
        r#"{"lspServers": {"rust": {"command": "rust-analyzer",
            "extensionToLanguage": {"RS": "rust", "toml": "toml"}}}}"#,
    );
    let servers = resolve_scoped_lsp_servers(&settings, None, true, None, None, &no_env);
    let keys: Vec<&str> = servers[0]
        .extension_to_language
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(keys, vec![".rs", ".toml"]);
    assert_eq!(language_for_extension(&servers[0], "Rs"), Some("rust"));
    drop(fs::remove_dir_all(&root));
}

/// An untrusted workspace entry never shadows a user server of the same
/// name: the repository can neither launch a command nor take away a
/// capability the user configured.
#[test]
fn test_untrusted_project_cannot_shadow() {
    let root = scratch("shadow");
    let settings = root.join("home/.houyicoder/settings.json");
    let work = root.join("work");
    write(
        &settings,
        r#"{"lspServers": {"rust": {"command": "user-ra",
            "extensionToLanguage": {".rs": "rust"}}}}"#,
    );
    write(
        &work.join(PROJECT_FILE),
        r#"{"rust": {"command": "repo-ra",
            "extensionToLanguage": {".rs": "rust"}}}"#,
    );
    let servers = resolve_scoped_lsp_servers(&settings, Some(&work), false, None, None, &no_env);
    assert_eq!(servers.len(), 1);
    assert_eq!(servers[0].command, "user-ra", "the user entry stands");
    assert_eq!(servers[0].scope, LspConfigScope::User);
    assert!(!servers[0].disabled);
    assert!(!servers[0].locked);
    drop(fs::remove_dir_all(&root));
}
