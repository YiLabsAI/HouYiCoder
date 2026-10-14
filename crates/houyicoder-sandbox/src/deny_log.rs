//! Sandbox deny-log discovery. When a sandboxed command is blocked from
//! a mach service the denial lands in the macOS unified log in one of
//! two record shapes: the kernel sandbox report (Sandbox: ... deny
//! records), or launchd's own denied lookup record, which is what a
//! blocked bootstrap lookup produces on recent macOS releases. This
//! module reads that log, extracts the denied service names from both
//! shapes, and strips the Apple deny-list so only authorizable services
//! surface. On non-macOS hosts the reader returns empty.

use houyicoder_api::skill::grant::is_denied;
#[cfg(target_os = "macos")]
use std::process::Command;

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
    discovered.into_iter().filter(|s| !is_denied(s)).collect()
}

/// Read denied mach-lookup services from the macOS unified log within a
/// time window, then strip the deny-list. Returns authorizable
/// candidates the caller may offer to authorize. On non-macOS, empty.
///
/// Window-scoped: surfaces every mach-lookup denial in the window,
/// including from unrelated sandboxed processes — a short window bounds
/// that noise. Candidates surface by service name only.
///
/// Blocks on a synchronous subprocess; call from spawn_blocking, never
/// on a tokio worker. The window is floored at one second.
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

/// Build the unified-log query arguments for a window. The floor keeps a
/// zero window from degenerating into an archive-wide scan: the log tool
/// treats zero as the whole archive rather than an empty result, and
/// archive-wide queries grow with host log volume.
#[cfg(target_os = "macos")]
fn query_args(window_secs: u64) -> Vec<String> {
    let window = format!("{}s", window_secs.max(1));
    vec![
        "show".to_string(),
        "--last".to_string(),
        window,
        "--predicate".to_string(),
        DENY_LOG_PREDICATE.to_string(),
        "--style".to_string(),
        "syslog".to_string(),
    ]
}

#[cfg(target_os = "macos")]
fn read_deny_log(window_secs: u64) -> String {
    run_query("log", &query_args(window_secs))
}

#[cfg(not(target_os = "macos"))]
fn read_deny_log(_window_secs: u64) -> String {
    String::new()
}

#[cfg(target_os = "macos")]
fn run_query(cmd: &str, args: &[String]) -> String {
    #[expect(clippy::disallowed_methods, reason = "infra query, not model-driven")]
    match Command::new(cmd).args(args).output() {
        Ok(o) => String::from_utf8_lossy(&o.stdout).into_owned(),
        Err(e) => {
            tracing::warn!("sandbox deny-log query failed: {e}");
            String::new()
        }
    }
}

#[cfg(test)]
mod tests;
