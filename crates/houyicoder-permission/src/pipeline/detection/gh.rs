//! Reading the GitHub client's own verb grammar.
//!
//! The client is one program with a group, a verb, and a set of flags, and
//! which of those makes a call a write is not readable from the command
//! position alone. It lives here so the network check stays about destinations.

use super::strip_quotes;

/// Whether a gh invocation only reads.
///
/// The group verbs are the inspection set. The api verb reads unless a method
/// or a field turns it into a write, which is the one form that would publish
/// on the user's behalf.
pub(super) fn gh_reads_only(args: &[&str]) -> bool {
    // Words arrive as written, so the group verb is folded before it is
    // matched. The flags below are folded individually, because the case of a
    // short flag carries nothing: -F and -f are the same field flag.
    let sub = strip_quotes(args.first().copied().unwrap_or("")).to_ascii_lowercase();
    let sub2 = strip_quotes(args.get(1).copied().unwrap_or("")).to_ascii_lowercase();
    if matches!(sub.as_str(), "version" | "help" | "status" | "search") {
        return true;
    }
    if sub == "auth" {
        return sub2 == "status";
    }
    if sub == "api" {
        let mut index = 0;
        while index < args.len() {
            let arg = strip_quotes(args[index]);
            index += 1;
            // A method is the one flag that decides read from write on its
            // own, so the value is read rather than the flag alone.
            let method = arg
                .strip_prefix("--method=")
                .map(str::to_string)
                .or_else(|| {
                    (arg == "--method").then(|| {
                        let next = args.get(index).map(|a| strip_quotes(a)).unwrap_or("");
                        index += 1;
                        next.to_string()
                    })
                })
                .or_else(|| {
                    // The caller folds the command to lower case before this
                    // walk sees it, so the short flag arrives as -x. Both
                    // spellings are accepted rather than assuming which.
                    let rest = arg.strip_prefix("-X").or_else(|| arg.strip_prefix("-x"))?;
                    if rest.is_empty() {
                        let next = args.get(index).map(|a| strip_quotes(a)).unwrap_or("");
                        index += 1;
                        return Some(next.to_string());
                    }
                    Some(rest.trim_start_matches('=').to_string())
                });
            if let Some(method) = method {
                let method = method.trim_start_matches('=').to_ascii_uppercase();
                if method.is_empty() || method == "GET" || method == "HEAD" {
                    continue;
                }
                return false;
            }
            // A field flag sends a value whatever its spelling: -f x, -fx,
            // -f=x, -F x. The bare flag and the attached form are the same
            // write, so the letter is what decides.
            let lower = arg.to_ascii_lowercase();
            if lower.starts_with("--field")
                || lower.starts_with("--raw-field")
                || lower.starts_with("--input")
                || lower.starts_with("-f")
            {
                return false;
            }
        }
        return true;
    }
    matches!(
        sub.as_str(),
        "pr" | "issue" | "repo" | "release" | "run" | "workflow" | "label" | "cache"
    ) && matches!(
        sub2.as_str(),
        "view" | "list" | "diff" | "checks" | "status"
    )
}
