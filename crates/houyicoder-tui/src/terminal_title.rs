//! Terminal tab title via OSC 0/2 (ESC ] 0 ; <title> BEL). The title tracks
//! the session name: the default app name at launch (no descriptor yet) + when
//! swapped to an unnamed session, the session name once set, and the new name
//! on rename. It is written only on CHANGE -- the idle poll fires a
//! StatusResult every second, so an unconditional write would spam stdout
//! (>=1 OSC/s) + race ratatui's writer. The tab title is kept in sync with
//! the session name so a multi-tab host distinguishes sessions at a glance
//! (no terminal-title sync exists otherwise).

use houyicoder_protocol::frontend::status::StatusSnapshot;
use std::borrow::Cow;
use std::io::Write;
use std::io::stdout;

/// The tab title when no session name is set (the bare app name; a clearer
/// signal than a blank title). Also the restore-on-teardown title.
pub(crate) const DEFAULT_TITLE: &str = "houyicoder";

/// The longest title sent. Names beyond this are truncated so a pathological
/// descriptor value cannot dump megabytes of OSC into the stream.
const MAX_TITLE_LEN: usize = 128;

/// Compute the title for a status snapshot. A missing or unnamed descriptor
/// resets the terminal to the default instead of retaining a previous session
/// name. The caller suppresses duplicate writes.
pub(crate) fn title_for(snap: &StatusSnapshot) -> Option<String> {
    let name = snap
        .descriptor
        .as_ref()
        .and_then(|descriptor| descriptor.name.as_deref());
    let title = match name {
        Some(n) if !n.trim().is_empty() => sanitize(n).into_owned(),
        _ => DEFAULT_TITLE.to_string(),
    };
    Some(title)
}

/// Sync the title from a snapshot, writing only on change. The idle status
/// poll fires a StatusResult every second; the caller passes its cached last
/// title so the OSC is rewritten only when the title actually changes (not
/// >=1/s), avoiding stdout spam + a race with ratatui's writer.
pub(crate) fn sync(snap: &StatusSnapshot, last: &mut Option<String>) {
    let new = title_for(snap);
    if new != *last {
        if let Some(t) = &new {
            set_title(t);
        }
        *last = new;
    }
}

/// Restore the default tab title on teardown so the session name does not
/// outlive the app in the terminal chrome. Called beside the progress clear
/// by the terminal guard's host-chrome restore.
pub(crate) fn restore() {
    set_title(DEFAULT_TITLE);
}

/// Write the OSC 0/2 title sequence to stdout (ESC ] 0 ; <title> BEL). Best
/// effort: a write failure (piped/non-tty stdout) is swallowed. Written
/// outside the draw call (in the event-dispatch path) so it does not race the
/// render loop.
pub(crate) fn set_title(title: &str) {
    let clean = sanitize(title);
    // ESC ] 0 ; <title> BEL. BEL (0x07) over ST for broader parser support
    // (OSC 52 makes the same call in selection.rs).
    let seq = format!("\x1b]0;{clean}\x07");
    let mut out = stdout();
    let _r = out.write_all(seq.as_bytes());
    let _r = out.flush();
}

/// Strip C0 + C1 control characters (ESC, BEL, LF, CR, TAB, ...) + truncate.
/// The title is not just keyboard-sourced: it comes from the disk descriptor,
/// which may hold a value written by an older version, hand-edited, or
/// produced by another tool. A name containing ESC or BEL would truncate the
/// OSC early + feed the rest to the terminal as commands (escape injection).
/// Rust's char::is_control covers C0 (0x00-0x1f) + C1 (0x80-0x9f) incl. the
/// OSC delimiters themselves.
fn sanitize(title: &str) -> Cow<'_, str> {
    let trimmed = title.trim();
    if !trimmed.chars().any(char::is_control) && trimmed.chars().count() <= MAX_TITLE_LEN {
        return Cow::Borrowed(trimmed);
    }
    let cleaned: String = trimmed
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_TITLE_LEN)
        .collect();
    Cow::Owned(cleaned)
}

#[cfg(test)]
mod tests;
