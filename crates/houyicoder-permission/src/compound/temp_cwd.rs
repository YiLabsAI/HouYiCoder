//! Where a compound command's working directory sits, and whether a path
//! written from there provably lands in the session scratch space.
//!
//! One state machine, so the network check and the removal check cannot drift
//! apart. Its reach ends at every separator that is not and-and: a semicolon, a
//! pipe, or a background ampersand runs its right side whether or not the change
//! on the left succeeded.

use crate::pipeline::detection::strip_quotes;
use crate::safety::marker_hit;

/// The whitespace words of a step, with quotes stripped so a quoted path is
/// read as the path it names.
fn first_words(text: &str) -> Vec<String> {
    text.split_whitespace()
        .map(|word| strip_quotes(word).to_string())
        .collect()
}

/// One top-level step of a compound command, with the separator that preceded
/// it. A pipe is not a top-level separator: a case pattern and a pipe look
/// alike, so a step holds its whole pipeline and reads the pipe per stage.
#[derive(Debug, Clone)]
pub(crate) struct Link {
    /// Whether and-and joins this step to the one before it, the only operator
    /// under which the left side running first is guaranteed.
    pub(crate) chained: bool,
    /// The step text joined by the separator it carried, trimmed.
    pub(crate) text: String,
}

/// Split a command into top-level steps, keeping the one fact about separators
/// the destination checks depend on. The public segment split discards the
/// operator, which is right for callers that read each step on its own.
pub(crate) fn chain_links(command: &str) -> Vec<Link> {
    let mut links = Vec::new();
    let mut cur = String::new();
    let chars: Vec<char> = command.chars().collect();
    let mut i = 0;
    let mut in_single = false;
    let mut in_double = false;
    let mut depth = 0i32;
    // The first step has no separator before it, so nothing can make its
    // result depend on an earlier step that might not have run.
    let mut chained = true;
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
        // A backslash makes the next character literal, so an escaped quote does not
        // end a quoted run and an escaped separator does not split the command.
        // It applies outside single quotes, where it is literal text.
        if c == '\\' && !in_single && chars.get(i + 1).is_some_and(|next| *next != '\n') {
            cur.push(c);
            if let Some(next) = chars.get(i + 1) {
                cur.push(*next);
            }
            i += 2;
            continue;
        }
        if in_single || in_double {
            cur.push(c);
            i += 1;
            continue;
        }
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
        // A brace group holds commands whose own separators the step reader
        // does not follow, so nothing inside it can be proven to run where a
        // change of directory outside it left the walk. The group ends the
        // reach instead of carrying it.
        if c == '{' || c == '}' {
            push_link(&mut links, &cur, chained);
            cur.clear();
            chained = false;
            i += 1;
            continue;
        }
        let doubled = chars.get(i + 1) == Some(&c);
        // A single ampersand that completes a redirect moves a stream rather
        // than starting a command, so the step continues through it.
        if c == '&' && !doubled && (chars.get(i + 1) == Some(&'>') || cur.ends_with('>')) {
            cur.push(c);
            i += 1;
            continue;
        }
        // A background ampersand starts the step before it and runs the rest
        // without waiting, so the steps are not ordered and nothing carries
        // across it.
        let is_and_and = c == '&' && doubled;
        // A pipe ends the reach through has_top_level_pipe, which reads it
        // without cutting the step, because a case pattern and a pipe look
        // alike and the readers that split a step on pipes need them together.
        let splits = c == '&' || (c == '|' && doubled) || c == ';' || c == '\n';
        if splits {
            push_link(&mut links, &cur, chained);
            cur.clear();
            chained = is_and_and;
            i += if matches!(c, '&' | '|') { 2 } else { 1 };
            continue;
        }
        cur.push(c);
        i += 1;
    }
    push_link(&mut links, &cur, chained);
    links
}

fn push_link(links: &mut Vec<Link>, cur: &str, chained: bool) {
    let text = cur.trim();
    if !text.is_empty() {
        links.push(Link {
            chained,
            text: text.to_string(),
        });
    }
}

/// The directory a change of directory names, or None when it names none.
///
/// A flag before the target is not a directory: cd -- marks the end of the
/// options, and cd -l likewise carries none, so either way the target is the
/// word after it. Only one target is read; a second operand is a form this
/// walk does not evaluate.
fn cd_target_of(words: &[String]) -> Option<String> {
    let mut operands = words.iter().skip(1).filter(|word| !word.starts_with('-'));
    let target = operands.next()?;
    if operands.next().is_some() {
        return None;
    }
    Some(strip_quotes(target).to_string())
}

/// Where the working directory sits, as far as the walk can prove.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TempCwd {
    /// Inside the session scratch directory: zero is its root and a positive
    /// depth is that many levels below it.
    Inside(u32),
    /// Anywhere the walk cannot vouch for.
    Outside,
}

/// Whether a step is a parenthesised group, which runs its commands in a
/// subshell whose working directory this walk cannot place. A command
/// substitution is not one: its body is walked on its own.
pub(crate) fn starts_a_subshell(text: &str) -> bool {
    text.trim_start().starts_with('(')
}

/// Whether a step holds a top-level pipe, which runs each side in its own
/// subshell. A change of directory on the left never reaches the right.
fn has_top_level_pipe(text: &str) -> bool {
    let chars: Vec<char> = text.chars().collect();
    let mut in_single = false;
    let mut in_double = false;
    let mut depth = 0i32;
    for (i, c) in chars.iter().enumerate() {
        match c {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            '(' if !in_single && !in_double => depth += 1,
            ')' if !in_single && !in_double && depth > 0 => depth -= 1,
            '|' if !in_single
                && !in_double
                && depth == 0
                && chars.get(i + 1) != Some(&'|')
                && (i == 0 || chars[i - 1] != '|') =>
            {
                return true;
            }
            _ => {}
        }
    }
    false
}

impl TempCwd {
    /// The state after a step. A change of directory not provably into the
    /// scratch space, and any separator other than and-and, both drop the walk
    /// back to outside.
    pub(crate) fn advance(self, link: &Link) -> Self {
        if !link.chained {
            return Self::Outside;
        }
        let words = first_words(&link.text);
        // A step whose body is a parenthesised group or a command substitution
        // runs its own commands in a subshell. What that body changes about its
        // own working directory never reaches this shell, and what this shell
        // established before it is not what the body starts from, so the step
        // is not one whose effect this walk can place.
        if starts_a_subshell(&link.text) {
            return Self::Outside;
        }
        let Some(first) = words.first().map(String::as_str) else {
            return self;
        };
        if matches!(first, "pushd" | "popd") {
            return Self::Outside;
        }
        if first != "cd" {
            return self;
        }
        // A change of directory in a pipeline runs in a subshell, so it does
        // not move the shell the later steps run in.
        if has_top_level_pipe(&link.text) {
            return Self::Outside;
        }
        match cd_target_of(&words) {
            Some(target) => Self::after_change(&target, self),
            // A bare cd returns to the home directory, which is not scratch.
            None => Self::Outside,
        }
    }

    fn after_change(target: &str, current: Self) -> Self {
        if let Some(depth) = scratch_depth(target) {
            return Self::Inside(depth);
        }
        match current {
            Self::Inside(depth) => match plain_depth(target) {
                Some(extra) => Self::Inside(depth + extra),
                None => Self::Outside,
            },
            Self::Outside => Self::Outside,
        }
    }

    /// Whether this state proves the path names a place inside the scratch
    /// directory.
    pub(crate) fn admits(self, path: &str) -> bool {
        if scratch_depth(path).is_some() {
            return true;
        }
        matches!(self, Self::Inside(_)) && plain_depth(path).is_some()
    }
}

/// The depth of a path spelled from the scratch root, or None when it does not
/// start there or carries a form the walk cannot read.
fn scratch_depth(path: &str) -> Option<u32> {
    let rest = path
        .strip_prefix("$TMPDIR")
        .or_else(|| path.strip_prefix("${TMPDIR}"))?;
    if rest.is_empty() {
        return Some(0);
    }
    let suffix = rest.strip_prefix('/')?;
    plain_depth(suffix)
}

/// The number of path components, or None when the word carries a form that
/// can move the walk somewhere it cannot see.
fn plain_depth(word: &str) -> Option<u32> {
    if word.is_empty() || word.starts_with('/') || word.starts_with('~') {
        return None;
    }
    if word.contains(['*', '?', '[', ']', '$', '`', '(', ')', '\\']) || marker_hit(word) {
        return None;
    }
    let mut depth = 0u32;
    for part in word.split('/') {
        if part.is_empty() || part == "." || part == ".." {
            return None;
        }
        depth += 1;
    }
    Some(depth)
}

/// Whether an output target is a place the walk proves inside the scratch
/// directory. The literal shared temporary roots are refused: the fence does
/// not bound writes there, so a gate that waived one would be overruled by the
/// kernel one step later.
pub(crate) fn scratch_target(path: &str, cwd: TempCwd) -> bool {
    if path == "/dev/null" || path == "-" {
        return true;
    }
    cwd.admits(path)
}

/// Whether a removal target is a place the walk proves inside the scratch
/// directory. Deletion is the irreversible effect, so it is narrower than a
/// write in two ways. A path spelled from the scratch root must be a single
/// component, never nested, because a nested path traverses a directory that
/// may be a symbolic link planted in the scratch space and would carry the
/// removal outside it. A relative path is only trusted when the walk stands at
/// the scratch root itself: a cd into a scratch subdirectory cannot prove that
/// subdirectory is not a link, so a relative removal from there is refused.
/// Writes do not carry this restriction because the fence bounds every write to
/// a writable root, but a removal that escapes the scratch space cannot be
/// undone, so it is held.
pub(crate) fn scratch_delete(path: &str, cwd: TempCwd) -> bool {
    match scratch_depth(path) {
        Some(depth) => depth == 1,
        None => matches!(cwd, TempCwd::Inside(0)) && plain_depth(path) == Some(1),
    }
}
