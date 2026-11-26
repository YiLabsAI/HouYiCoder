//! Live language-server test against a real rust-analyzer binary. #[ignore]
//! and skipped with a printed reason when the binary is absent, so the
//! default gates never require it and the live suite proves the protocol
//! against the server models actually meet.
//!
//! The probe builds a throwaway cargo crate, opens its main file through the
//! manager, and asks the questions the tool asks: a definition, a hover, and
//! the diagnostics a save triggers. Every answer is asserted against what a
//! real compiler-backed server reports, not against a stub.

#![allow(clippy::unwrap_in_result)]

use std::env;
use std::env::var_os;
use std::fs::canonicalize;
use std::fs::create_dir_all;
use std::fs::read_dir;
use std::fs::remove_dir_all;
use std::fs::write;
use std::path::Path;
use std::path::PathBuf;
use std::process::id;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use houyicoder_api::launcher::StdProcessLauncher;
use houyicoder_protocol::lsp::{LspDiagnostic, ScopedLspServerConfig};
use houyicoder_service::lsp::{LspConnections, LspTimings};
use serde_json::Value;
use serde_json::json;
use tokio::time::sleep;

/// The environment override naming the binary, for a host that keeps it
/// outside PATH.
const ANALYZER_ENV: &str = "HOUYICODER_RUST_ANALYZER";

/// How long the probe waits for a cold server to finish its loading pass and
/// report. The first run against a cold analyzer can take half a minute
/// before its diagnostics arrive, so the budget spans that.
const REQUEST_BUDGET: Duration = Duration::from_secs(60);

/// How often an answer the server has not produced yet is asked again.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// The clean source the probe starts from, with the call site whose caret
/// the definition query names.
const SOURCE: &str = "fn helper() -> u32 {\n    1\n}\n\nfn main() {\n    let value: u32 = helper();\n    println!(\"{value}\");\n}\n";

/// The same file with a type error, so the diagnostics wait has a real
/// finding to report rather than an empty publish.
const BROKEN_SOURCE: &str = "fn helper() -> u32 {\n    1\n}\n\nfn main() {\n    let value: u32 = \"text\";\n    println!(\"{value}\");\n}\n";

/// The analyzer binary: the override first, then PATH. None means the
/// probe cannot run on this host. A rustup proxy whose toolchain lacks the
/// component is not a server: it exits before the handshake, so it is
/// treated as absent rather than run and mistaken for a broken server.
fn find_analyzer() -> Option<PathBuf> {
    if let Some(path) = var_os(ANALYZER_ENV) {
        let path = PathBuf::from(path);
        return usable(&path).then_some(path);
    }
    let name = if cfg!(windows) {
        "rust-analyzer.exe"
    } else {
        "rust-analyzer"
    };
    if let Some(found) = env::split_paths(&var_os("PATH")?)
        .map(|dir| dir.join(name))
        .find(|candidate| usable(candidate))
    {
        return Some(found);
    }
    // A proxy on PATH means rustup owns the toolchains. The component, when
    // installed, lives inside one of them rather than beside the proxy.
    let root = var_os("RUSTUP_HOME")
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|home| home.join(".rustup")))?;
    read_dir(root.join("toolchains"))
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path().join("bin").join(name))
        .find(|candidate| usable(candidate))
}

/// The user home this probe resolves the toolchain tree against.
fn home_dir() -> Option<PathBuf> {
    var_os("HOME")
        .or_else(|| var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// Whether one candidate is a file that is not a rustup proxy. A proxy is
/// recognized by the binary it links to, so a host that installed the
/// component resolves to the toolchain's own copy instead.
fn usable(candidate: &Path) -> bool {
    if !candidate.is_file() {
        return false;
    }
    let Ok(resolved) = canonicalize(candidate) else {
        return false;
    };
    let stem = resolved.file_stem().and_then(|stem| stem.to_str());
    stem != Some("rustup")
}

/// A file URI for one absolute path, percent-free because the probe's paths
/// carry no characters that need escaping.
fn file_uri(path: &Path) -> String {
    let text = path.display().to_string().replace('\\', "/");
    format!("file://{text}")
}

/// The live contract: open a real crate, resolve a definition and a hover
/// through the server, then save a broken file and receive its diagnostics.
#[tokio::test]
#[ignore = "requires a real rust-analyzer on PATH"]
async fn test_live_queries_and_diagnostics() {
    let Some(analyzer) = find_analyzer() else {
        println!("skipped: no rust-analyzer found (set {ANALYZER_ENV} or put it on PATH)");
        return;
    };
    let base = env::temp_dir().join(format!("houyi-lsp-live-{}", id()));
    drop(remove_dir_all(&base));
    let root = base.join("crate");
    let home = base.join("home");
    create_dir_all(root.join("src")).expect("the crate tree is created");
    create_dir_all(home.join(".houyicoder/plugins")).expect("the plugin tree is created");
    write(
        root.join("Cargo.toml"),
        "[package]\nname = \"lsp-live-probe\"\nversion = \"0.0.0\"\nedition = \"2021\"\n",
    )
    .expect("the manifest is written");
    let file = root.join("src/main.rs");
    write(&file, SOURCE).expect("the source is written");
    let servers = plugin_servers(&home, &analyzer);

    let manager = open_manager(&root, servers).await;
    let name = match manager.ensure_document(&file, SOURCE).await {
        Ok(Some(name)) => name,
        Ok(None) => panic!("no server routes the probe file"),
        Err(error) => {
            let stderr = manager
                .statuses()
                .await
                .first()
                .map(|status| status.recent_stderr.clone())
                .unwrap_or_default();
            panic!(
                "the manager could not start the server: {error}\nstderr:\n{}",
                stderr.join("\n")
            );
        }
    };
    assert_eq!(
        name, "plugin:rust-analyzer-lsp:rust",
        "the server came from the plugin declaration, saw {name}"
    );

    let uri = file_uri(&file);
    let deadline = Instant::now() + REQUEST_BUDGET;
    let definition = definition_until(&manager, &file, &uri, deadline).await;
    assert!(
        definition.to_string().contains("main.rs"),
        "the location names the source file: {definition}"
    );
    let hover = hover_until(&manager, &file, &uri, deadline).await;
    assert!(hover.to_string().contains("u32"), "{hover}");

    // The broken source must reach disk before the save is announced: the
    // server's type check reads the file from disk, so syncing a body the
    // disk never held leaves the checker looking at the clean original.
    write(&file, BROKEN_SOURCE).expect("the broken source is written");
    manager
        .sync_file_saved(&file, BROKEN_SOURCE)
        .await
        .expect("the broken save is synchronized");
    let diagnostics = diagnostics_until(&manager, &name, &uri, deadline).await;
    assert!(!diagnostics.is_empty());

    manager.shutdown().await;
    drop(remove_dir_all(&base));
}

/// The servers one real plugin declaration resolves to. The analyzer is
/// wired through the same resolver a production session runs, so the live
/// contract covers the plugin directory path rather than a hand-built entry.
fn plugin_servers(home: &Path, analyzer: &Path) -> Vec<ScopedLspServerConfig> {
    let plugin = home.join(".houyicoder/plugins/rust-analyzer-lsp");
    create_dir_all(&plugin).expect("the plugin directory is created");
    let declaration = json!({
        "rust": {
            "command": analyzer.to_string_lossy(),
            "extensionToLanguage": {".rs": "rust"},
        }
    });
    write(
        plugin.join(".lsp.json"),
        serde_json::to_string_pretty(&declaration).expect("the declaration encodes"),
    )
    .expect("the declaration is written");
    let settings = home.join(".houyicoder/settings.json");
    write(&settings, "{}").expect("the settings file is written");
    let lookup = |name: &str| env::var(name).ok();
    let servers = houyicoder_config::resolve_scoped_lsp_servers(
        &settings,
        None,
        true,
        Some(&home.join(".houyicoder/plugins")),
        None,
        &lookup,
    );
    assert!(
        !servers.is_empty(),
        "the plugin declaration resolves to a server"
    );
    servers
}

/// The manager over one probe crate with the resolved servers registered.
async fn open_manager(root: &Path, servers: Vec<ScopedLspServerConfig>) -> Arc<LspConnections> {
    let timings = LspTimings {
        diagnostics_grace: Duration::from_secs(10),
        diagnostics_cap: Duration::from_secs(20),
        index_wait: Duration::from_secs(5),
        ..LspTimings::default()
    };
    let manager = Arc::new(LspConnections::new(
        Arc::new(StdProcessLauncher::new()),
        Arc::from(root),
        timings,
    ));
    let mut servers = servers;
    for server in &mut servers {
        server.startup_timeout_ms = 30_000;
        server.shutdown_timeout_ms = 5_000;
    }
    manager.start_all(servers).await;
    manager
}

/// The caret position the probe asks about, the call site of the helper.
fn probe_position(uri: &str) -> Value {
    json!({"textDocument": {"uri": uri}, "position": {"line": 5, "character": 22}})
}

/// Ask for the definition until the server answers one. A cold server
/// answers an empty set until its loading pass finishes, so the first empty
/// answer is not this server's final word.
async fn definition_until(
    manager: &Arc<LspConnections>,
    file: &Path,
    uri: &str,
    deadline: Instant,
) -> Value {
    loop {
        let answer = manager
            .request_for_file(
                file,
                "textDocument/definition",
                probe_position(uri),
                None,
                REQUEST_BUDGET,
            )
            .await
            .expect("the definition answers");
        if !answer.is_null() && answer != json!([]) {
            return answer;
        }
        assert!(
            Instant::now() < deadline,
            "the definition stayed empty until the deadline"
        );
        sleep(POLL_INTERVAL).await;
    }
}

/// Ask for the hover until it names the helper's return type.
async fn hover_until(
    manager: &Arc<LspConnections>,
    file: &Path,
    uri: &str,
    deadline: Instant,
) -> Value {
    loop {
        let answer = manager
            .request_for_file(
                file,
                "textDocument/hover",
                probe_position(uri),
                None,
                REQUEST_BUDGET,
            )
            .await
            .expect("the hover answers");
        if answer.to_string().contains("u32") {
            return answer;
        }
        assert!(
            Instant::now() < deadline,
            "the hover never named the return type until the deadline"
        );
        sleep(POLL_INTERVAL).await;
    }
}

/// Wait for the publish that reports the type error. An intermediate pass
/// may publish an empty set first, so each publish is read from the
/// generation after the previous one and the wait never re-reads an entry
/// it already saw.
async fn diagnostics_until(
    manager: &Arc<LspConnections>,
    name: &str,
    uri: &str,
    deadline: Instant,
) -> Vec<LspDiagnostic> {
    let mut since = manager.publish_generation(name, uri).await;
    loop {
        if let Some(diagnostics) = manager.wait_for_diagnostics(name, uri, since).await {
            if diagnostics.iter().any(|diagnostic| {
                let message = diagnostic.message.to_lowercase();
                message.contains("mismatched") || message.contains("expected")
            }) {
                return diagnostics;
            }
            since = manager.publish_generation(name, uri).await;
        }
        assert!(
            Instant::now() < deadline,
            "the server never reported the type error until the deadline"
        );
        sleep(POLL_INTERVAL).await;
    }
}
