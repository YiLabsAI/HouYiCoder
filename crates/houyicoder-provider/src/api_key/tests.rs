//! Key resolution against a recording launcher: the helper path is asserted on
//! the request and policy it hands the chokepoint, so no test spawns.

use super::*;
use houyicoder_api::launcher::{LauncherChild, LauncherExit, SpawnError};
use std::sync::Mutex;

/// Resolve and keep only the key, for the cases that assert on it alone.
fn resolved_key(
    source: Option<&ApiKeySource>,
    launcher: Option<&dyn ProcessLauncher>,
) -> Option<String> {
    resolve_api_key(source, None, launcher).key
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
    let source = ApiKeySource::Helper {
        command: "print-my-key".into(),
        shell: None,
    };
    let key = resolved_key(Some(&source), Some(&launcher));
    assert_eq!(key.as_deref(), Some("secret-from-helper"));
}

/// A helper that names a shell the host cannot run is reported and falls back
/// to the environment, with no spawn attempted.
#[cfg(unix)]
#[test]
fn test_unavailable_helper_shell() {
    let launcher = RecordingLauncher::ok_stdout("k");
    let source = ApiKeySource::Helper {
        command: "echo k".into(),
        shell: Some(HelperShell::Cmd),
    };
    let resolved = resolve_api_key(Some(&source), None, Some(&launcher));
    assert!(resolved.key.is_none(), "an unavailable shell yields no key");
    assert_eq!(
        launcher.spawn_count(),
        0,
        "no spawn when the shell cannot resolve"
    );
    assert!(
        resolved.warnings.iter().any(|w| w.field == "apiKeyHelper"),
        "the unavailable shell is reported: {:?}",
        resolved.warnings
    );
}

/// A helper whose named shell was not recognized is refused before any spawn,
/// and the environment answers instead. The command text never reaches another
/// interpreter.
#[test]
fn test_unknown_shell_refuses_spawn() {
    let launcher = RecordingLauncher::ok_stdout("k");
    let source = ApiKeySource::UnknownHelperShell {
        command: "Get-Secret".into(),
        named: "pwsh7".into(),
    };
    let resolved = resolve_api_key(
        Some(&source),
        Some(("DASHSCOPE_API_KEY", "env-key".into())),
        Some(&launcher),
    );
    assert_eq!(launcher.spawn_count(), 0, "an unknown shell never runs");
    assert_eq!(resolved.key.as_deref(), Some("env-key"));
    assert_eq!(resolved.origin.as_deref(), Some("env DASHSCOPE_API_KEY"));
    assert!(
        resolved
            .warnings
            .iter()
            .any(|w| w.field == "apiKeyHelperShell"),
        "the refusal is reported: {:?}",
        resolved.warnings
    );
}

/// One shell argument, not a split argv: splitting would break every helper
/// that uses a pipe or a redirect.
#[test]
fn test_runs_under_shell() {
    let launcher = RecordingLauncher::ok_stdout("k\n");
    let source = ApiKeySource::Helper {
        command: "cat /tmp/key | tr -d x".into(),
        shell: None,
    };
    drop(resolved_key(Some(&source), Some(&launcher)));
    let (req, _policy) = launcher.last();
    assert_eq!(req.program, "sh");
    assert_eq!(req.args, vec!["-c", "cat /tmp/key | tr -d x"]);
}

/// An explicit sh shell resolves to the POSIX shell and runs the command
/// through it, the same as the default.
#[test]
fn test_explicit_sh_shell_runs() {
    let launcher = RecordingLauncher::ok_stdout("k\n");
    let source = ApiKeySource::Helper {
        command: "cat /tmp/key | tr -d x".into(),
        shell: Some(HelperShell::Sh),
    };
    drop(resolved_key(Some(&source), Some(&launcher)));
    let (req, _policy) = launcher.last();
    assert_eq!(req.program, "sh");
    assert_eq!(req.args, vec!["-c", "cat /tmp/key | tr -d x"]);
}

/// The spawn carries a wall timeout; without one a helper that never exits
/// hangs startup.
#[test]
fn test_spawn_time_bounded() {
    let launcher = RecordingLauncher::ok_stdout("k\n");
    let source = ApiKeySource::Helper {
        command: "sleep-forever".into(),
        shell: None,
    };
    drop(resolved_key(Some(&source), Some(&launcher)));
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
    let source = ApiKeySource::Helper {
        command: "echo sk-inline-secret".into(),
        shell: None,
    };
    drop(resolved_key(Some(&source), Some(&launcher)));
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
    let source = ApiKeySource::Helper {
        command: "vault read".into(),
        shell: None,
    };
    assert_eq!(resolved_key(Some(&source), Some(&launcher)), None);
}

/// A timeout surfaces as a wait error, which must yield None.
#[test]
fn test_timeout_no_key() {
    let launcher =
        RecordingLauncher::with_exit(Err(SpawnError::Io("wall timeout exceeded".into())));
    let source = ApiKeySource::Helper {
        command: "sleep 600".into(),
        shell: None,
    };
    assert_eq!(resolved_key(Some(&source), Some(&launcher)), None);
}

/// Blank output yields no key; an empty string would fail at the provider.
#[test]
fn test_blank_output_no_key() {
    let launcher = RecordingLauncher::ok_stdout("   \n");
    let source = ApiKeySource::Helper {
        command: "true".into(),
        shell: None,
    };
    assert_eq!(resolved_key(Some(&source), Some(&launcher)), None);
}

/// With no source there is no command, so the env fallback costs no process.
#[test]
fn test_no_source_no_spawn() {
    let launcher = RecordingLauncher::ok_stdout("should-not-run\n");
    drop(resolved_key(None, Some(&launcher)));
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
    let source = ApiKeySource::Helper {
        command: "print-my-key".into(),
        shell: None,
    };
    drop(resolved_key(Some(&source), Some(&launcher)));
    assert_eq!(launcher.spawn_count(), 1);
}

/// A store that answers supplies the key, so the caller gets it and no
/// warning is recorded.
#[test]
fn test_keychain_value_is_key() {
    let mut warnings = Vec::new();
    let key = keychain_key(
        "houyicoder",
        "dashscope",
        SecretRead::Value("kc-secret".into()),
        &mut warnings,
    );
    assert_eq!(key.as_deref(), Some("kc-secret"));
    assert!(warnings.is_empty(), "a resolved key is quiet: {warnings:?}");
}

/// Every outcome but a value records a notice naming the item, so the user
/// can tell a missing entry from an unreadable store and fix the right one.
#[test]
fn test_keychain_misses_warn() {
    let outcomes = [
        SecretRead::NotFound,
        SecretRead::InvalidValue,
        SecretRead::UnsupportedPlatform,
        SecretRead::Unavailable,
    ];
    for outcome in outcomes {
        let mut warnings = Vec::new();
        let key = keychain_key("houyicoder", "dashscope", outcome, &mut warnings);
        assert!(key.is_none(), "an outcome other than a value yields no key");
        let warning = warnings
            .first()
            .expect("a store read that yields no key says so");
        assert_eq!(warning.field, "keychain");
        assert!(
            warning.reason.contains("houyicoder") && warning.reason.contains("dashscope"),
            "the notice names the item: {}",
            warning.reason
        );
    }
}

/// A platform with no keychain says that, rather than reporting the item as
/// missing: the user cannot fix a keychain that the host does not have.
#[test]
fn test_keychain_platform_warns() {
    let mut warnings = Vec::new();
    drop(keychain_key(
        "s",
        "a",
        SecretRead::UnsupportedPlatform,
        &mut warnings,
    ));
    assert!(
        warnings[0].reason.contains("no keychain"),
        "the notice names the cause: {}",
        warnings[0].reason
    );
}

/// A configured source that failed leaves the environment to answer, so the
/// origin reports the environment: naming the failed item would tell the user
/// their key comes from somewhere it does not.
#[test]
fn test_origin_env_after_failure() {
    let source = ApiKeySource::Keychain {
        service: "houyicoder".into(),
        account: "dashscope-team".into(),
    };
    assert_eq!(
        key_origin(true, Some(&source), Some("DASHSCOPE_API_KEY")).as_deref(),
        Some("env DASHSCOPE_API_KEY")
    );
}

/// A configured source that failed with no environment key leaves no key at
/// all, so there is no origin to report. This is the branch a keychain lands in
/// on a platform that cannot read one, and naming the item there would report a
/// credential the caller does not have.
#[test]
fn test_origin_absent_after_failure() {
    let source = ApiKeySource::Keychain {
        service: "houyicoder".into(),
        account: "dashscope-team".into(),
    };
    assert_eq!(key_origin(false, Some(&source), None), None);
}

/// The environment alone is a real origin: a user who exports the key and
/// configures no source still gets a truthful status line.
#[test]
fn test_origin_env_only() {
    assert_eq!(
        key_origin(true, None, Some("OPENAI_API_KEY")).as_deref(),
        Some("env OPENAI_API_KEY")
    );
}

/// A keychain source that supplied the key is named as the origin, so the
/// status line tells the user where their key came from.
#[test]
fn test_origin_names_keychain() {
    let source = ApiKeySource::Keychain {
        service: "houyicoder".into(),
        account: "dashscope".into(),
    };
    assert_eq!(
        key_origin(true, Some(&source), None).as_deref(),
        Some("keychain houyicoder/dashscope"),
        "a successful keychain read names the item as the origin"
    );
}

/// Nothing supplied a key, so there is no origin to report and the status line
/// says so rather than inventing one.
#[test]
fn test_origin_absent() {
    assert_eq!(key_origin(false, None, None), None);
}

/// With no configured source the environment is the only origin, and nothing
/// is spawned to look for one.
#[test]
fn test_no_source_env_only() {
    let launcher = RecordingLauncher::ok_stdout("unused\n");
    let resolved = resolve_api_key(
        None,
        Some(("DASHSCOPE_API_KEY", "env-key".into())),
        Some(&launcher),
    );
    assert_eq!(launcher.spawn_count(), 0, "no source means no spawn");
    assert_eq!(resolved.key.as_deref(), Some("env-key"));
    assert_eq!(resolved.origin.as_deref(), Some("env DASHSCOPE_API_KEY"));
}

/// Neither a source nor the environment supplied a key, so the origin stays
/// absent rather than naming a place the key did not come from.
#[test]
fn test_nothing_supplies_a_key() {
    let resolved = resolve_api_key(None, None, None);
    assert_eq!(resolved.key, None);
    assert_eq!(resolved.origin, None);
}

/// A configured source that fails, with no environment key behind it, leaves
/// both the key and the origin absent: the status line must not name an item
/// that did not supply anything.
#[test]
fn test_failed_source_no_origin() {
    let launcher = RecordingLauncher::with_exit(Ok(LauncherExit {
        exit_code: Some(44),
        stdout: Some(String::new()),
        stderr: Some("could not be found".into()),
    }));
    let source = ApiKeySource::Helper {
        command: "missing-key-helper".into(),
        shell: None,
    };
    let resolved = resolve_api_key(Some(&source), None, Some(&launcher));
    assert_eq!(resolved.key, None, "the helper yielded nothing");
    assert_eq!(
        resolved.origin, None,
        "an item that supplied no key is not an origin"
    );
    assert!(
        !resolved.warnings.is_empty(),
        "the failed source still has to say so"
    );
}
