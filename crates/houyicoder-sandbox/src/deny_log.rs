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

/// Parse denied mach-lookup service names from macOS sandbox log text.
/// A candidate must follow the adjacent kernel denial shape within a
/// Sandbox record; unrelated messages and file paths that merely contain
/// mach-lookup are ignored. Service names are truncated to the renderer's
/// accepted charset and deduplicated.
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
    let sandbox = line.find("Sandbox:")?;
    let record = &line[sandbox + "Sandbox:".len()..];
    let deny = record.find(" deny(")?;
    let after_open = &record[deny + " deny(".len()..];
    let close = after_open.find(')')?;
    let code = &after_open[..close];
    if code.is_empty() || !code.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let rest = after_open[close + 1..].strip_prefix(" mach-lookup ")?;
    let token = rest.split_whitespace().next()?;
    let raw_name = token.split_once('(').map_or(token, |(name, _)| name);
    let name = truncate_service_name(raw_name);
    (!name.is_empty()).then_some(name)
}

/// Strip the Apple deny-list from a set of discovered services. Only
/// non-deny-listed services survive — these are candidates the caller
/// may surface to the user for authorization.
pub fn authorizable_services(discovered: Vec<String>) -> Vec<String> {
    discovered
        .into_iter()
        .filter(|s| !houyicoder_api::skill::grant::is_denied(s))
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
            "eventMessage CONTAINS \"mach-lookup\" AND eventMessage CONTAINS \"deny(\"",
            "--style",
            "syslog",
        ],
    )
}

#[cfg(not(target_os = "macos"))]
fn read_deny_log(_window_secs: u64) -> String {
    String::new()
}

#[cfg(target_os = "macos")]
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
        let log = "2024-01-01 host kernel: Sandbox: ego-browser(123) deny(1) mach-lookup com.citrolabs.ego.lite.ego-browser(456)";
        let services = parse_denied_services(log);
        assert_eq!(
            services,
            vec!["com.citrolabs.ego.lite.ego-browser".to_string()]
        );
    }

    #[test]
    fn test_parse_dedup() {
        let log = "Sandbox: one(1) deny(1) mach-lookup com.apple.system.logger(1)\nSandbox: two(2) deny(1) mach-lookup com.apple.system.logger(2)";
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
        let log = "Sandbox: proc(1) deny(1) mach-lookup (123)";
        assert!(parse_denied_services(log).is_empty());
    }

    /// A line that mentions mach-lookup but is not a sandbox denial must
    /// not surface a service.
    #[test]
    fn test_parse_skips_non_denial() {
        let log = "ego-browser[123]: Missing application-service mach-lookup entitlement, will NOT register the process";
        assert!(parse_denied_services(log).is_empty());
    }

    /// A file denial whose attacker-controlled path contains mach-lookup
    /// must not be confused with an adjacent mach service denial.
    #[test]
    fn test_parse_rejects_path_injection() {
        let log = "kernel: Sandbox: sh(123) deny(1) file-read-data /workspace/mach-lookup com.citrolabs.injected";
        assert!(parse_denied_services(log).is_empty());
    }

    #[test]
    fn test_parse_requires_sandbox_sender() {
        let log = "process: deny(1) mach-lookup com.citrolabs.injected";
        assert!(parse_denied_services(log).is_empty());
    }

    #[test]
    fn test_parse_numeric_deny() {
        let log = "Sandbox: proc(1) deny(other) mach-lookup com.citrolabs.injected";
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

    #[cfg(target_os = "macos")]
    #[test]
    fn test_run_query_success() {
        // "true" succeeds with no stdout.
        let result = run_query("true", &[]);
        assert!(result.is_empty());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn test_run_query_failure() {
        // "false" exits non-zero; output() still succeeds (Err is only for
        // spawn failure, not non-zero exit).
        let result = run_query("false", &[]);
        assert!(result.is_empty());
    }

    #[cfg(target_os = "macos")]
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
}
