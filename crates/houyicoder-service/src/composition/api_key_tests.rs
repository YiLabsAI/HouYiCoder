//! Key resolution against a recording launcher: the helper path is asserted on
//! the request and policy it hands the chokepoint, so no test spawns.

use super::*;
use houyicoder_api::launcher::{LauncherChild, LauncherExit, SpawnError};
use std::sync::Mutex;

/// Resolve and keep only the key, for the cases that assert on it alone.
fn key_of(source: Option<&ApiKeySource>, launcher: Option<&dyn ProcessLauncher>) -> Option<String> {
    resolve_api_key(source, launcher).0
}

/// Records what it was asked to spawn and replays a scripted exit.
struct RecordingLauncher {
    seen: Mutex<Vec<(SpawnRequest, SpawnPolicy)>>,
    exit: Mutex<Option<Result<LauncherExit, SpawnError>>>,
}

impl RecordingLauncher {
    fn with_exit(exit: Result<LauncherExit, SpawnError>) -> Self {
        Self {
            seen: Mutex::new(Vec::new()),
            exit: Mutex::new(Some(exit)),
        }
    }

    fn ok_stdout(text: &str) -> Self {
        Self::with_exit(Ok(LauncherExit {
            exit_code: Some(0),
            stdout: Some(text.to_string()),
            stderr: None,
        }))
    }

    fn spawn_count(&self) -> usize {
        self.seen.lock().expect("seen").len()
    }

    fn last(&self) -> (SpawnRequest, SpawnPolicy) {
        let seen = self.seen.lock().expect("seen");
        let (req, policy) = seen.last().expect("a spawn was recorded");
        (req.clone(), policy.clone())
    }
}

impl ProcessLauncher for RecordingLauncher {
    fn spawn(&self, req: SpawnRequest, policy: SpawnPolicy) -> Result<LauncherChild, SpawnError> {
        self.seen.lock().expect("seen").push((req, policy.clone()));
        let scripted = self
            .exit
            .lock()
            .expect("exit")
            .take()
            .unwrap_or_else(|| Ok(LauncherExit::default()));
        Ok(LauncherChild::new(None, Box::pin(async move { scripted })))
    }
}

/// The helper's stdout is the key, trimmed of the newline echo leaves.
#[test]
fn test_stdout_is_key() {
    let launcher = RecordingLauncher::ok_stdout("secret-from-helper\n");
    let source = ApiKeySource::Helper("print-my-key".into());
    let key = key_of(Some(&source), Some(&launcher));
    assert_eq!(key.as_deref(), Some("secret-from-helper"));
}

/// One shell argument, not a split argv: splitting would break every helper
/// that uses a pipe or a redirect.
#[test]
fn test_runs_under_shell() {
    let launcher = RecordingLauncher::ok_stdout("k\n");
    let source = ApiKeySource::Helper("cat /tmp/key | tr -d x".into());
    drop(key_of(Some(&source), Some(&launcher)));
    let (req, _policy) = launcher.last();
    assert_eq!(req.program, "sh");
    assert_eq!(req.args, vec!["-c", "cat /tmp/key | tr -d x"]);
}

/// The spawn carries a wall timeout; without one a helper that never exits
/// hangs startup.
#[test]
fn test_spawn_time_bounded() {
    let launcher = RecordingLauncher::ok_stdout("k\n");
    let source = ApiKeySource::Helper("sleep-forever".into());
    drop(key_of(Some(&source), Some(&launcher)));
    let (_req, policy) = launcher.last();
    let fence = policy.fence.expect("a fence carries the wall timeout");
    assert_eq!(fence.wall_timeout_ms, HELPER_TIMEOUT_MS);
    assert!(
        fence.wall_timeout_ms > 0,
        "a zero timeout would kill every helper"
    );
}

/// The argv audit stays off: a key written inline in the helper command would
/// be copied into the log.
#[test]
fn test_spawn_skips_audit() {
    let launcher = RecordingLauncher::ok_stdout("k\n");
    let source = ApiKeySource::Helper("echo sk-inline-secret".into());
    drop(key_of(Some(&source), Some(&launcher)));
    let (_req, policy) = launcher.last();
    assert!(
        !policy.audit,
        "argv audit would log a helper command that may itself hold the key"
    );
}

/// A non-zero exit yields no key, so an error message is never sent as one.
#[test]
fn test_failed_helper_no_key() {
    let launcher = RecordingLauncher::with_exit(Ok(LauncherExit {
        exit_code: Some(1),
        stdout: Some("usage: vault read ...\n".into()),
        stderr: None,
    }));
    let source = ApiKeySource::Helper("vault read".into());
    // The env may hold a key on this machine, so this pins only that the
    // failed helper's own output is never the answer.
    let key = key_of(Some(&source), Some(&launcher));
    assert_ne!(key.as_deref(), Some("usage: vault read ..."));
}

/// A timeout surfaces as a wait error, which must not read as a blank key.
#[test]
fn test_timeout_no_key() {
    let launcher =
        RecordingLauncher::with_exit(Err(SpawnError::Io("wall timeout exceeded".into())));
    let source = ApiKeySource::Helper("sleep 600".into());
    let key = key_of(Some(&source), Some(&launcher));
    assert!(
        key.as_deref() != Some(""),
        "a timeout is not an empty credential"
    );
}

/// Blank output yields no key; an empty string would fail at the provider.
#[test]
fn test_blank_output_no_key() {
    let launcher = RecordingLauncher::ok_stdout("   \n");
    let source = ApiKeySource::Helper("true".into());
    let key = key_of(Some(&source), Some(&launcher));
    assert_ne!(key.as_deref(), Some(""));
}

/// With no source there is no command, so the env fallback costs no process.
#[test]
fn test_no_source_no_spawn() {
    let launcher = RecordingLauncher::ok_stdout("should-not-run\n");
    drop(key_of(None, Some(&launcher)));
    assert_eq!(
        launcher.spawn_count(),
        0,
        "with no key source there is nothing to spawn"
    );
}

/// One resolution, one spawn: a second would mean a second remote fetch.
#[test]
fn test_one_spawn_per_resolve() {
    let launcher = RecordingLauncher::ok_stdout("k\n");
    let source = ApiKeySource::Helper("print-my-key".into());
    drop(key_of(Some(&source), Some(&launcher)));
    assert_eq!(launcher.spawn_count(), 1);
}

/// The keychain lookup addresses the tool by absolute path. Resolving it
/// through PATH would let anything earlier on the path answer for a keychain.
#[cfg(target_os = "macos")]
#[test]
fn test_keychain_tool_absolute() {
    let launcher = RecordingLauncher::ok_stdout("kc-secret\n");
    let source = ApiKeySource::Keychain {
        service: "houyicoder".into(),
        account: "dashscope".into(),
    };
    drop(key_of(Some(&source), Some(&launcher)));
    let (req, _policy) = launcher.last();
    assert_eq!(req.program, "/usr/bin/security");
}

/// The lookup names the item by service and account, and asks for the password
/// alone. Dropping the account would return whichever one the keychain answers
/// with; dropping the password flag would return a description instead.
#[cfg(target_os = "macos")]
#[test]
fn test_keychain_argv_names_item() {
    let launcher = RecordingLauncher::ok_stdout("kc-secret\n");
    let source = ApiKeySource::Keychain {
        service: "houyicoder".into(),
        account: "dashscope".into(),
    };
    drop(key_of(Some(&source), Some(&launcher)));
    let (req, _policy) = launcher.last();
    assert_eq!(
        req.args,
        vec![
            "find-generic-password",
            "-s",
            "houyicoder",
            "-a",
            "dashscope",
            "-w"
        ]
    );
}

/// The keychain item's password is the key.
#[cfg(target_os = "macos")]
#[test]
fn test_keychain_password_is_key() {
    let launcher = RecordingLauncher::ok_stdout("kc-secret\n");
    let source = ApiKeySource::Keychain {
        service: "s".into(),
        account: "a".into(),
    };
    assert_eq!(
        key_of(Some(&source), Some(&launcher)).as_deref(),
        Some("kc-secret")
    );
}

/// This spawn IS audited, unlike the helper: argv carries the item's service
/// and account, never the password, and reading a user's keychain is an event
/// an operator should be able to see.
#[cfg(target_os = "macos")]
#[test]
fn test_keychain_spawn_is_audited() {
    let launcher = RecordingLauncher::ok_stdout("kc-secret\n");
    let source = ApiKeySource::Keychain {
        service: "s".into(),
        account: "a".into(),
    };
    drop(key_of(Some(&source), Some(&launcher)));
    let (req, policy) = launcher.last();
    assert!(
        policy.audit,
        "a keychain read must be visible to an operator"
    );
    assert!(
        !req.args.iter().any(|a| a.contains("kc-secret")),
        "argv must not carry the password"
    );
}

/// The lookup is time bounded, and given longer than the helper because the
/// keychain can put an authorization dialog in front of a human.
#[cfg(target_os = "macos")]
#[test]
fn test_keychain_spawn_time_bounded() {
    let launcher = RecordingLauncher::ok_stdout("kc-secret\n");
    let source = ApiKeySource::Keychain {
        service: "s".into(),
        account: "a".into(),
    };
    drop(key_of(Some(&source), Some(&launcher)));
    let (_req, policy) = launcher.last();
    let fence = policy.fence.expect("a fence carries the wall timeout");
    assert_eq!(fence.wall_timeout_ms, KEYCHAIN_TIMEOUT_MS);
}

/// A missing item or a denied authorization warns rather than falling back in
/// silence: stub mode with no explanation leaves the user with no way to tell
/// that their keychain entry is the reason.
#[cfg(target_os = "macos")]
#[test]
fn test_keychain_failure_warns() {
    let launcher = RecordingLauncher::with_exit(Ok(LauncherExit {
        exit_code: Some(44),
        stdout: Some(String::new()),
        stderr: Some("could not be found".into()),
    }));
    let source = ApiKeySource::Keychain {
        service: "houyicoder".into(),
        account: "dashscope".into(),
    };
    let (_key, warnings) = resolve_api_key(Some(&source), Some(&launcher));
    let w = warnings
        .iter()
        .find(|w| w.field == "keychain")
        .expect("a failed lookup warns");
    assert!(
        w.reason.contains("houyicoder") && w.reason.contains("dashscope"),
        "the notice must name the item so the user can fix it: {}",
        w.reason
    );
}

/// A successful lookup is quiet: a warning on the happy path would train the
/// user to ignore the channel.
#[cfg(target_os = "macos")]
#[test]
fn test_keychain_success_is_quiet() {
    let launcher = RecordingLauncher::ok_stdout("kc-secret\n");
    let source = ApiKeySource::Keychain {
        service: "s".into(),
        account: "a".into(),
    };
    let (_key, warnings) = resolve_api_key(Some(&source), Some(&launcher));
    assert!(warnings.is_empty(), "no warning on success: {warnings:?}");
}

/// Away from macOS the entry is reported and skipped, and no lookup is
/// attempted: a shared settings file must not look broken on a platform that
/// cannot honor it.
#[cfg(not(target_os = "macos"))]
#[test]
fn test_keychain_off_mac_warns() {
    let launcher = RecordingLauncher::ok_stdout("kc-secret\n");
    let source = ApiKeySource::Keychain {
        service: "s".into(),
        account: "a".into(),
    };
    let (_key, warnings) = resolve_api_key(Some(&source), Some(&launcher));
    assert_eq!(launcher.spawn_count(), 0, "no lookup off macOS");
    assert!(warnings.iter().any(|w| w.field == "keychain"));
}
