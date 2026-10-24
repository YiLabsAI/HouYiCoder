//! Which network destination a shell command reaches, and whether that
//! destination can be remembered for the session.
//!
//! The gate's network check is text-based: it recognizes the programs that
//! leave the machine and asks. A remembered answer must name the destination
//! the same way the check recognizes it, so this module derives one scope
//! string from a whole command, for example curl followed by the origin
//! https://api.github.com.
//!
//! The derivation is deliberately narrow. It only covers a literal, read-only
//! request to one origin: a single http or https URL, no upload, no retargeting
//! flag, no credential header, and no other egress segment pointing somewhere
//! else. Anything else returns None, which makes the caller ask every time
//! instead of remembering a scope wider than the command the user saw.

use std::mem::take;

use crate::compound::is_attestable;
use crate::compound::is_effect_safe;
use crate::compound::is_readonly_compound;
use crate::compound::is_temp_output_target;
use crate::compound::normalize_shell_command;
use crate::compound::sets_tmpdir;
use crate::compound::split_compound;
use crate::compound::starts_with_supported_command;
use crate::pipeline::detection::should_ask_egress;
use crate::pipeline::detection::strip_quotes;

const TRUSTED_HOSTS_JSON: &str = include_str!("../preapproved_hosts.json");

/// Public developer documentation and repository hosts whose read-only
/// requests do not require an interactive prompt in autonomous mode.
pub fn trusted_hosts() -> Vec<String> {
    serde_json::from_str::<Vec<String>>(TRUSTED_HOSTS_JSON).unwrap_or_default()
}

/// The session-scope allow entries corresponding to trusted_hosts.
pub fn trusted_egress_scopes() -> Vec<String> {
    let mut scopes = Vec::new();
    for host in trusted_hosts() {
        for tool in EGRESS_TOOLS {
            scopes.push(format!("{tool} https://{host}"));
        }
    }
    scopes
}

/// Tools this module can derive a destination for.
const EGRESS_TOOLS: &[&str] = &["curl", "wget"];

/// Long flags permitted on a read-only curl or wget request that take no
/// separate value argument.
const ALLOWED_BOOL_LONG: &[&str] = &[
    "--silent",
    "--show-error",
    "--fail",
    "--fail-with-body",
    "--head",
    "--include",
    "--compressed",
    "--http1.1",
    "--http2",
    "--ipv4",
    "--ipv6",
    "--no-buffer",
    "--no-progress-meter",
    "--progress-bar",
    "--quiet",
    "--server-response",
];

/// Long flags permitted on a read-only curl or wget request that consume a
/// value (either attached via = or as the next argument).
const ALLOWED_VALUE_LONG: &[&str] = &[
    "--max-time",
    "--connect-timeout",
    "--write-out",
    "--cacert",
    "--capath",
    "--user-agent",
    "--output",
    "--output-document",
];

/// Short flag letters permitted on a read-only curl or wget request that take
/// no value.
const ALLOWED_BOOL_SHORT: &[char] = &['s', 'S', 'f', 'I', 'i', 'N', 'q', '4', '6', 'v'];

/// Short flag letters permitted on a read-only curl or wget request that
/// consume the next argument as their value.
const ALLOWED_VALUE_SHORT: &[char] = &['m', 'w', 'A', 'o', 'O'];

/// Short flags that carry a header, whose value can hold a credential.
const HEADER_SHORT: char = 'H';
/// The long spelling of the header flag.
const HEADER_LONG: &str = "--header";

/// What one segment contributes to the command's destination.
enum Segment {
    /// The segment does not leave the machine.
    Local,
    /// The segment reaches the network but not in a form this module can name.
    Refused,
    /// The segment reaches one or more origins through one program.
    Scoped {
        program: String,
        origin_list: Vec<String>,
    },
}

/// The session scope a shell command's network access can be remembered under,
/// or None when the command cannot be narrowed to one read-only destination.
///
/// The result names the program and the origin, for example curl followed by
/// https://api.github.com, and is used both as the durable scope key and as the
/// text the approval card shows for its remember option.
pub fn egress_destination(command: &str) -> Option<String> {
    let list = collect_egress_scopes(command, false, false)?;
    if list.len() == 1 {
        list.into_iter().next()
    } else {
        None
    }
}

/// All read-only program-and-origin scopes a command reaches when every
/// non-network clause in the command is effect-safe. Used by the egress
/// validator to check whether every destination in a multi-step command has
/// already been approved or trusted.
pub fn egress_scope_list(command: &str, fenced: bool) -> Option<Vec<String>> {
    collect_egress_scopes(command, true, fenced)
}

/// Whether supported output arguments name only stdout, /dev/null, or (with an
/// active fence) one child of the session temporary directory. This classifies
/// arguments, not client startup configuration. Origin consent never grants
/// permission to open a local output file outside the session's own scratch
/// space, because the fence is what keeps that write inside the session.
pub fn egress_outputs_safe(command: &str, fenced: bool) -> bool {
    let can_use_temp = fenced && !sets_tmpdir(command);
    let normalized = normalize_shell_command(command);
    let mut preceding_supported = true;
    for clause in split_clauses(&normalized) {
        for stage in split_compound(&clause) {
            let words = shell_words(stage.trim());
            let Some(first) = words.first() else {
                continue;
            };
            let program = strip_quotes(first);
            if !EGRESS_TOOLS.contains(&program) {
                preceding_supported = preceding_supported && starts_with_supported_command(&stage);
                continue;
            }
            let allow_temp = can_use_temp && preceding_supported;
            let mut document_output = program == "curl";
            let mut idx = 1;
            while idx < words.len() {
                let w = strip_quotes(&words[idx]);
                if let Some(val) = w
                    .strip_prefix("--output=")
                    .or_else(|| w.strip_prefix("--output-document="))
                {
                    let v = strip_quotes(val);
                    if !output_target_safe(v, allow_temp) {
                        return false;
                    }
                    document_output = true;
                } else if matches!(w, "-o" | "-O" | "--output" | "--output-document") {
                    if program == "curl" && w == "-O" {
                        return false;
                    }
                    let v = words.get(idx + 1).map(|s| strip_quotes(s)).unwrap_or("");
                    if !output_target_safe(v, allow_temp) {
                        return false;
                    }
                    if program == "wget" && w == "-o" {
                        if v != "/dev/null" {
                            return false;
                        }
                    } else {
                        document_output = true;
                    }
                    idx += 2;
                    continue;
                }
                idx += 1;
            }
            if !document_output {
                return false;
            }
        }
    }
    true
}

/// Whether one output argument names stdout, /dev/null, or a fenced session
/// temporary child.
fn output_target_safe(value: &str, fenced: bool) -> bool {
    if value == "/dev/null" || value == "-" {
        return true;
    }
    fenced && is_temp_output_target(value)
}

fn collect_egress_scopes(
    command: &str,
    allow_effect_safe: bool,
    fenced: bool,
) -> Option<Vec<String>> {
    let normalized = normalize_shell_command(command);
    let mut scopes: Vec<String> = Vec::new();
    for clause in split_clauses(&normalized) {
        let stages: Vec<String> = split_compound(&clause);
        let stage_refs: Vec<&str> = stages.iter().map(|s| s.as_str()).collect();
        let piped = stages.len() > 1;
        for (idx, stage) in stages.iter().enumerate() {
            let trimmed = stage.trim();
            if trimmed.is_empty() {
                continue;
            }
            let stage_ok = if allow_effect_safe {
                is_attestable(trimmed) || is_effect_safe(trimmed, fenced)
            } else {
                is_attestable(trimmed)
            };
            if !stage_ok {
                return None;
            }
            match classify_segment(trimmed) {
                Segment::Local => {
                    if piped && idx > 0 && !is_readonly_compound(&stage_refs[idx..=idx]) {
                        return None;
                    }
                }
                Segment::Refused => return None,
                Segment::Scoped {
                    program,
                    origin_list,
                } => {
                    if piped
                        && !stage_refs[idx + 1..]
                            .iter()
                            .all(|s| is_readonly_compound(&[*s]))
                    {
                        return None;
                    }
                    for origin in origin_list {
                        let key = format!("{program} {origin}");
                        if !scopes.contains(&key) {
                            scopes.push(key);
                        }
                    }
                }
            }
        }
    }
    if scopes.is_empty() {
        None
    } else {
        Some(scopes)
    }
}

/// Split a normalized command on semicolons and logical operators outside
/// quotes and parentheses, keeping each pipeline together as one clause so
/// downstream pipe stages of an egress command can be checked independently
/// of local pipelines in later statements.
fn split_clauses(command: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let chars: Vec<char> = command.chars().collect();
    let mut i = 0;
    let mut in_single = false;
    let mut in_double = false;
    let mut depth = 0i32;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            '(' if !in_single && !in_double => depth += 1,
            ')' if !in_single && !in_double && depth > 0 => depth -= 1,
            '&' | '|'
                if !in_single
                    && !in_double
                    && depth == 0
                    && i + 1 < chars.len()
                    && chars[i + 1] == c =>
            {
                let trimmed = cur.trim();
                if !trimmed.is_empty() {
                    out.push(trimmed.to_string());
                }
                cur.clear();
                i += 2;
                continue;
            }
            ';' if !in_single && !in_double && depth == 0 => {
                let trimmed = cur.trim();
                if !trimmed.is_empty() {
                    out.push(trimmed.to_string());
                }
                cur.clear();
                i += 1;
                continue;
            }
            _ => {}
        }
        cur.push(c);
        i += 1;
    }
    let trimmed = cur.trim();
    if !trimmed.is_empty() {
        out.push(trimmed.to_string());
    }
    out
}

/// Classify one command segment by where it reaches.
fn classify_segment(segment: &str) -> Segment {
    let words = shell_words(segment);
    if words
        .iter()
        .take_while(|w| w.contains('='))
        .any(|w| is_refused_egress_env(w))
    {
        return Segment::Refused;
    }
    let Some(first) = words.first() else {
        return Segment::Local;
    };
    let program = strip_quotes(first);
    if !EGRESS_TOOLS.contains(&program) {
        // A channel this module cannot name — git push, a package publish, an
        // interpreter-wrapped request — would ride along inside the remembered
        // scope, so the whole command stays unanswerable.
        return if should_ask_egress("bash", segment) {
            Segment::Refused
        } else {
            Segment::Local
        };
    }
    let arg_refs: Vec<&str> = words[1..].iter().map(|s| s.as_str()).collect();
    match literal_origin_list(&arg_refs, allow_multi_origin_for(program)) {
        Some(origin_list) => Segment::Scoped {
            program: program.to_string(),
            origin_list,
        },
        None => Segment::Refused,
    }
}

fn allow_multi_origin_for(program: &str) -> bool {
    matches!(program, "curl" | "wget")
}

fn is_refused_egress_env(word: &str) -> bool {
    let Some((name, _)) = strip_quotes(word).split_once('=') else {
        return false;
    };
    matches!(
        name,
        "CURL_HOME"
            | "HTTP_PROXY"
            | "HTTPS_PROXY"
            | "ALL_PROXY"
            | "http_proxy"
            | "https_proxy"
            | "all_proxy"
            | "GIT_SSH_COMMAND"
    )
}

/// Split a segment into words while keeping single- and double-quoted spans
/// intact, so a multi-word header argument stays one token.
fn shell_words(segment: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut in_single = false;
    let mut in_double = false;
    for c in segment.chars() {
        match c {
            '\'' if !in_double => {
                in_single = !in_single;
                cur.push(c);
            }
            '"' if !in_single => {
                in_double = !in_double;
                cur.push(c);
            }
            c if c.is_whitespace() && !in_single && !in_double => {
                if !cur.is_empty() {
                    words.push(take(&mut cur));
                }
            }
            _ => cur.push(c),
        }
    }
    if !cur.is_empty() {
        words.push(cur);
    }
    words
}

/// The single origin a request program's arguments name, or None when the
/// arguments upload, retarget, carry a credential, or name no single literal
/// URL.
fn literal_origin_list(args: &[&str], allow_multiple: bool) -> Option<Vec<String>> {
    let mut origin_list: Vec<String> = Vec::new();
    let mut index = 0;
    while index < args.len() {
        let word = strip_quotes(args[index]);
        if let Some(name) = word.strip_prefix("--") {
            let (flag_name, inline_val) = match name.split_once('=') {
                Some((f, v)) => (format!("--{f}"), Some(strip_quotes(v))),
                None => (format!("--{name}"), None),
            };
            if ALLOWED_BOOL_LONG.contains(&flag_name.as_str()) {
                if inline_val.is_some() {
                    return None;
                }
            } else if flag_name == HEADER_LONG {
                let value = match inline_val {
                    Some(v) => v,
                    None => {
                        let next = args.get(index + 1).map(|v| strip_quotes(v))?;
                        index += 1;
                        next
                    }
                };
                if carries_credential(value) || value.starts_with('@') || value.contains(['$', '`'])
                {
                    return None;
                }
            } else if ALLOWED_VALUE_LONG.contains(&flag_name.as_str()) {
                let value = match inline_val {
                    Some(v) => v,
                    None => {
                        let next = args.get(index + 1).map(|v| strip_quotes(v))?;
                        index += 1;
                        next
                    }
                };
                if !is_safe_flag_value(&flag_name, value) {
                    return None;
                }
            } else {
                return None;
            }
        } else if let Some(cluster) = word.strip_prefix('-')
            && !cluster.is_empty()
        {
            if cluster.len() == 1 {
                let ch = cluster.chars().next()?;
                if ALLOWED_BOOL_SHORT.contains(&ch) {
                    // Pure read-only boolean flag.
                } else if ch == HEADER_SHORT {
                    let value = args.get(index + 1).map(|v| strip_quotes(v))?;
                    if carries_credential(value)
                        || value.starts_with('@')
                        || value.contains(['$', '`'])
                    {
                        return None;
                    }
                    index += 2;
                    continue;
                } else if ALLOWED_VALUE_SHORT.contains(&ch) {
                    let value = args.get(index + 1).map(|v| strip_quotes(v))?;
                    let mapped = match ch {
                        'o' | 'O' => "--output",
                        'w' => "--write-out",
                        'A' => "--user-agent",
                        _ => "",
                    };
                    if !is_safe_flag_value(mapped, value) {
                        return None;
                    }
                    index += 2;
                    continue;
                } else {
                    return None;
                }
            } else if !cluster.chars().all(|c| ALLOWED_BOOL_SHORT.contains(&c)) {
                return None;
            }
        } else if let Some(found) = http_origin(word) {
            if !origin_list.contains(&found) {
                if !allow_multiple && !origin_list.is_empty() {
                    return None;
                }
                origin_list.push(found);
            }
        } else {
            return None;
        }
        index += 1;
    }
    if origin_list.is_empty() {
        None
    } else {
        Some(origin_list)
    }
}

fn is_safe_flag_value(flag: &str, value: &str) -> bool {
    if value.is_empty() || value.contains('`') || value.contains("$(") {
        return false;
    }
    match flag {
        "--write-out" => {
            !value.starts_with('@')
                && !value.contains('$')
                && !value.to_ascii_lowercase().contains("%output")
        }
        "--user-agent" => !value.contains('$'),
        "--output" | "--output-document" => {
            if value == "/dev/null" || value == "-" {
                return true;
            }
            let suffix = value
                .strip_prefix("$TMPDIR/")
                .or_else(|| value.strip_prefix("${TMPDIR}/"));
            suffix.is_some_and(|s| {
                !s.is_empty()
                    && s != "."
                    && s != ".."
                    && !s.contains(['/', '*', '?', '[', '$', '`', '('])
            })
        }
        _ => true,
    }
}

/// The origin of a literal http or https URL: scheme plus host, with the
/// default port for that scheme dropped. None for anything a static check
/// cannot read as a plain URL.
pub(crate) fn http_origin(url: &str) -> Option<String> {
    // A character the shell would expand or escape means the text does not
    // name the destination the process will use. It is checked over the whole
    // URL because an expansion in the path can extend the host.
    if url.contains(['$', '`', '\\', '"', '\'']) {
        return None;
    }
    let (scheme, rest) = url.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let authority = rest.split(['/', '?', '#']).next()?;
    if authority.is_empty() || authority.contains('@') {
        return None;
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) if port.chars().all(|c| c.is_ascii_digit()) => (host, Some(port)),
        _ => (authority, None),
    };
    if host.is_empty() {
        return None;
    }
    let default_port = if scheme == "https" { "443" } else { "80" };
    match port {
        Some(port) if port != default_port => Some(format!("{scheme}://{host}:{port}")),
        _ => Some(format!("{scheme}://{host}")),
    }
}

/// Whether a header value names a credential rather than a content
/// preference.
fn carries_credential(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    lower.contains("authorization")
        || lower.contains("cookie")
        || lower.contains("token")
        || lower.contains("bearer")
        || lower.contains("api-key")
        || lower.contains("apikey")
}

#[cfg(test)]
mod tests;
