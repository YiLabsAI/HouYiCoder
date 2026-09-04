//! Compound-command safety. A compound shell command (A && B || C) is only
//! auto-allowable when every segment is independently attestable: no file
//! redirect, no command substitution, no heredoc. A single un-attestable
//! segment escalates the whole command to Ask so the user inspects it. The
//! gate never auto-allows a compound command it cannot statically attest.
//!
//! The checks here are structural heuristics, not a full shell parser — they
//! catch the high-risk constructs (redirects, substitution, heredoc, process
//! substitution) that change which resources a command touches. A full grammar
//! is out of scope; the gate escalates anything ambiguous to Ask.

/// Split a compound command into its top-level segments on and/or, semicolon,
/// and pipe. A bare pipe counts as a segment boundary (each stage of a pipeline
/// is a separate attestable unit). Empty segments are dropped. Quote- and
/// paren-aware: a separator inside quotes or inside a command substitution
/// $(...) or group (...) is not a boundary, so a pipe inside a substitution
/// stays in one segment. A full grammar is out of scope; the gate escalates
/// anything ambiguous to Ask.
pub fn split_compound(cmd: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let chars: Vec<char> = cmd.chars().collect();
    let mut i = 0;
    let mut in_single = false;
    let mut in_double = false;
    let mut depth = 0i32;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\'' if !in_double => {
                in_single = !in_single;
                cur.push(c);
                i += 1;
                continue;
            }
            '"' if !in_single => {
                in_double = !in_double;
                cur.push(c);
                i += 1;
                continue;
            }
            _ => {}
        }
        if in_single || in_double {
            cur.push(c);
            i += 1;
            continue;
        }
        // Track parens so a separator inside $(...) or (...) does not split.
        if c == '(' {
            depth += 1;
            cur.push(c);
            i += 1;
            continue;
        }
        if c == ')' {
            if depth > 0 {
                depth -= 1;
            }
            cur.push(c);
            i += 1;
            continue;
        }
        if depth > 0 {
            cur.push(c);
            i += 1;
            continue;
        }
        // and/or operators: double ampersand or double pipe.
        if (c == '&' || c == '|') && i + 1 < chars.len() && chars[i + 1] == c {
            push_seg(&mut out, &cur);
            cur.clear();
            i += 2;
            continue;
        }
        // single semicolon or pipe as a boundary.
        if c == ';' || c == '|' {
            push_seg(&mut out, &cur);
            cur.clear();
            i += 1;
            continue;
        }
        cur.push(c);
        i += 1;
    }
    push_seg(&mut out, &cur);
    out
}

fn push_seg(out: &mut Vec<String>, s: &str) {
    let t = s.trim();
    if !t.is_empty() {
        out.push(t.into());
    }
}

/// Strip the redirect forms that do NOT touch a real file, so the
/// attestability scan only escalates on a redirect to a path. The
/// safe-redirect strip covers three forms: 2>&1 (stderr to stdout), an
/// optional source fd then > then /dev/null (discard stdout), and < then
/// /dev/null (discard stdin). The trailing boundary is mandatory — a
/// redirect to /dev/nullo must NOT match /dev/null as a prefix, else the
/// strip would hide a real file write and the redirect check would pass.
/// The expression here is a hand-rolled char scan.
fn strip_safe_redirects(content: &str) -> String {
    let chars: Vec<char> = content.chars().collect();
    let n = chars.len();
    let mut out = String::with_capacity(n);
    let mut i = 0;
    while i < n {
        if let Some(end) = match_safe_redirect(&chars, i) {
            i = end;
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

/// Try to match a safe-redirect form at chars[start..], returning the position
/// one past the match (the caller skips the whole match). Returns None when no
/// form matches or the trailing boundary (whitespace or end-of-input) fails.
/// NOT quote-aware: a redirect inside a quoted string is rare data, an
/// acceptable approximation.
fn match_safe_redirect(chars: &[char], start: usize) -> Option<usize> {
    let at = |i: usize| chars.get(i).copied();

    // Form A: 2 >& 1 — stderr to stdout. Source fd is literally 2, dest 1;
    // whitespace tolerated between every token. A leading whitespace run is
    // not required; the attestability result does not depend on it — only
    // the trailing boundary is load-bearing for correctness.
    if at(start) == Some('2') {
        let mut j = start + 1;
        while at(j) == Some(' ') || at(j) == Some('\t') {
            j += 1;
        }
        if at(j) == Some('>') {
            let mut k = j + 1;
            while at(k) == Some(' ') || at(k) == Some('\t') {
                k += 1;
            }
            if at(k) == Some('&') {
                let mut m = k + 1;
                while at(m) == Some(' ') || at(m) == Some('\t') {
                    m += 1;
                }
                if at(m) == Some('1') && is_boundary(at(m + 1)) {
                    return Some(m + 1);
                }
            }
        }
    }

    // Form B: [012]? > /dev/null — discard stdout, optional source fd.
    let mut j = start;
    if matches!(at(j), Some('0' | '1' | '2')) {
        j += 1;
    }
    while at(j) == Some(' ') || at(j) == Some('\t') {
        j += 1;
    }
    if at(j) == Some('>') {
        let mut k = j + 1;
        while at(k) == Some(' ') || at(k) == Some('\t') {
            k += 1;
        }
        if matches_dev_null(chars, k) && is_boundary(at(k + DEV_NULL_LEN)) {
            return Some(k + DEV_NULL_LEN);
        }
    }

    // Form C: < /dev/null — discard stdin. Tolerate leading whitespace so a
    // space before < is consumed with the redirect.
    let mut j = start;
    while at(j) == Some(' ') || at(j) == Some('\t') {
        j += 1;
    }
    if at(j) == Some('<') {
        let mut k = j + 1;
        while at(k) == Some(' ') || at(k) == Some('\t') {
            k += 1;
        }
        if matches_dev_null(chars, k) && is_boundary(at(k + DEV_NULL_LEN)) {
            // The leading whitespace (j - start) is part of the stripped span.
            return Some(k + DEV_NULL_LEN);
        }
    }

    None
}

const DEV_NULL_LEN: usize = 9;

/// True when the slice at start matches the literal /dev/null char sequence.
fn matches_dev_null(chars: &[char], start: usize) -> bool {
    const DEV_NULL: [char; DEV_NULL_LEN] = ['/', 'd', 'e', 'v', '/', 'n', 'u', 'l', 'l'];
    if start + DEV_NULL_LEN > chars.len() {
        return false;
    }
    chars[start..start + DEV_NULL_LEN] == DEV_NULL
}

/// The trailing boundary a safe redirect must satisfy: followed by whitespace
/// or end-of-input. This is the detail that prevents a prefix match on
/// /dev/nullo from stripping the redirect and hiding a file write.
fn is_boundary(next: Option<char>) -> bool {
    matches!(next, None | Some(' ' | '\t' | '\n' | '\r'))
}

/// Whether a single command segment is structurally attestable: free of file
/// redirects, command substitution, heredoc, and process substitution outside
/// quotes. A redirect or substitution inside quotes is data, not an operator.
///
/// Safe redirect forms (2>&1, > /dev/null, < /dev/null) are stripped first so
/// cargo test 2>&1 reads as attestable while cargo test > log.txt escalates.
pub fn is_attestable(segment: &str) -> bool {
    let stripped = strip_safe_redirects(segment);
    let mut scan = QuoteScan::new(&stripped);
    while let Some(c) = scan.next() {
        if scan.in_quote() {
            continue;
        }
        if c == '>' || c == '<' {
            return false;
        }
        if c == '`' {
            return false;
        }
        // Command substitution: dollar plus open paren.
        if c == '$' && scan.peek_next() == Some('(') {
            return false;
        }
    }
    true
}

/// Whether all segments are independently attestable. This is the gate's
/// per-segment safety predicate: returning false escalates the whole compound
/// command to Ask.
pub fn compound_safe(segments: &[&str]) -> bool {
    if segments.is_empty() {
        return false;
    }
    segments.iter().all(|s| is_attestable(s))
}

/// Commands whose effect is reading or printing, never writing or executing.
/// A segment whose first command token is in this set and whose only shell
/// constructs are safe redirects (stripped) and command substitution (whose
/// contents are themselves read-only) is read-only, so a pipe chain of
/// read-only commands with command substitution auto-allows in Auto. Absent
/// are passthrough builtins (command, env, exec, xargs) that run a wrapped
/// command, and commands with dangerous positional forms (date sets the
/// clock, hostname sets the host, printenv leaks secrets); the conditional
/// ones need per-flag validation a follow-up ports. They still ask, which
/// is safe.
const READONLY_COMMANDS: &[&str] = &[
    "cat", "head", "tail", "grep", "egrep", "fgrep", "rg", "strings", "ls", "wc", "file", "which",
    "whereis", "basename", "dirname", "realpath", "stat", "du", "df", "pwd", "uname", "whoami",
    "id", "groups", "echo", "printf", "seq", "test", "true", "false",
];

/// Whether a compound command is entirely read-only: every segment's first
/// command token is in the read-only set, no segment writes a file (safe
/// /dev/null and stderr-to-stdout redirects are stripped first), and every
/// command substitution nests read-only commands (depth-limited,
/// fail-closed). The destructive validator's word scan catches destructive
/// verbs anywhere in the content, and the egress validator catches network
/// tools at the top level, so a read-only shell around a destructive
/// substitution still asks via the inner scan.
pub fn is_readonly_compound(segments: &[&str]) -> bool {
    is_readonly_compound_depth(segments, 3)
}

fn is_readonly_compound_depth(segments: &[&str], depth: u8) -> bool {
    if segments.is_empty() || depth == 0 {
        return false;
    }
    segments.iter().all(|s| is_readonly_segment(s, depth))
}

/// Whether one segment is read-only at the given recursion depth. Strips
/// safe redirects, rejects unquoted file redirects and process substitution,
/// extracts command-substitution and backtick bodies for recursive read-only
/// checks, and requires the first non-assignment token to be a read-only
/// command.
fn is_readonly_segment(seg: &str, depth: u8) -> bool {
    if depth == 0 {
        return false;
    }
    let stripped = strip_safe_redirects(seg);
    let chars: Vec<char> = stripped.chars().collect();
    let n = chars.len();
    let mut i = 0;
    let mut in_single = false;
    let mut in_double = false;
    let mut substs: Vec<String> = Vec::new();
    while i < n {
        let c = chars[i];
        match c {
            '\'' if !in_double => {
                in_single = !in_single;
                i += 1;
                continue;
            }
            '"' if !in_single => {
                in_double = !in_double;
                i += 1;
                continue;
            }
            _ => {}
        }
        if in_single || in_double {
            i += 1;
            continue;
        }
        // Unquoted file redirect or process substitution writes or talks;
        // not read-only. Safe redirects were already stripped.
        if c == '>' || c == '<' {
            return false;
        }
        // Command substitution: dollar plus open paren. Extract the body for
        // a recursive read-only check.
        if c == '$' && i + 1 < n && chars[i + 1] == '(' {
            let Some(end) = find_matching_paren(&chars, i + 1) else {
                return false;
            };
            substs.push(chars[i + 2..end].iter().collect());
            i = end + 1;
            continue;
        }
        // Backtick substitution. Extract the body for a recursive check.
        if c == '`' {
            let Some(rel) = chars[i + 1..].iter().position(|&ch| ch == '`') else {
                return false;
            };
            let end = i + 1 + rel;
            substs.push(chars[i + 1..end].iter().collect());
            i = end + 1;
            continue;
        }
        i += 1;
    }
    // First non-assignment command token must be a read-only command. Skip
    // leading env assignments (FOO=bar) the way the egress scan does.
    let first = stripped.split_whitespace().find(|t| !t.contains('='));
    let Some(cmd) = first else { return false };
    let cmd = crate::pipeline::detection::strip_quotes(cmd);
    if !READONLY_COMMANDS.contains(&cmd) {
        return false;
    }
    // Every substitution body is itself a read-only compound (split on pipes,
    // and-or, and semicolon so each stage is checked, depth-limited).
    for s in &substs {
        let segs = split_compound(s);
        let refs: Vec<&str> = segs.iter().map(|x| x.as_str()).collect();
        if !is_readonly_compound_depth(&refs, depth - 1) {
            return false;
        }
    }
    true
}

/// Find the index of the close paren matching the open paren at the given
/// position, quote-aware so a paren inside quotes is not counted. Returns
/// None when unbalanced (the caller fails closed).
fn find_matching_paren(chars: &[char], open: usize) -> Option<usize> {
    let mut depth = 0;
    let mut in_single = false;
    let mut in_double = false;
    let mut i = open;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            _ => {
                if !in_single && !in_double {
                    if c == '(' {
                        depth += 1;
                    } else if c == ')' {
                        depth -= 1;
                        if depth == 0 {
                            return Some(i);
                        }
                    }
                }
            }
        }
        i += 1;
    }
    None
}

/// A quote-aware char scanner. Tracks single and double quote state so
/// operators inside quotes are not mistaken for shell operators. Ambiguous
/// quoting (a single quote inside double quotes and vice versa) is handled by
/// the simple in-single / in-double toggle — good enough for the high-risk
/// constructs; ambiguous input still escalates to Ask.
struct QuoteScan {
    chars: Vec<char>,
    pos: usize,
    in_single: bool,
    in_double: bool,
}

impl QuoteScan {
    fn new(s: &str) -> Self {
        Self {
            chars: s.chars().collect(),
            pos: 0,
            in_single: false,
            in_double: false,
        }
    }

    fn next(&mut self) -> Option<char> {
        let c = self.chars.get(self.pos).copied()?;
        match c {
            '\'' if !self.in_double => self.in_single = !self.in_single,
            '"' if !self.in_single => self.in_double = !self.in_double,
            _ => {}
        }
        self.pos += 1;
        Some(c)
    }

    fn peek_next(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn in_quote(&self) -> bool {
        self.in_single || self.in_double
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_split_simple_segments() {
        assert_eq!(split_compound("ls && echo hi"), vec!["ls", "echo hi"]);
    }

    #[test]
    fn test_split_or_pipe() {
        assert_eq!(split_compound("a || b"), vec!["a", "b"]);
    }

    #[test]
    fn test_split_semicolon() {
        assert_eq!(split_compound("a; b ;c"), vec!["a", "b", "c"]);
    }

    #[test]
    fn test_split_pipe() {
        assert_eq!(split_compound("a | b | c"), vec!["a", "b", "c"]);
    }

    #[test]
    fn test_split_drops_empty() {
        assert_eq!(split_compound("a && && b"), vec!["a", "b"]);
    }

    #[test]
    fn test_attestable_simple_command() {
        assert!(is_attestable("ls -la"));
        assert!(is_attestable("grep foo bar.txt"));
    }

    #[test]
    fn test_quoted_redirect_attestable() {
        // An angle bracket inside quotes is data, not a redirect.
        assert!(is_attestable("echo 'a > b'"));
        assert!(is_attestable(r#"echo "a > b""#));
    }

    #[test]
    fn test_unattestable_redirect_out() {
        assert!(!is_attestable("echo hi > /tmp/x"));
    }

    #[test]
    fn test_unattestable_redirect_append() {
        assert!(!is_attestable("echo hi >> /tmp/x"));
    }

    #[test]
    fn test_input_redirect_unattestable() {
        assert!(!is_attestable("cat < /etc/passwd"));
    }

    #[test]
    fn test_attestable_fd_redirect() {
        // stderr to stdout never writes a file, so it is safe. Other fd
        // combinations (1>&2, >&2) are NOT in the safe set — only 2>&1 is
        // stripped, the rest escalate.
        assert!(is_attestable("cargo test 2>&1"));
        assert!(!is_attestable("cargo test 1>&2"));
        assert!(!is_attestable("make check >&2"));
    }

    #[test]
    fn test_attestable_dev_null_discard() {
        assert!(is_attestable("cargo test > /dev/null"));
        assert!(
            is_attestable("cargo test 2>/dev/null") || is_attestable("cargo test 2> /dev/null")
        );
        assert!(is_attestable("grep foo bar < /dev/null"));
    }

    #[test]
    fn test_unattestable_dev_null_prefix() {
        // The trailing boundary is load-bearing: > /dev/nullo must NOT match
        // /dev/null as a prefix (else the strip hides a real file write).
        assert!(!is_attestable("echo hi > /dev/nullo"));
    }

    #[test]
    fn test_unattestable_heredoc() {
        assert!(!is_attestable("cat <<EOF"));
    }

    #[test]
    fn test_unattestable_cmd_subst_dollar() {
        assert!(!is_attestable("echo $(whoami)"));
    }

    #[test]
    fn test_unattestable_cmd_subst_backtick() {
        assert!(!is_attestable("echo `whoami`"));
    }

    #[test]
    fn test_compound_safe_all_attestable() {
        assert!(compound_safe(&["ls", "grep foo"]));
    }

    #[test]
    fn test_compound_safe_one_unattestable() {
        assert!(!compound_safe(&["ls", "echo hi > /tmp/x"]));
    }

    #[test]
    fn test_empty_compound_unsafe() {
        assert!(!compound_safe(&[]));
    }

    #[test]
    fn test_readonly_pipe_chain_subst() {
        // The user-reported case: a pipe chain of read-only commands with a
        // which-substitution and a stderr-to-dev-null redirect auto-allows.
        let cmd = "strings $(which ego-browser) 2>/dev/null | grep -i 'ego' | head -40";
        let segs = split_compound(cmd);
        let refs: Vec<&str> = segs.iter().map(|s| s.as_str()).collect();
        assert!(is_readonly_compound(&refs), "read-only chain should allow");
    }

    #[test]
    fn test_readonly_single_subst() {
        // A single read-only command with a read-only substitution.
        let segs = split_compound("echo $(whoami)");
        let refs: Vec<&str> = segs.iter().map(|s| s.as_str()).collect();
        assert!(is_readonly_compound(&refs));
    }

    #[test]
    fn test_readonly_nested_subst() {
        // Substitution whose body is itself a compound of read-only commands.
        let segs = split_compound("head -5 $(grep foo bar | head -3)");
        let refs: Vec<&str> = segs.iter().map(|s| s.as_str()).collect();
        assert!(is_readonly_compound(&refs));
    }

    #[test]
    fn test_not_readonly_file_redirect() {
        // A file redirect to a real path writes; not read-only.
        let segs = split_compound("echo hi > /tmp/x");
        let refs: Vec<&str> = segs.iter().map(|s| s.as_str()).collect();
        assert!(!is_readonly_compound(&refs));
    }

    #[test]
    fn test_not_readonly_destructive_subst() {
        // A substitution body whose command is not in the read-only set
        // (rm here) is not read-only.
        let segs = split_compound("strings $(rm -rf /tmp)");
        let refs: Vec<&str> = segs.iter().map(|s| s.as_str()).collect();
        assert!(!is_readonly_compound(&refs));
    }

    #[test]
    fn test_not_readonly_network_subst() {
        // A network tool in a substitution is not read-only.
        let segs = split_compound("strings $(curl http://evil.com)");
        let refs: Vec<&str> = segs.iter().map(|s| s.as_str()).collect();
        assert!(!is_readonly_compound(&refs));
    }

    #[test]
    fn test_not_readonly_write_command() {
        // A write/exec command as the first token is not read-only.
        let segs = split_compound("sed -i 's/x/y/' file | grep y");
        let refs: Vec<&str> = segs.iter().map(|s| s.as_str()).collect();
        assert!(!is_readonly_compound(&refs));
    }

    #[test]
    fn test_unbalanced_paren_fails_closed() {
        // An unbalanced command substitution must not be treated as read-only.
        let segs = split_compound("echo $(whoami");
        let refs: Vec<&str> = segs.iter().map(|s| s.as_str()).collect();
        assert!(!is_readonly_compound(&refs));
    }

    #[test]
    fn test_split_quoted_pipe_nosplit() {
        // A pipe inside quotes is data, not a segment boundary.
        let segs = split_compound("echo 'a|b' | cat");
        let refs: Vec<&str> = segs.iter().map(|s| s.as_str()).collect();
        assert_eq!(refs.len(), 2, "quoted pipe must not split: {refs:?}");
        assert!(
            refs.iter()
                .all(|s| !s.contains("'a|b'") || s.contains("echo"))
        );
    }

    #[test]
    fn test_not_readonly_passthrough_command() {
        // command is a passthrough builtin: it runs the wrapped command, so
        // it is not in the read-only set (would let command-curl through).
        let segs = split_compound("command whoami");
        let refs: Vec<&str> = segs.iter().map(|s| s.as_str()).collect();
        assert!(!is_readonly_compound(&refs));
    }

    #[test]
    fn test_not_readonly_date_positional() {
        // date has a dangerous positional form (sets the clock); excluded.
        let segs = split_compound("date 01010000");
        let refs: Vec<&str> = segs.iter().map(|s| s.as_str()).collect();
        assert!(!is_readonly_compound(&refs));
    }

    #[test]
    fn test_not_readonly_hostname_positional() {
        // hostname has a dangerous positional form (sets the host); excluded.
        let segs = split_compound("hostname foo");
        let refs: Vec<&str> = segs.iter().map(|s| s.as_str()).collect();
        assert!(!is_readonly_compound(&refs));
    }
}
