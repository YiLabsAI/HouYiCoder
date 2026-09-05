//! Sandbox deny-log discovery. When a sandboxed command is blocked from
//! a mach service the macOS sandbox logs the denial to the unified log.
//! This module reads that log, extracts the denied service names, and
//! strips the Apple deny-list so only authorizable services surface.
//! On non-macOS hosts the reader returns empty.

/// A mach service name is alphanumeric plus dot, dash, underscore — the
/// same charset the seatbelt profile renderer accepts when emitting an
/// allow mach-lookup line. The unified log sometimes appends a metadata
/// blob to the service token with no whitespace separator (quoted JSON
/// like com.apple.x","global-name":...); a raw whitespace split keeps
/// the whole blob as the service. Truncating at the first char outside
/// the charset recovers the real service name and drops the blob tail.
fn truncate_service_name(name: &str) -> String {
    name.chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '.' || *c == '-' || *c == '_')
        .collect()
}

/// Parse denied mach-lookup service names from macOS log text. Only lines
/// that are actual sandbox denials are read — the kernel format is
/// Sandbox: proc(pid) deny(n) mach-lookup service, so a line must
/// contain deny( to count. This excludes unrelated log lines that merely
/// mention mach-lookup (an AppIntents message like
/// mach-lookup entitlement, will NOT register is not a denial and must
/// not surface a service named entitlement,). The service name is
/// truncated to the mach-name charset so a metadata blob cannot pose as
/// a service. Dedup.
pub fn parse_denied_services(log_text: &str) -> Vec<String> {
    let mut services = Vec::new();
    for line in log_text.lines() {
        if !line.contains("deny(") {
            continue;
        }
        if let Some(name) = extract_mach_service(line)
            && !name.is_empty()
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
/// Window-scoped: surfaces every mach-lookup denial in the window,
/// including from unrelated sandboxed processes — a short window bounds
/// that noise. The log attributes denials to sandboxd, not the denied
/// process, so pid correlation is unreliable.
///
/// Blocks on a synchronous subprocess (~0.7s); call from
/// spawn_blocking, never on a tokio worker.
pub fn discover_authorizable(window_secs: u64) -> Vec<String> {
    let text = read_deny_log(window_secs);
    authorizable_services(parse_denied_services(&text))
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
    let (raw_name, pid) = if let Some(paren) = token.find('(') {
        let name = &token[..paren];
        let pid_str = token[paren + 1..].trim_end_matches(')');
        (name, pid_str.parse::<u32>().ok())
    } else {
        (token, None)
    };
    let name = truncate_service_name(raw_name);
    if name.is_empty() {
        return None;
    }
    Some((name, pid))
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
        let log = "line1 deny(1) mach-lookup com.apple.system.logger(1)\nline2 deny(1) mach-lookup com.apple.system.logger(2)";
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
        let log = "deny(1) mach-lookup (123)";
        assert!(parse_denied_services(log).is_empty());
    }

    /// A line that mentions mach-lookup but is not a sandbox denial (an
    /// AppIntents complaint) must not surface a service. Without the
    /// deny-line filter this parsed a service named entitlement,.
    #[test]
    fn test_parse_skips_non_denial() {
        let log = "ego-browser[123]: Missing com.apple.linkd.application-service / com.apple.linkd.autoShortcut mach-lookup entitlement, will NOT register the process";
        assert!(parse_denied_services(log).is_empty());
    }

    /// A denial line whose service token carries a metadata blob with no
    /// whitespace separator must yield just the bare service name, then be
    /// dropped by the deny-list filter as a system service.
    #[test]
    fn test_parse_truncates_blob() {
        let log = "Sandbox: ego-browser(1) deny(1) mach-lookup com.apple.DiskArbitration.diskarbitrationd\",\"global-name\":\"com.apple.DiskArbitration.diskarbitrationd\",\"primary-filter-value\":\"com.apple.DiskArbitration.diskarbitrationd\"}";
        let parsed = parse_denied_services(log);
        assert_eq!(
            parsed,
            vec!["com.apple.DiskArbitration.diskarbitrationd".to_string()]
        );
        assert!(
            authorizable_services(parsed).is_empty(),
            "a deny-listed service extracted from a blob must not be offered"
        );
    }

    #[test]
    fn test_authorizable_strips_deny() {
        // Real log names are suffixed: pasteboard.1, cfprefsd.daemon. The
        // filter must drop the suffixed variants, not just the bare roots.
        let discovered = vec![
            "com.apple.pasteboard.1".to_string(),
            "com.citrolabs.ego.lite.ego-browser".to_string(),
            "com.apple.cfprefsd.daemon".to_string(),
            "com.apple.tccd.system".to_string(),
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
}
