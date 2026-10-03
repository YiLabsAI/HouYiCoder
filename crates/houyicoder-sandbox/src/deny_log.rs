//! Sandbox deny-log discovery. When a sandboxed command is blocked from
//! a mach service the denial lands in the macOS unified log in one of
//! two record shapes: the kernel sandbox report (Sandbox: ... deny
//! records), or launchd's own denied lookup record, which is what a
//! blocked bootstrap lookup produces on recent macOS releases. This
//! module reads that log, extracts the denied service names from both
//! shapes, and strips the Apple deny-list so only authorizable services
//! surface. On non-macOS hosts the reader returns empty.

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
/// A candidate must follow one of the two denial record shapes: the
/// kernel denial adjacent to a Sandbox record, or a launchd denied
/// lookup caused by a sandbox restriction. Unrelated messages and file
/// paths that merely contain mach-lookup are ignored. Service names are
/// truncated to the renderer's accepted charset and deduplicated. Shape
/// identification is a substring check over untrusted log text; the
/// authorization flow consuming these candidates gates on explicit user
/// confirmation.
pub fn parse_denied_services(log_text: &str) -> Vec<String> {
    let mut services = Vec::new();
    for line in log_text.lines() {
        if let Some(name) = extract_mach_service(line).or_else(|| extract_launchd_denial(line))
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

/// Parse one launchd denied-lookup record. launchd enforces the seatbelt
/// mach-lookup rule for bootstrap lookups and logs the denial itself: the
/// service follows the name = marker and the cause reads Sandbox
/// restriction. Three guards bound the record: the line must carry the
/// launchd pid-1 sender token; a line carrying the kernel Sandbox marker
/// belongs to the kernel shape, so launchd text embedded there (a file
/// path in a file denial) is data, not a record; and the cause must be
/// the sandbox, since a denial for any other cause offers nothing a
/// profile grant could fix.
fn extract_launchd_denial(line: &str) -> Option<String> {
    if line.contains("Sandbox:") {
        return None;
    }
    let sender = line.find("launchd[1]:")?;
    let record = &line[sender + "launchd[1]:".len()..];
    let marker = "denied lookup: name = ";
    let at = record.find(marker)?;
    let rest = &record[at + marker.len()..];
    if !rest.contains("Sandbox restriction") {
        return None;
    }
    let name = truncate_service_name(rest);
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
/// that noise. Candidates surface by service name only.
///
/// Blocks on a synchronous subprocess whose runtime scales with the
/// host's unified log store and is not bounded by the window; call
/// from spawn_blocking, never on a tokio worker.
pub fn discover_authorizable(window_secs: u64) -> Vec<String> {
    let text = read_deny_log(window_secs);
    authorizable_services(parse_denied_services(&text))
}

/// Unified-log query predicate covering both denial record shapes. The
/// launchd branch is scoped to sandbox-caused denials. The log records
/// each query's own command line, so a query also matches its own
/// invocation text; those lines fail the parser's sender and record-shape
/// checks.
#[cfg(target_os = "macos")]
const DENY_LOG_PREDICATE: &str = "(eventMessage CONTAINS \"mach-lookup\" AND eventMessage CONTAINS \"deny(\") OR (eventMessage CONTAINS \"denied lookup\" AND eventMessage CONTAINS \"Sandbox restriction\")";

#[cfg(target_os = "macos")]
fn read_deny_log(window_secs: u64) -> String {
    run_query(
        "log",
        &[
            "show",
            "--last",
            &format!("{window_secs}s"),
            "--predicate",
            DENY_LOG_PREDICATE,
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

    /// A launchd denied-lookup record captured verbatim from the host
    /// unified log: a blocked bootstrap lookup is enforced and logged by
    /// launchd itself, in its own record shape rather than the kernel
    /// Sandbox report.
    #[test]
    fn test_parse_launchd_denial() {
        let log = "2026-10-04 02:32:30.968952+0800  localhost launchd[1]: [gui/501 [100015]:] denied lookup: name = com.houyi.test.entitlement, flags = 0x1, requestor = machlookup[82725], error = 159: Sandbox restriction";
        let services = parse_denied_services(log);
        assert_eq!(services, vec!["com.houyi.test.entitlement".to_string()]);
    }

    /// A launchd lookup denial for a non-sandbox cause must not surface:
    /// no profile grant can fix it. A launchd line that is not a denial
    /// record at all must not surface either.
    #[test]
    fn test_parse_skips_non_sandbox() {
        let log = "localhost launchd[1]: [system:] denied lookup: name = com.example.svc, requestor = tool[42], error = 5: Input/output error\nlocalhost launchd[1]: [system:] notice: com.example.svc state changed";
        assert!(parse_denied_services(log).is_empty());
    }

    /// The launchd record shape must come from launchd itself, which is
    /// pid 1; another process logging the same text is not a denial.
    #[test]
    fn test_parse_requires_launchd() {
        let log = "evil[999]: denied lookup: name = com.evil.svc, flags = 0x1, error = 159: Sandbox restriction";
        assert!(parse_denied_services(log).is_empty());
    }

    /// A file denial whose attacker-controlled path embeds launchd
    /// record text must not surface a service: a line carrying the
    /// kernel Sandbox marker is a kernel record, and launchd text
    /// inside it is data, not a record.
    #[test]
    fn test_parse_rejects_embedded_launchd() {
        let log = "kernel: Sandbox: touch(123) deny(1) file-write-create /etc/launchd[1]: denied lookup: name = com.citrolabs.injected, error = 159: Sandbox restriction";
        assert!(parse_denied_services(log).is_empty());
    }

    /// The sandbox cause must follow the name marker: restriction text
    /// elsewhere on the line belongs to other message content and must
    /// not authenticate a denial recorded for another cause.
    #[test]
    fn test_parse_launchd_cause_order() {
        let log = "localhost launchd[1]: [system:] Sandbox restriction advisory; denied lookup: name = com.example.svc, error = 5: Input/output error";
        assert!(parse_denied_services(log).is_empty());
    }

    /// A launchd denial whose name field is empty yields nothing, the
    /// same as the kernel shape's empty-name case.
    #[test]
    fn test_parse_launchd_no_name() {
        let log = "localhost launchd[1]: [system:] denied lookup: name = , requestor = tool[7], error = 159: Sandbox restriction";
        assert!(parse_denied_services(log).is_empty());
    }

    /// Both record shapes in one log yield both services.
    #[test]
    fn test_parse_mixed_formats() {
        let log = "Sandbox: one(1) deny(1) mach-lookup com.legacy.svc(1)\nlocalhost launchd[1]: [system:] denied lookup: name = com.new.svc, error = 159: Sandbox restriction";
        let services = parse_denied_services(log);
        assert_eq!(
            services,
            vec!["com.legacy.svc".to_string(), "com.new.svc".to_string()]
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

    // Queries the host unified log through a real subprocess: the runtime
    // scales with the size of the log store, not with this test, so it
    // belongs to the ignored live suite rather than the unit gate. The
    // parse and filter logic is covered by the pure tests above.
    #[test]
    #[ignore = "host unified log query, runtime unbounded"]
    fn test_discover_does_not_panic() {
        // A 0-second window exercises the full pipeline (log show, parse,
        // filter) without asserting on the result — the log may contain
        // entries from other sandboxed processes on the host.
        discover_authorizable(0);
    }
}
