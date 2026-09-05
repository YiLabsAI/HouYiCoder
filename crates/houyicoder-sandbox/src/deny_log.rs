//! Sandbox deny-log discovery. When a sandboxed command is blocked from
//! a mach service the macOS sandbox logs the denial to the unified log.
//! This module reads that log, extracts the denied service names, and
//! strips the Apple deny-list so only authorizable services surface.
//! On non-macOS hosts the reader returns empty.

/// Parse denied mach-lookup service names from macOS log text. Each
/// line containing "mach-lookup <name>" yields one service. Dedup. The
/// trailing (PID) sandboxd appends is stripped.
pub fn parse_denied_services(log_text: &str) -> Vec<String> {
    let mut services = Vec::new();
    for line in log_text.lines() {
        if let Some(name) = extract_mach_service(line)
            && !services.contains(&name)
        {
            services.push(name);
        }
    }
    services
}

fn extract_mach_service(line: &str) -> Option<String> {
    extract_mach_service_and_pid(line).map(|(name, _)| name)
}

/// Strip the Apple deny-list from a set of discovered services. Only
/// non-deny-listed services survive — these are candidates the caller
/// may surface to the user for authorization.
pub fn authorizable_services(discovered: Vec<String>) -> Vec<String> {
    discovered
        .into_iter()
        .filter(|s| !houyicoder_api::skill_grant::is_denied(s))
        .collect()
}

/// Read denied mach-lookup services from the macOS unified log within a
/// time window, then strip the deny-list. Returns authorizable
/// candidates the caller may offer to authorize. On non-macOS, empty.
///
/// The scan is window-scoped, not pid-scoped: it surfaces every
/// mach-lookup denial in the window, including denials from unrelated
/// sandboxed processes on the host. The caller must post-filter by the
/// target exec pid before offering to authorize — otherwise a denial
/// from another process may be mis-attributed to this skill.
///
/// Blocks: runs a synchronous subprocess (log show, ~0.7s for a 1s
/// window). Call from spawn_blocking, never on a tokio worker.
pub fn discover_authorizable(window_secs: u64) -> Vec<String> {
    let text = read_deny_log(window_secs);
    authorizable_services(parse_denied_services(&text))
}

/// Read denied mach-lookup services for a specific process id within a
/// time window, then strip the deny-list. The pid is parsed from the
/// message text (the trailing (PID) sandboxd appends), not from the
/// log predicate — processID in the unified log is sandboxd itself,
/// not the denied process. Pid-scoped so denials from unrelated
/// sandboxed processes on the host do not surface. On non-macOS, empty.
///
/// Blocks: runs a synchronous subprocess (log show). Call from
/// spawn_blocking, never on a tokio worker.
pub fn discover_for_pid(pid: u32, window_secs: u64) -> Vec<String> {
    let text = read_deny_log(window_secs);
    authorizable_services(filter_by_pid(&text, pid))
}

/// Parse log text and keep only entries whose trailing (PID) matches.
/// Pure so the pid-filter logic is testable without running log show.
fn filter_by_pid(text: &str, pid: u32) -> Vec<String> {
    let mut services = Vec::new();
    for line in text.lines() {
        if let Some((name, Some(line_pid))) = extract_mach_service_and_pid(line)
            && line_pid == pid
            && !services.contains(&name)
        {
            services.push(name);
        }
    }
    services
}

#[cfg(target_os = "macos")]
fn read_deny_log(window_secs: u64) -> String {
    run_query(
        "log",
        &[
            "show",
            "--last",
            &format!("{window_secs}s"),
            "--predicate",
            "eventMessage CONTAINS \"mach-lookup\"",
            "--style",
            "syslog",
        ],
    )
}

#[cfg(not(target_os = "macos"))]
fn read_deny_log(_window_secs: u64) -> String {
    String::new()
}

fn extract_mach_service_and_pid(line: &str) -> Option<(String, Option<u32>)> {
    let pos = line.find("mach-lookup ")?;
    let rest = &line[pos + "mach-lookup ".len()..];
    let token = rest.split_whitespace().next()?;
    if let Some(paren) = token.find('(') {
        let name = &token[..paren];
        if name.is_empty() {
            return None;
        }
        let pid_str = token[paren + 1..].trim_end_matches(')');
        let pid = pid_str.parse::<u32>().ok();
        Some((name.to_string(), pid))
    } else if token.is_empty() {
        None
    } else {
        Some((token.to_string(), None))
    }
}

fn run_query(cmd: &str, args: &[&str]) -> String {
    #[expect(clippy::disallowed_methods, reason = "infra query, not model-driven")]
    match std::process::Command::new(cmd).args(args).output() {
        Ok(o) => String::from_utf8_lossy(&o.stdout).into_owned(),
        Err(e) => {
            tracing::warn!("sandbox deny-log query failed: {e}");
            String::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_extracts_service() {
        let log = "2024-01-01 host sandboxd[123]: deny(1) mach-lookup com.citrolabs.ego.lite.ego-browser(456)";
        let services = parse_denied_services(log);
        assert_eq!(
            services,
            vec!["com.citrolabs.ego.lite.ego-browser".to_string()]
        );
    }

    #[test]
    fn test_parse_dedup() {
        let log = "line1 mach-lookup com.apple.system.logger(1)\nline2 mach-lookup com.apple.system.logger(2)";
        let services = parse_denied_services(log);
        assert_eq!(services.len(), 1);
        assert_eq!(services[0], "com.apple.system.logger");
    }

    #[test]
    fn test_parse_no_match() {
        let log = "some unrelated log line\nanother line without mach";
        assert!(parse_denied_services(log).is_empty());
    }

    #[test]
    fn test_parse_empty_name() {
        let log = "mach-lookup (123)";
        assert!(parse_denied_services(log).is_empty());
    }

    #[test]
    fn test_authorizable_strips_deny() {
        let discovered = vec![
            "com.apple.pasteboard".to_string(),
            "com.citrolabs.ego.lite.ego-browser".to_string(),
            "com.apple.cfprefsd".to_string(),
        ];
        let result = authorizable_services(discovered);
        assert_eq!(
            result,
            vec!["com.citrolabs.ego.lite.ego-browser".to_string()]
        );
    }

    #[test]
    fn test_run_query_success() {
        // "true" succeeds with no stdout.
        let result = run_query("true", &[]);
        assert!(result.is_empty());
    }

    #[test]
    fn test_run_query_failure() {
        // "false" exits non-zero; output() still succeeds (Err is only for
        // spawn failure, not non-zero exit).
        let result = run_query("false", &[]);
        assert!(result.is_empty());
    }

    #[test]
    fn test_run_query_spawn_fail() {
        // A command that does not exist triggers the Err branch.
        let result = run_query("/nonexistent/binary/xyz", &[]);
        assert!(result.is_empty());
    }

    #[test]
    fn test_discover_does_not_panic() {
        // A 0-second window exercises the full pipeline (log show, parse,
        // filter) without asserting on the result — the log may contain
        // entries from other sandboxed processes on the host.
        discover_authorizable(0);
    }

    #[test]
    fn test_extract_service_and_pid() {
        let line = "deny(1) mach-lookup com.citrolabs.ego.lite.ego-browser(456)";
        let result = extract_mach_service_and_pid(line);
        assert_eq!(
            result,
            Some(("com.citrolabs.ego.lite.ego-browser".to_string(), Some(456)))
        );
    }

    #[test]
    fn test_extract_service_no_pid() {
        let line = "deny(1) mach-lookup com.apple.system.logger";
        let result = extract_mach_service_and_pid(line);
        assert_eq!(result, Some(("com.apple.system.logger".to_string(), None)));
    }

    #[test]
    fn test_extract_non_numeric_pid() {
        let line = "deny(1) mach-lookup com.apple.system.logger(abc)";
        let result = extract_mach_service_and_pid(line);
        assert_eq!(result, Some(("com.apple.system.logger".to_string(), None)));
    }

    #[test]
    fn test_discover_for_pid_zero() {
        // pid 0 never matches — no-suffix lines return None, not Some(0).
        assert!(discover_for_pid(0, 0).is_empty());
    }

    #[test]
    fn test_filter_pid_no_suffix() {
        // A line with no pid suffix must not match pid 0.
        let log = "deny(1) mach-lookup com.apple.system.logger";
        assert!(filter_by_pid(log, 0).is_empty());
    }

    #[test]
    fn test_filter_pid_match() {
        let log = "deny(1) mach-lookup com.citrolabs.ego.lite.ego-browser(456)\n\
                   deny(1) mach-lookup com.apple.system.logger(789)";
        let result = filter_by_pid(log, 456);
        assert_eq!(
            result,
            vec!["com.citrolabs.ego.lite.ego-browser".to_string()]
        );
    }

    #[test]
    fn test_filter_pid_miss() {
        let log = "deny(1) mach-lookup com.apple.system.logger(789)";
        assert!(filter_by_pid(log, 456).is_empty());
    }
}
