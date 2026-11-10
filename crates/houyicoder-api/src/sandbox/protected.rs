//! The paths that steer the agent's own configuration and later decisions:
//! the version-control directory, control files under a configuration
//! directory, instruction and startup files, the memory store, and the
//! credentials that decide which account a tool acts as.
//!
//! The gate that decides whether to ask and the card that decides whether an
//! answer may resolve unattended share this judgement. A string is read as
//! the shell would spell it, each name matched as a whole path component.

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
/// write one cannot succeed while the fence confines writes, so the gate
/// refuses it outright instead of asking for an approval the kernel then
/// overrules.
///
/// The entries are listed per directory because the fence's deny list is not a
/// cross product: it names the settings and permission stores of the
/// directories this product reads, the credential file wherever it sits, and
/// the skill directories of all three, while a hooks or snapshots directory
/// under another of them stays writable inside the workspace. Every control
/// child absent from this table keeps the user's answer, because a refusal
/// there would claim a kernel denial that does not happen.
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

/// The protected path families a text names.
///
/// The families are reported apart because they are protected for different
/// reasons. A control path or a startup file escalates on any mention, while an
/// instruction file or a memory store escalates only when the text can change
/// it, so a verified read stays out of the ask.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProtectedPathHits {
    /// The version-control directory or a control child under a configuration
    /// directory.
    pub control: bool,
    /// A shell startup file.
    pub startup: bool,
    /// An instruction file.
    pub instruction: bool,
    /// A memory store.
    pub memory: bool,
}

impl ProtectedPathHits {
    /// Whether the text names a protected path of any family.
    pub fn any(self) -> bool {
        self.control || self.startup || self.instruction || self.memory
    }
}

/// The protected path families a string names, case-insensitively. A caller
/// judging a command's effect reads this over the command text; a caller
/// judging a resolved target reads it over the path.
pub fn protected_path_hits(content: &str) -> ProtectedPathHits {
    let forms = spellings(content);
    ProtectedPathHits {
        control: control_hit(&forms),
        startup: startup_hit(&forms),
        instruction: instruction_hit(&forms),
        memory: memory_hit(&forms),
    }
}

/// Whether a command names a path the execution fence refuses to let it write,
/// so that running it can only end in a kernel refusal.
pub fn fence_denied_path(content: &str) -> bool {
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
/// while the fence holds, so the read waiver in the gate rests on the kernel
/// rather than on a classification of the command.
pub fn fence_covers_all_hits(content: &str) -> bool {
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
