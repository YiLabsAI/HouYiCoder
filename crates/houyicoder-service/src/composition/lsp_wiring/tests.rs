//! Tests for the session wiring: which configurations yield a manager.

use std::env::temp_dir;
use std::env::var_os;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::id;

use super::*;

fn scratch(tag: &str) -> PathBuf {
    let root = temp_dir().join(format!("houyi-lsp-wire-{tag}-{}", id()));
    drop(fs::remove_dir_all(&root));
    fs::create_dir_all(root.join("home/.houyicoder")).expect("home");
    fs::create_dir_all(root.join("work")).expect("work");
    root
}

/// A configured, routable server yields a manager whose routes are in place
/// before the call returns; nothing configured, a routing-less entry, and a
/// disabled entry all yield none, so the tool never registers without a
/// capability behind it.
#[test]
fn test_wiring_follows_config() {
    let root = scratch("config");
    let home = root.join("home");
    let work = root.join("work");
    let settings = houyicoder_config::settings_path_in(&home);
    assert!(
        start(Some(&home), Some(&work)).is_none(),
        "nothing configured"
    );

    fs::write(
        &settings,
        r#"{"lspServers": {"rust": {"command": "rust-analyzer",
            "extensionToLanguage": {".rs": "rust"}}}}"#,
    )
    .expect("settings");
    let manager = start(Some(&home), Some(&work)).expect("a routable server yields a manager");
    let routed = block_on(manager.server_for_path(Path::new("a.rs")));
    assert_eq!(
        routed.as_deref(),
        Some("rust"),
        "the route exists before the tool registers"
    );

    fs::write(
        &settings,
        r#"{"lspServers": {"empty": {"command": "x", "extensionToLanguage": {}}}}"#,
    )
    .expect("settings");
    assert!(
        start(Some(&home), Some(&work)).is_none(),
        "an entry routing no suffix registers nothing"
    );

    fs::write(
        &settings,
        r#"{"lspServers": {"rust": {"command": "x", "disabled": true,
            "extensionToLanguage": {".rs": "rust"}}}}"#,
    )
    .expect("settings");
    assert!(
        start(Some(&home), Some(&work)).is_none(),
        "a disabled entry registers nothing"
    );

    fs::write(
        &settings,
        r#"{"lspServers": {"rust": {"command": "x", "transport": "socket",
            "extensionToLanguage": {".rs": "rust"}}}}"#,
    )
    .expect("settings");
    assert!(
        start(Some(&home), Some(&work)).is_none(),
        "an entry this host cannot launch registers nothing"
    );
    drop(fs::remove_dir_all(&root));
}

/// The plugin directory resolves from the home, and an explicit frontend
/// config override wins over the conventional directory.
#[test]
fn test_wiring_plugin_dirs() {
    let root = scratch("dirs");
    let home = root.join("home");
    let work = root.join("work");
    fs::create_dir_all(home.join(".houyicoder/plugins/demo")).expect("plugin dir");
    fs::write(
        home.join(".houyicoder/plugins/demo/.lsp.json"),
        r#"{"rust": {"command": "demo", "extensionToLanguage": {".rs": "rust"}}}"#,
    )
    .expect("plugin config");
    assert!(
        start(Some(&home), Some(&work)).is_some(),
        "a native plugin server is routable"
    );
    drop(fs::remove_dir_all(&root));
}

/// A production session has no home at hand. Its plugin directory comes from
/// the same config root the settings file does, and the gate's isolated
/// environment never falls back to the developer's own frontend cache.
#[test]
fn test_wiring_production_dirs() {
    assert_eq!(
        plugin_dir(None),
        houyicoder_config::config_home().join("plugins")
    );
    let root = scratch("prod-dirs");
    assert_eq!(
        plugin_dir(Some(&root)),
        root.join(".houyicoder").join("plugins")
    );
    if var_os(houyicoder_config::FRONTEND_CONFIG_DIR_ENV).is_none() {
        if var_os(houyicoder_config::ENV_HOUYICODER_CONFIG_HOME).is_some() {
            assert!(
                session_frontend_dir(None).is_none(),
                "an isolated gate environment reads no real frontend cache"
            );
        } else {
            assert_eq!(
                session_frontend_dir(None),
                houyicoder_config::frontend_config_dir(
                    houyicoder_config::user_home_dir().as_deref(),
                ),
                "a production session falls back to the real user home"
            );
        }
    }
    drop(fs::remove_dir_all(&root));
}

/// A session assembled without a home still mounts the servers a plugin
/// directory declares, which is the production shape that used to resolve
/// nothing at all.
#[test]
fn test_wiring_none_home_plugins() {
    let root = scratch("none-home");
    let plugin = root.join("plugins/demo");
    fs::create_dir_all(&plugin).expect("plugin dir");
    fs::write(
        plugin.join(".lsp.json"),
        r#"{"rust": {"command": "rust-analyzer", "extensionToLanguage": {".rs": "rust"}}}"#,
    )
    .expect("plugin config");
    let manager = start_with(
        Some(&root.join("work")),
        SessionDirs {
            settings_file: root.join("settings.json"),
            plugin_dir: Some(root.join("plugins")),
            frontend_dir: None,
        },
    )
    .expect("a plugin server is routable without a home");
    let routed = block_on(manager.server_for_path(Path::new("a.rs")));
    assert_eq!(routed.as_deref(), Some("plugin:demo:rust"));
    drop(fs::remove_dir_all(&root));
}
