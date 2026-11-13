//! The lexical and naming foundations of an atomic file transaction.
//!
//! The lexical guard runs before any system call: a path that names another
//! host, a device, or a fork of a file is rejected from its text alone, so the
//! transaction never opens it. The temp name gives each writer its own file so
//! two writers cannot truncate one another's half-written bytes. Both are pure
//! over their inputs so a host with no Windows kernel can still exercise every
//! branch.

#[cfg(test)]
mod tests;

/// The reason a path was rejected at the lexical gate, before any handle was
/// opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LexicalRejection {
    /// A UNC path names a share on another host, which is a network read with
    /// credential exposure rather than a local file.
    UncShare,
    /// A DOS reserved device name resolves to a device rather than a file, so
    /// a write to it reaches hardware or a null sink.
    ReservedDevice,
    /// An alternate data stream names a fork of a file, which the transaction
    /// does not replace.
    AlternateDataStream,
}

/// Reject a path that cannot be opened safely from its text alone.
///
/// The check is lexical on purpose: a UNC path that reaches another host over
/// SMB can stall on a dead server and leak credentials, and a reparse point
/// check that opens the handle first has already paid that cost. The device and
/// stream checks run on each path component so a name buried under directories
/// is still caught.
pub fn reject(path: &str) -> Option<LexicalRejection> {
    if is_unc(path) {
        return Some(LexicalRejection::UncShare);
    }
    for component in components(path) {
        if is_alternate_data_stream(&component) {
            return Some(LexicalRejection::AlternateDataStream);
        }
        if is_reserved_device(&component) {
            return Some(LexicalRejection::ReservedDevice);
        }
    }
    None
}

/// Translate a POSIX drive path a Git Bash shell hands the transaction into the
/// Windows path it names, or None when the path is not one of those forms.
///
/// The MSYS forms are a drive letter behind a single slash and behind the
/// cygdrive mount. The translation is lexical so the transaction does not pay a
/// probe to learn what the shell already named.
pub fn translate_posix_drive(path: &str) -> Option<String> {
    let bytes = path.as_bytes();
    if bytes.len() >= 2
        && bytes[0] == b'/'
        && bytes[1].is_ascii_alphabetic()
        && (bytes.len() == 2 || bytes[2] == b'/' || bytes[2] == b'\\')
    {
        return Some(drive_path(bytes[1], &path[2..]));
    }
    if let Some(rest) = path.strip_prefix("/cygdrive/") {
        let bytes = rest.as_bytes();
        if !bytes.is_empty() && bytes[0].is_ascii_alphabetic() {
            return Some(drive_path(bytes[0], &rest[1..]));
        }
    }
    None
}

/// A unique temp file name for one writer, beside the file it replaces.
///
/// The pid and the counter together name a writer that no other writer shares,
/// so the temp file one truncates while it writes is never the temp file
/// another is mid-flight in. The leading dot marks the file as transient so a
/// listing can hide it, and the original extension is kept so a reader that
/// filters by extension still sees it.
pub fn temp_name_for(name: &str, pid: u32, seq: u64) -> String {
    let (stem, extension) = split_extension(name);
    match extension {
        Some(ext) => format!(".{stem}.{pid}.{seq}.{ext}"),
        None => format!(".{stem}.{pid}.{seq}.tmp"),
    }
}

/// Whether a path is a UNC share path: two leading separators of either kind.
fn is_unc(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() >= 2
        && ((bytes[0] == b'\\' && bytes[1] == b'\\') || (bytes[0] == b'/' && bytes[1] == b'/'))
}

/// The path components, split on either separator and with empties dropped.
fn components(path: &str) -> Vec<String> {
    path.split(['\\', '/'])
        .filter(|part| !part.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Whether a component is an alternate data stream: a colon that does not belong
/// to a drive letter. A drive letter is exactly one alphabetic byte followed by
/// a colon at the start of the first component, and only that form is allowed.
fn is_alternate_data_stream(component: &str) -> bool {
    let bytes = component.as_bytes();
    match bytes.iter().position(|&b| b == b':') {
        Some(0) => true,
        Some(1) => !bytes[0].is_ascii_alphabetic(),
        Some(_) => true,
        None => false,
    }
}

/// Whether a component names a DOS reserved device. The stem before the first
/// dot is the name a device answers to, so the check reads it past an
/// extension the way the kernel does.
fn is_reserved_device(component: &str) -> bool {
    let stem = component.split('.').next().unwrap_or(component);
    let upper = stem.to_ascii_uppercase();
    matches!(
        upper.as_str(),
        "NUL"
            | "CON"
            | "PRN"
            | "AUX"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    )
}

/// Build the Windows path for a drive letter and the rest that follows it. The
/// rest keeps the separators it had, with forward slashes turned to backslashes
/// so the path is one the Windows kernel reads.
fn drive_path(letter: u8, rest: &str) -> String {
    let upper = letter.to_ascii_uppercase();
    let mut out = String::from(upper as char);
    out.push(':');
    let rest = rest.trim_start_matches(['/', '\\']).replace('/', "\\");
    if !rest.is_empty() {
        out.push('\\');
        out.push_str(&rest);
    }
    out
}

/// Split a file name into its stem and its extension, where the extension is the
/// part after the last dot and is None when the name has no dot.
fn split_extension(name: &str) -> (&str, Option<&str>) {
    match name.rfind('.') {
        Some(0) | None => (name, None),
        Some(index) => (&name[..index], Some(&name[index + 1..])),
    }
}
