//! Tests for this module.

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

/// The floor keeps a zero window from degenerating into an
/// archive-wide scan: the log tool treats zero as the whole archive,
/// not an empty result.
#[cfg(target_os = "macos")]
#[test]
fn test_query_args_floors_window() {
    let args = query_args(0);
    assert_eq!(args[2], "1s");
    assert_eq!(query_args(5)[2], "5s");
}

// Queries the host unified log through a real subprocess: host-state
// probes belong to the ignored live suite whatever their runtime.
// The parse, filter, and query-building logic is covered by the pure
// tests above.
#[test]
#[ignore = "spawns a host unified log query"]
fn test_discover_does_not_panic() {
    // A zero window exercises the floor plus the full pipeline (log
    // show, parse, filter) without asserting on the result — the log
    // may contain entries from other sandboxed processes on the host.
    discover_authorizable(0);
}
