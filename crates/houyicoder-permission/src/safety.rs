//! Bypass-immune safety checks. Certain paths are protected so strongly that
//! neither the current mode nor a stored consent can auto-allow a call that
//! touches them: the version-control directory, the control files under a
//! configuration directory, the instruction and startup files, and the memory
//! store. An instruction file or a memory store is only protected from being
//! rewritten, since a read of either decides nothing. The check is a coarse
//! text heuristic over the call's primary content string; a false negative is
//! caught by the protected-path resolved target and by the sandbox fence.

use serde_json::Value;

use crate::compound::{is_readonly_compound, split_compound};
use crate::heredoc::strip_quoted_heredoc_bodies;
use crate::rule::{Effect, input_content};
use crate::side_effect::is_shell_tool;

/// The version-control directory. Any path inside it decides the repository's
/// state, so every occurrence escalates.
const VERSION_CONTROL_DIR: &str = ".git/";

/// Directories that hold the agent's own configuration. A path under one of
/// them is control plane only when it lands on a child named below: a session
/// log or a captured command output lives under the same directory and is
/// data, which the path-bounds check judges like any other outside directory.
const CONTROL_DIRS: [&str; 3] = [".claude/", ".houyicoder/", ".agents/"];

/// Children of a configuration directory that decide what the agent may do
/// later: the permission and settings stores, the hook and command
/// definitions, the skill bodies and scripts, and the installed plugins that
/// supply them.
const CONTROL_CHILDREN: &[&str] = &[
    "settings.json",
    "settings.local.json",
    "permissions.json",
    "permissions.local.json",
    "skill-grants.json",
    ".credentials.json",
    "hooks",
    "skills",
    "commands",
    "agents",
    "plugins",
    "snapshots",
];

/// The control children the execution fence refuses to let any process write,
/// keyed by the configuration directory that holds them. A command that would
/// write one cannot succeed while the fence is active, so the gate refuses it
/// outright instead of asking for an approval the kernel then overrules.
///
/// The pairs are listed per directory because the fence's deny list is not a
/// cross product: it names the settings and permission stores of the directories
/// this product reads, the credential file wherever it sits, and the skill
/// directories of all three, while a hooks or snapshots directory under another
/// of them stays writable inside the workspace. Every control child absent from
/// this table keeps the user's answer, because a refusal there would claim a
/// kernel denial that does not happen.
const FENCE_DENIED_CHILDREN: &[(&str, &[&str])] = &[
    (
        ".houyicoder/",
        &[
            ".credentials.json",
            "permissions.json",
            "permissions.local.json",
            "settings.json",
            "settings.local.json",
            "skill-grants.json",
            "skills",
            "snapshots",
        ],
    ),
    (
        ".claude/",
        &[
            ".credentials.json",
            "settings.json",
            "settings.local.json",
            "skills",
        ],
    ),
    (".agents/", &["skills"]),
];

/// The fence-denied slice of the version-control directory. A planted hook runs
/// at the user's next checkout, and the profile denies writes to it; every
/// other path under the directory is how the repository client itself works.
const FENCE_DENIED_GIT_PATH: &str = ".git/hooks";

/// The memory store. Its text is recalled into a later prompt, so a silent
/// write plants an instruction the model follows then, which is what the
/// instruction files are protected from as well. Reading it is not protected:
/// the store is loaded before the first turn, so a read adds no exposure.
const MEMORY_DIR: &str = "memory";

/// Files that steer every later decision wherever they appear: the workspace
/// instruction files and the skill body. Matched on whole path components, so
/// a name that merely ends in one of these does not escalate. A verified
/// read-only command may read one: the model is expected to follow this text
/// already, so reading it adds no exposure, while rewriting it changes every
/// later decision.
const INSTRUCTION_FILES: &[&str] = &["agents.md", "claude.md", "skill.md"];

/// Shell startup files, which are also credential files: a write to one runs
/// code at every later shell start, and a read can expose the exports they
/// define. Both escalate, so the read waiver above does not reach them.
const STARTUP_FILES: &[&str] = &[
    ".bashrc",
    ".bash_profile",
    ".zshrc",
    ".zprofile",
    ".profile",
];

/// Whether a tool call touches a protected path. Returns Ask when the call's
/// primary content string names one of the bypass-immune paths; None
/// otherwise. It fires before the mode default and before a stored consent, so
/// neither the current mode nor an earlier answer silence it. A configuration
/// directory only protects the control paths under it, so session logs and
/// captured output do not escalate and a worktree checked out under it keeps
/// working.
pub fn safety_check(tool_name: &str, input: Option<&Value>) -> Option<Effect> {
    let content = input_content(tool_name, input);
    if content.is_empty() {
        return None;
    }
    let shell = is_shell_tool(tool_name);
    // Strip quoted heredoc bodies: a cat <<'EOF' body is literal text, so a
    // protected path (.git/, .bashrc, ...) appearing in it must not trip
    // this bypass-immune check. Unquoted heredoc bodies stay (bash expands
    // them).
    let content = if shell {
        strip_quoted_heredoc_bodies(&content)
    } else {
        content
    };
    let forms = spellings(&content);
    if control_hit(&forms) || startup_hit(&forms) {
        return Some(Effect::Ask);
    }
    // An instruction file or a memory store is protected from being
    // rewritten, which is how an approval would change every later decision.
    // Reading one decides nothing, so a command that only reads does not
    // escalate.
    if (instruction_hit(&forms) || memory_hit(&forms)) && !(shell && reads_only(&content)) {
        return Some(Effect::Ask);
    }
    None
}

/// Whether a shell command is one the read-only classifier statically
/// attests: no redirect, no substitution, no write option.
fn reads_only(content: &str) -> bool {
    let segments = split_compound(content);
    let refs: Vec<&str> = segments.iter().map(String::as_str).collect();
    !refs.is_empty() && is_readonly_compound(&refs)
}

/// Whether a string names a protected path, case-insensitively. Split out
/// from safety_check so the same judgement can be applied to a path resolved
/// to the file it actually names, and to the segments of a shell command the
/// caller has already classified for reading.
pub(crate) fn marker_hit(content: &str) -> bool {
    let forms = spellings(content);
    control_hit(&forms) || memory_hit(&forms) || instruction_hit(&forms) || startup_hit(&forms)
}

/// Whether a command names a path the execution fence refuses to let it write,
/// so that running it can only end in a kernel refusal.
pub(crate) fn fence_denied_path(content: &str) -> bool {
    let forms = spellings(content);
    forms.iter().any(|lower| {
        lower
            .match_indices(FENCE_DENIED_GIT_PATH)
            .any(|(at, _)| starts_component(lower, at))
            || FENCE_DENIED_CHILDREN.iter().any(|(dir, children)| {
                lower
                    .match_indices(dir)
                    .filter(|(at, _)| starts_component(lower, *at))
                    .any(|(at, _)| {
                        let rest = &lower[at + dir.len()..];
                        children.iter().any(|child| under_child(rest, child))
                    })
            })
    })
}

/// Whether the match starts a path component, so a name that merely ends in
/// the directory name is not that directory. The fence addresses the
/// directory by its own component, and a refusal may only stand where the
/// kernel refuses. One exception: an option can carry the path it names
/// without a separator, so a short option letter right before the marker
/// still starts the component.
fn starts_component(haystack: &str, at: usize) -> bool {
    let before = &haystack[..at];
    let Some(last) = before.chars().next_back() else {
        return true;
    };
    if !is_name_char(last) {
        return true;
    }
    option_prefix(before)
}

/// Whether the text ends with a short option -- a dash that itself starts a
/// word, followed by letters -- so the letters are an option and not part of
/// a file name.
fn option_prefix(before: &str) -> bool {
    let chars: Vec<char> = before.chars().collect();
    let mut at = chars.len();
    while at > 0 && (chars[at - 1].is_ascii_alphanumeric() || chars[at - 1] == '_') {
        at -= 1;
    }
    at > 0 && at < chars.len() && chars[at - 1] == '-' && (at == 1 || !is_name_char(chars[at - 2]))
}

/// Whether every protected path the content names is one the fence refuses to
/// let a process write. A command that only names such a path cannot change it
/// while the fence holds, so the read waiver below rests on the kernel rather
/// than on a classification of the command.
pub(crate) fn fence_covers_all_hits(content: &str) -> bool {
    let forms = spellings(content);
    if !forms.iter().any(|lower| fence_denied_path(lower)) {
        return false;
    }
    if instruction_hit(&forms) || memory_hit(&forms) || startup_hit(&forms) {
        return false;
    }
    !control_hit_outside_fence(&forms)
}

/// Whether one of the spellings names a protected path the fence does not
/// deny: a version-control path other than the hooks directory, or a control
/// child absent from the fence's own list.
fn control_hit_outside_fence(forms: &[String]) -> bool {
    forms.iter().any(|lower| {
        lower
            .match_indices(VERSION_CONTROL_DIR)
            .filter(|(at, _)| starts_component(lower, *at))
            .any(|(at, _)| !under_child(&lower[at + VERSION_CONTROL_DIR.len()..], "hooks"))
            || CONTROL_DIRS.iter().any(|dir| {
                lower
                    .match_indices(dir)
                    .filter(|(at, _)| starts_component(lower, *at))
                    .any(|(at, _)| {
                        let rest = &lower[at + dir.len()..];
                        CONTROL_CHILDREN.iter().any(|child| {
                            under_child(rest, child) && !fence_denied_child(dir, child)
                        })
                    })
            })
    })
}

/// Whether the fence denies writes to a control child of a configuration
/// directory.
fn fence_denied_child(dir: &str, child: &str) -> bool {
    FENCE_DENIED_CHILDREN
        .iter()
        .any(|(denied_dir, children)| *denied_dir == dir && children.contains(&child))
}

/// Whether one of the spellings names the version-control directory or a
/// control path under an agent configuration directory.
fn control_hit(forms: &[String]) -> bool {
    forms.iter().any(|lower| {
        lower
            .match_indices(VERSION_CONTROL_DIR)
            .any(|(at, _)| starts_component(lower, at))
            || CONTROL_DIRS.iter().any(|dir| {
                lower
                    .match_indices(dir)
                    .filter(|(at, _)| starts_component(lower, *at))
                    .any(|(at, _)| {
                        let rest = &lower[at + dir.len()..];
                        CONTROL_CHILDREN
                            .iter()
                            .any(|child| under_child(rest, child))
                    })
            })
    })
}

/// Whether one of the spellings names a memory store under an agent
/// configuration directory.
fn memory_hit(forms: &[String]) -> bool {
    forms.iter().any(|lower| {
        CONTROL_DIRS.iter().any(|dir| {
            lower
                .match_indices(dir)
                .filter(|(at, _)| starts_component(lower, *at))
                .any(|(at, _)| under_memory(&lower[at + dir.len()..]))
        })
    })
}

/// Whether one of the spellings names an instruction file.
fn instruction_hit(forms: &[String]) -> bool {
    forms
        .iter()
        .any(|lower| INSTRUCTION_FILES.iter().any(|name| names_file(lower, name)))
}

/// Whether one of the spellings names a shell startup file.
fn startup_hit(forms: &[String]) -> bool {
    forms
        .iter()
        .any(|lower| STARTUP_FILES.iter().any(|name| names_file(lower, name)))
}

/// Whether what follows a configuration directory reaches a memory store. The
/// store sits either directly under the directory or one project level down,
/// so only those two shapes match: a source directory that happens to be
/// named memory elsewhere under the configuration directory stays data.
fn under_memory(rest: &str) -> bool {
    if under_child(rest, MEMORY_DIR) {
        return true;
    }
    let Some(nested) = rest.strip_prefix("projects/") else {
        return false;
    };
    match nested.split_once('/') {
        Some((project, target)) => !project.is_empty() && under_child(target, MEMORY_DIR),
        None => false,
    }
}

/// The spellings of one content string, each lowercased: the path form, the
/// form with quotes and backslashes removed (a path split across an escaped
/// separator still names the file), and the collapsed form that drops dot
/// components and repeated separators (a path written with a redundant step
/// still names the directory).
fn spellings(content: &str) -> Vec<String> {
    let plain = content.replace('\\', "/").to_ascii_lowercase();
    let unquoted = content.contains(['\'', '"', '\\']).then(|| {
        content
            .chars()
            .filter(|&c| c != '\'' && c != '"' && c != '\\')
            .collect::<String>()
            .to_ascii_lowercase()
    });
    let collapsed = collapse_path_spelling(unquoted.as_deref().unwrap_or(&plain));
    let mut forms = vec![plain];
    if let Some(unquoted) = unquoted
        && forms.last() != Some(&unquoted)
    {
        forms.push(unquoted);
    }
    if forms.last() != Some(&collapsed) {
        forms.push(collapsed);
    }
    forms
}

/// One spelling per directory: drop empty parts and current-directory steps so
/// a path written with a redundant separator reads like the plain form, and
/// take a parent step back one component so a detour through an unrelated
/// child cannot hide the file the path finally names. The leading separator
/// goes with them; every protected name is matched as a component sequence,
/// so the root marker is not needed.
fn collapse_path_spelling(text: &str) -> String {
    let mut kept: Vec<&str> = Vec::new();
    for part in text.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        if part == ".." {
            kept.pop();
            continue;
        }
        kept.push(part);
    }
    kept.join("/")
}

/// Whether what follows a configuration directory names the given control
/// child, either the child itself or a path below it. The name ends where a
/// character that cannot continue a file name begins, so a separator, a shell
/// operator, or a closing quote after it still names the child while a longer
/// name such as a backup copy does not.
fn under_child(rest: &str, child: &str) -> bool {
    match rest.strip_prefix(child) {
        None => false,
        Some(after) => match after.chars().next() {
            None => true,
            Some(c) => c == '/' || !is_name_char(c),
        },
    }
}

/// Whether the string contains the file name as a whole path component, so a
/// similarly named file such as install.bashrc does not escalate.
fn names_file(haystack: &str, name: &str) -> bool {
    let mut from = 0;
    while let Some(at) = haystack[from..].find(name) {
        let start = from + at;
        let end = start + name.len();
        let before = haystack[..start].chars().next_back();
        let after = haystack[end..].chars().next();
        let bounded_before = before.is_none_or(|c| !is_name_char(c));
        let bounded_after = after.is_none_or(|c| !is_name_char(c));
        if bounded_before && bounded_after {
            return true;
        }
        from = start + 1;
    }
    false
}

/// Whether the character can continue a file name, so the boundary test does
/// not treat a longer name as the protected one.
fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')
}

#[cfg(test)]
mod tests;
