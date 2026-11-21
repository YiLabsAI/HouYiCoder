//! Whether a step beside a network read is a local file operation confined to
//! the session scratch space. The approval card asks this before it lets a
//! network ask count down; a download often carries companion steps that
//! prepare or unpack its output, and those are only safe to resolve unattended
//! when every path they touch stays in the scratch directory and none runs
//! another program or reaches a host.

/// One command segment paired with whether an unconditional and-and chain joins
/// it to the segment before it. Any separator that can run its right side
/// after a failed left side, or inside a subshell or group, drops the chain.
pub(super) struct ScratchStep<'a> {
    pub(super) chained: bool,
    pub(super) text: &'a str,
}

/// Split a command string on shell separators while recording whether each
/// segment is reached exclusively through an and-and operator.
pub(super) fn split_steps(text: &str) -> Vec<ScratchStep<'_>> {
    let is_sep = |c: char| matches!(c, '&' | ';' | '|' | '\n' | '(' | ')' | '`' | '{' | '}');
    let mut steps = Vec::new();
    let mut chained = false;
    let mut i = 0;
    let bytes = text.as_bytes();
    while i < bytes.len() {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let start = i;
        while i < bytes.len() && !is_sep(bytes[i] as char) {
            i += 1;
        }
        let seg = text[start..i].trim();
        if !seg.is_empty() {
            steps.push(ScratchStep { chained, text: seg });
        }
        let sep_start = i;
        while i < bytes.len() && (is_sep(bytes[i] as char) || bytes[i].is_ascii_whitespace()) {
            i += 1;
        }
        let gap: String = text[sep_start..i]
            .chars()
            .filter(|c| !c.is_ascii_whitespace())
            .collect();
        chained = !seg.is_empty() && gap == "&&";
    }
    steps
}

/// Advance whether the shell's working directory provably sits inside the
/// session scratch directory. Any non-chained separator, directory stack
/// command, bare change of directory, or target outside the scratch space
/// resets the state to false.
pub(super) fn advance_scratch(in_scratch: bool, chained: bool, words: &[&str]) -> bool {
    let current = in_scratch && chained;
    let Some(&first) = words.first() else {
        return current;
    };
    if first.eq_ignore_ascii_case("pushd") || first.eq_ignore_ascii_case("popd") {
        return false;
    }
    if !first.eq_ignore_ascii_case("cd") {
        return current;
    }
    let mut operands = words
        .iter()
        .skip(1)
        .copied()
        .filter(|w| !w.starts_with('-'));
    let Some(target) = operands.next() else {
        return false;
    };
    if operands.next().is_some() {
        return false;
    }
    if target.starts_with("$TMPDIR") || target.starts_with("${TMPDIR}") {
        return scratch_suffix_is_plain(target);
    }
    current && is_scratch_relative(target)
}

/// Whether the program acts only on files its own command line names, and so
/// cannot run another program or reach a destination no word names. An archive
/// program is judged separately because its flags can run a program or extract
/// outside the directory it is given.
pub(super) fn acts_on_named_files(word: &str) -> bool {
    matches!(word, "mkdir" | "rm" | "rmdir" | "touch" | "chmod")
}

/// Whether an archive flag takes the step outside an unpack in place: it runs
/// another program, extracts somewhere other than the directory it was given,
/// or honors an absolute or traversing member name. Attached-value forms such
/// as -d/ws or -C/ws start with the same flag letter, so the prefix match
/// catches them; a slash in any other word is refused separately.
fn escapes_in_place_unpack(flag: &str) -> bool {
    flag.starts_with("--to-command")
        || flag.starts_with("--checkpoint-action")
        || flag.starts_with("--use-compress-program")
        || flag.starts_with("--rsh-command")
        || flag.starts_with("--info-script")
        || flag.starts_with("--new-volume-script")
        || flag.starts_with("--absolute-names")
        || flag.starts_with("--transform")
        || flag.starts_with("--xform")
        || flag.starts_with("-I")
        || flag.starts_with("-C")
        || flag.starts_with("--directory")
        || flag.starts_with("-P")
        || flag.starts_with("-d")
}

/// The tar short flags an in-place unpack may bundle into one operand. This is
/// an allowlist, not a denylist, because a bundled cluster carries no leading
/// dash, so a denylist of dash-prefixed flags never sees it, and a letter this
/// walk does not know could take a directory value or honor an absolute member
/// name.
const SAFE_TAR_BUNDLE: &[char] = &['x', 't', 'f', 'z', 'j', 'J', 'v', 'p', 'm'];

/// Whether an extraction step unpacks a relative archive name in place.
///
/// Unpacking is the one step of the session that has to run beside a download,
/// and it is read narrowly: the program is tar or unzip, no flag names another
/// program or another directory, no operand bundles a tar short flag this walk
/// does not know is safe, and every path is a plain relative name, which can
/// only resolve inside the directory the walk put the shell in.
pub(super) fn unpacks_in_place(words: &[&str], in_scratch: bool) -> bool {
    if !in_scratch {
        return false;
    }
    let Some(&program) = words.first() else {
        return false;
    };
    if !matches!(program, "tar" | "unzip") {
        return false;
    }
    if words
        .iter()
        .skip(1)
        .any(|word| escapes_in_place_unpack(word))
    {
        return false;
    }
    !bundles_unknown_tar_flag(program, words) && names_only_scratch_paths(words, in_scratch)
}

/// Whether the archive step bundles a tar short flag this walk does not know is
/// safe. A dashless operand is read as bundled flags unless it holds a dot, in
/// which case it is an archive name and the path rules below judge it. A bundle
/// that carries a slash, such as a directory value attached to the flag, holds
/// no known flag letter and is refused, as is a bundle that mixes in an unknown
/// letter. Both refuse in the safe direction, so an extension-less archive or a
/// subdirectory operand simply waits for a human rather than counting down.
fn bundles_unknown_tar_flag(program: &str, words: &[&str]) -> bool {
    if program != "tar" {
        return false;
    }
    words.iter().skip(1).any(|word| {
        !word.starts_with('-')
            && !word.contains('.')
            && !word.chars().all(|c| SAFE_TAR_BUNDLE.contains(&c))
    })
}

/// Whether every word that names a path in the step stays inside the session
/// scratch space. A flag carries no path of its own, and a bare word such as a
/// glob is not one this walk can resolve. A flag that holds a slash, such as
/// -d/ws, names a path in an attached value, so it is refused rather than
/// waved through. A remote path carries a colon before any slash, which names
/// another host.
pub(super) fn names_only_scratch_paths(words: &[&str], in_scratch: bool) -> bool {
    words.iter().skip(1).all(|word| {
        if names_remote_path(word) {
            return false;
        }
        if word.starts_with('-') {
            return !word.contains('/');
        }
        if word.starts_with("$TMPDIR") || word.starts_with("${TMPDIR}") {
            return scratch_suffix_is_plain(word);
        }
        in_scratch && is_scratch_relative(word)
    })
}

/// Whether the part after the scratch root prefix is a plain relative path.
fn scratch_suffix_is_plain(word: &str) -> bool {
    let rest = word
        .strip_prefix("$TMPDIR")
        .or_else(|| word.strip_prefix("${TMPDIR}"))
        .unwrap_or_default();
    match rest.strip_prefix('/') {
        Some(suffix) => is_scratch_relative(suffix),
        None => rest.is_empty(),
    }
}

/// Whether a word names a path on another host, written host:path.
fn names_remote_path(word: &str) -> bool {
    match word.find(':') {
        Some(colon) => !word[..colon].contains('/'),
        None => false,
    }
}

/// Whether a word is a relative path that cannot leave the directory it is
/// resolved against.
fn is_scratch_relative(word: &str) -> bool {
    if word.is_empty()
        || word.starts_with('/')
        || word.starts_with('~')
        || word.contains(['*', '?', '[', ']', '$', '`', '(', ')', '\\'])
    {
        return false;
    }
    word.split('/')
        .all(|part| !part.is_empty() && part != "." && part != "..")
}
