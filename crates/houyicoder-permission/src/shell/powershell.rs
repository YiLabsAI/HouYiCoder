//! A read-only classifier for a single PowerShell command.
//!
//! The gate lets a pure-literal read-only cmdlet run without approval, so a
//! model reading the repository costs no prompt. Anything dynamic -- a
//! pipeline, a redirect, a substitution, a type reference, or a parameter
//! whose value is an expression -- is judged as needing approval. The decision
//! is pure over the command text, so every edge runs on a host with no
//! PowerShell, and a benchmark pins the cost well under a prompt's budget.

#[cfg(test)]
mod tests;

/// Whether a PowerShell command is a pure-literal read-only invocation that may
/// run without approval.
///
/// The command must be a single statement (no pipeline or sequence), carry no
/// redirect, substitution, parenthesized subexpression, or type reference, and
/// name a read-only cmdlet with only safe flags. A path argument must be a
/// literal, not an expression; the authorized-area check the gate already runs
/// decides whether that path is one the user meant to expose.
pub fn read_only(command: &str) -> bool {
    let trimmed = command.trim();
    if trimmed.is_empty() || has_dynamic_construct(trimmed) {
        return false;
    }
    is_read_only_cmdlet(trimmed)
}

/// Whether the text carries a construct that makes a command dynamic rather than
/// a literal read. Each of these turns a read-only-looking cmdlet into one whose
/// effect the gate cannot bound from the text alone. A parenthesized
/// subexpression is the form an arbitrary command hides behind a read-only
/// cmdlet name, so both parentheses are rejected alongside the rest.
fn has_dynamic_construct(text: &str) -> bool {
    text.chars().any(|c| {
        matches!(
            c,
            ';' | '|' | '&' | '>' | '<' | '`' | '$' | '(' | ')' | '{' | '}' | '[' | ']'
        )
    }) || has_dynamic_parameter(text)
}

/// Whether a parameter is given an expression value, the form a colon binds a
/// parameter to an expression the gate cannot read: a flag letter, a colon, and
/// a value that begins with a dollar or a parenthesis.
fn has_dynamic_parameter(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'-' && index + 1 < bytes.len() && bytes[index + 1].is_ascii_alphabetic()
        {
            // Walk the flag name to the colon or the next separator.
            let mut name = index + 1;
            while name < bytes.len() && bytes[name].is_ascii_alphanumeric() {
                name += 1;
            }
            if name < bytes.len() && bytes[name] == b':' {
                let value = bytes.get(name + 1).copied();
                if value == Some(b'$') || value == Some(b'(') {
                    return true;
                }
            }
            index = name;
            continue;
        }
        index += 1;
    }
    false
}

/// Whether the command names a read-only cmdlet and carries only safe flags. The
/// cmdlet is the first token; the rest are flags or a literal path.
fn is_read_only_cmdlet(command: &str) -> bool {
    let mut tokens = command.split_whitespace();
    let Some(cmdlet) = tokens.next() else {
        return false;
    };
    if !is_read_only_cmdlet_name(cmdlet) {
        return false;
    }
    // Every remaining token is a safe flag or a literal operand. A token that is
    // neither is a construct the gate does not whitelist.
    tokens.all(is_safe_token)
}

/// Whether a token names one of the read-only cmdlets the gate whitelists. The
/// comparison is case-insensitive; PowerShell is.
fn is_read_only_cmdlet_name(token: &str) -> bool {
    let upper = token.to_ascii_uppercase();
    matches!(
        upper.as_str(),
        "GET-LOCATION"
            | "GET-CHILDITEM"
            | "GET-CONTENT"
            | "GET-ITEM"
            | "SELECT-STRING"
            | "MEASURE-OBJECT"
            | "TEST-PATH"
            | "PWD"
    )
}

/// Whether a token is a safe flag or a literal operand. A flag begins with a
/// dash and names a safe parameter; an operand is any other literal token, which
/// the path-containment check judges separately.
fn is_safe_token(token: &str) -> bool {
    if let Some(rest) = token.strip_prefix('-') {
        if rest.is_empty() {
            return false;
        }
        let name = rest.split(':').next().unwrap_or(rest).to_ascii_uppercase();
        return matches!(
            name.as_str(),
            "PATH"
                | "LITERALPATH"
                | "NAME"
                | "FILTER"
                | "INCLUDE"
                | "EXCLUDE"
                | "RECURSE"
                | "DEPTH"
                | "FILE"
                | "DIRECTORY"
                | "HIDDEN"
                | "SYSTEM"
                | "READONLY"
                | "ERRORACTION"
                | "WARNINGACTION"
                | "VERBOSE"
                | "DEBUG"
                | "SIMPLEMATCH"
                | "CASESENSITIVE"
                | "ENCODING"
                | "ALLMATCHES"
                | "LIST"
                | "TOTALCOUNT"
                | "TAIL"
                | "HEAD"
                | "PATTERN"
        );
    }
    // An operand is a literal; the gate's path check decides whether it is in an
    // authorized area. A token with a quote is still a literal the shell parses.
    true
}
