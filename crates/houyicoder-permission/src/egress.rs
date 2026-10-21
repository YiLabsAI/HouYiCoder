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

use crate::compound::is_attestable;
use crate::compound::normalize_shell_command;
use crate::compound::split_compound;
use crate::pipeline::detection::should_ask_egress;
use crate::pipeline::detection::strip_quotes;

/// Programs this module can derive a destination for.
const EGRESS_PROGRAMS: &[&str] = &["curl", "wget"];

/// Long flags that turn the request into an upload, retarget it, or load
/// options from a file, so the literal URL no longer describes the request.
/// Matched by prefix so an inline value form is caught too.
const UNSCOPABLE_LONG: &[&str] = &[
    "--data",
    "--form",
    "--upload-file",
    "--post-data",
    "--post-file",
    "--body-data",
    "--body-file",
    "--json",
    "--method",
    "--request",
    "--config",
    "--proxy",
    "--connect-to",
    "--resolve",
    "--interface",
    "--unix-socket",
];

/// Short flags with the same effect. A short cluster is matched by the letter
/// it carries, so an attached value form is caught as well.
const UNSCOPABLE_SHORT: &[char] = &['d', 'F', 'T', 'K', 'x', 'X'];

/// Short flags that carry a header, whose value can hold a credential.
const HEADER_SHORT: char = 'H';
/// The long spelling of the header flag.
const HEADER_LONG: &str = "--header";

/// What one segment contributes to the command's destination.
enum Segment {
    /// The segment does not leave the machine.
    Local,
    /// The segment reaches the network but not in a form this module can name.
    Unscopable,
    /// The segment reaches one origin through one program.
    Scoped { program: String, origin: String },
}

/// The session scope a shell command's network access can be remembered under,
/// or None when the command cannot be narrowed to one read-only destination.
///
/// The result names the program and the origin, for example curl followed by
/// https://api.github.com, and is used both as the durable scope key and as the
/// text the approval card shows for its remember option.
pub fn bash_egress_scope(command: &str) -> Option<String> {
    let normalized = normalize_shell_command(command);
    // A pipeline hands the fetched bytes to another program, which the
    // remembered scope does not name. A read-only tail such as a line filter
    // is a common case this refuses; the same work stays available as two
    // statements, so refusing costs the user a rewrite rather than the task.
    if has_unquoted_pipe(&normalized) {
        return None;
    }
    let mut scope: Option<String> = None;
    for segment in split_compound(&normalized) {
        let trimmed = segment.trim();
        if trimmed.is_empty() {
            continue;
        }
        // Every segment has to be one the gate can reason about, or the
        // remembered scope would also cover shell syntax the user never saw
        // approved.
        if !is_attestable(trimmed) {
            return None;
        }
        match classify_segment(trimmed) {
            Segment::Local => {}
            Segment::Unscopable => return None,
            Segment::Scoped { program, origin } => {
                let key = format!("{program} {origin}");
                match &scope {
                    Some(existing) if existing != &key => return None,
                    Some(_) => {}
                    None => scope = Some(key),
                }
            }
        }
    }
    scope
}

/// Whether the command contains a pipe outside quotes.
fn has_unquoted_pipe(command: &str) -> bool {
    let mut in_single = false;
    let mut in_double = false;
    for c in command.chars() {
        match c {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            '|' if !in_single && !in_double => return true,
            _ => {}
        }
    }
    false
}

/// Classify one command segment by where it reaches.
fn classify_segment(segment: &str) -> Segment {
    let words: Vec<&str> = segment.split_whitespace().collect();
    let Some(first) = words.first() else {
        return Segment::Local;
    };
    let program = strip_quotes(first);
    if !EGRESS_PROGRAMS.contains(&program) {
        // A channel this module cannot name — git push, a package publish, an
        // interpreter-wrapped request — would ride along inside the remembered
        // scope, so the whole command stays unanswerable.
        return if should_ask_egress("bash", segment) {
            Segment::Unscopable
        } else {
            Segment::Local
        };
    }
    match literal_origin(&words[1..]) {
        Some(origin) => Segment::Scoped {
            program: program.to_string(),
            origin,
        },
        None => Segment::Unscopable,
    }
}

/// The single origin a request program's arguments name, or None when the
/// arguments upload, retarget, carry a credential, or name no single literal
/// URL.
fn literal_origin(args: &[&str]) -> Option<String> {
    let mut origin: Option<String> = None;
    let mut index = 0;
    while index < args.len() {
        let word = strip_quotes(args[index]);
        if let Some(name) = word.strip_prefix("--") {
            let flag = format!("--{}", name.split('=').next().unwrap_or(""));
            if UNSCOPABLE_LONG.contains(&flag.as_str()) {
                return None;
            }
            if flag == HEADER_LONG {
                let value = args.get(index + 1).map(|v| strip_quotes(v))?;
                if carries_credential(value) {
                    return None;
                }
                index += 2;
                continue;
            }
        } else if let Some(cluster) = word.strip_prefix('-')
            && !cluster.is_empty()
        {
            if cluster.chars().any(|c| UNSCOPABLE_SHORT.contains(&c)) {
                return None;
            }
            if cluster.contains(HEADER_SHORT) {
                let value = args.get(index + 1).map(|v| strip_quotes(v))?;
                if carries_credential(value) {
                    return None;
                }
                index += 2;
                continue;
            }
        } else if let Some(found) = http_origin(word) {
            // A second URL means the command reaches two destinations, which
            // one scope cannot describe.
            if origin.is_some() {
                return None;
            }
            origin = Some(found);
        }
        index += 1;
    }
    origin
}

/// Whether a header value names a credential rather than a content
/// preference.
fn carries_credential(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    lower.contains("authorization") || lower.contains("cookie") || lower.contains("token")
}

/// The origin of a literal http or https URL: scheme plus host, with the
/// default port for that scheme dropped. None for anything a static check
/// cannot read as a plain URL.
fn http_origin(url: &str) -> Option<String> {
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

#[cfg(test)]
mod tests;
