//! Terminal tab title via OSC 0/2 (ESC ] 0 ; <title> BEL). The title tracks
//! the session name: the default app name at launch (no sidecar yet) + when
//! swapped to an unnamed session, the session name once set, and the new name
//! on rename. It is written only on CHANGE -- the idle poll fires a
//! StatusResult every second, so an unconditional write would spam stdout
//! (>=1 OSC/s) + race ratatui's writer. The tab title is kept in sync with
//! the session name so a multi-tab host distinguishes sessions at a glance
//! (no terminal-title sync exists otherwise).

use houyicoder_protocol::frontend::status::StatusSnapshot;
use std::io::Write;

/// The tab title when no session name is set (the bare app name; a clearer
/// signal than a blank title). Also the restore-on-teardown title.
pub(crate) const DEFAULT_TITLE: &str = "houyicoder";

/// The longest title sent. Names beyond this are truncated so a pathological
/// sidecar value cannot dump megabytes of OSC into the stream.
const MAX_TITLE_LEN: usize = 128;

/// Compute the title a snapshot would set, without writing. A snapshot with
/// no sidecar meta or no name yields the default app name -- the launch scene
/// (fresh session, no sidecar yet) + a swap to an unnamed session both reset
/// the tab to the default rather than leaving a stale prior session's name on
/// the chrome. The caller caches the last title + only writes on change, so
/// the per-second idle poll does not flood stdout.
pub(crate) fn title_for(snap: &StatusSnapshot) -> Option<String> {
    let name = snap.meta.as_ref().and_then(|m| m.name.as_deref());
    let title = match name {
        Some(n) if !n.trim().is_empty() => sanitize(n).into_owned(),
        // No meta, or a meta with no/blank name: the default title. Returning
        // Some (not None) is what makes the launch + swap-to-unnamed scenes
        // reset the chrome -- None would leave the prior OSC title in place.
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
/// outlive the app in the terminal chrome (pairs with set_terminal_progress(false)
/// in app.rs teardown).
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
    let mut out = std::io::stdout();
    let _r = out.write_all(seq.as_bytes());
    let _r = out.flush();
}

/// Strip C0 + C1 control characters (ESC, BEL, LF, CR, TAB, ...) + truncate.
/// The title is not just keyboard-sourced: it comes from the disk sidecar,
/// which may hold a value written by an older version, hand-edited, or
/// produced by another tool. A name containing ESC or BEL would truncate the
/// OSC early + feed the rest to the terminal as commands (escape injection).
/// Rust's char::is_control covers C0 (0x00-0x1f) + C1 (0x80-0x9f) incl. the
/// OSC delimiters themselves.
fn sanitize(title: &str) -> std::borrow::Cow<'_, str> {
    let trimmed = title.trim();
    if !trimmed.chars().any(char::is_control) && trimmed.chars().count() <= MAX_TITLE_LEN {
        return std::borrow::Cow::Borrowed(trimmed);
    }
    let cleaned: String = trimmed
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_TITLE_LEN)
        .collect();
    std::borrow::Cow::Owned(cleaned)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// No sidecar meta => the default title (not None). Returning Some is what
    /// makes the launch scene + a swap to an unnamed session reset the chrome;
    /// None would leave the prior session's title stuck on the tab.
    #[test]
    fn test_no_meta_uses_default() {
        let snap = StatusSnapshot::default();
        assert_eq!(title_for(&snap).as_deref(), Some(DEFAULT_TITLE));
    }

    /// A meta with no name (sidecar exists but the session is unnamed) also
    /// yields the default title -- the swap-to-unnamed case where a stale
    /// prior name must not outlive the session.
    #[test]
    fn test_meta_no_name_default() {
        use houyicoder_protocol::frontend::status::SessionMetaSummary;
        let snap = StatusSnapshot {
            meta: Some(SessionMetaSummary {
                name: None,
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(title_for(&snap).as_deref(), Some(DEFAULT_TITLE));
    }

    /// A snapshot with a meta name yields that name as the title.
    #[test]
    fn test_named_snapshot_titles_name() {
        use houyicoder_protocol::frontend::status::SessionMetaSummary;
        let snap = StatusSnapshot {
            meta: Some(SessionMetaSummary {
                name: Some("fix-login".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(title_for(&snap).as_deref(), Some("fix-login"));
    }

    /// An empty/whitespace name falls back to the default title (not blank,
    /// not None -- the sidecar exists, the name just is not set).
    #[test]
    fn test_empty_name_uses_default() {
        use houyicoder_protocol::frontend::status::SessionMetaSummary;
        let snap = StatusSnapshot {
            meta: Some(SessionMetaSummary {
                name: Some("   ".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(title_for(&snap).as_deref(), Some(DEFAULT_TITLE));
    }

    /// A name carrying ESC / BEL / newline is stripped before it reaches the
    /// OSC -- a disk-sourced name with a control char would otherwise
    /// truncate the sequence + inject the tail as terminal commands.
    #[test]
    fn test_control_chars_stripped() {
        use houyicoder_protocol::frontend::status::SessionMetaSummary;
        let snap = StatusSnapshot {
            meta: Some(SessionMetaSummary {
                name: Some("fix\x1b]0;evil\x07login\n".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let title = title_for(&snap).expect("title computed");
        assert!(!title.contains('\x1b'), "no ESC in title: {title:?}");
        assert!(!title.contains('\x07'), "no BEL: {title:?}");
        assert!(!title.contains('\n'), "no newline: {title:?}");
        assert!(title.contains("fix"), "safe chars kept: {title:?}");
        assert!(title.contains("login"), "safe chars kept: {title:?}");
    }

    /// A pathologically long name is truncated so the OSC stays bounded.
    #[test]
    fn test_long_name_truncated() {
        use houyicoder_protocol::frontend::status::SessionMetaSummary;
        let snap = StatusSnapshot {
            meta: Some(SessionMetaSummary {
                name: Some("x".repeat(500)),
                ..Default::default()
            }),
            ..Default::default()
        };
        let title = title_for(&snap).expect("title computed");
        assert!(
            title.chars().count() <= MAX_TITLE_LEN,
            "truncated: {}",
            title.len()
        );
    }

    /// Sync writes the default when the new snapshot has no name but the last
    /// title was a real name: the swap-to-unnamed-session regression. Without
    /// the Some(DEFAULT) for no-name, sync would skip the write + the chrome
    /// would keep the prior session's name indefinitely.
    #[test]
    fn test_sync_resets_when_unnamed() {
        use houyicoder_protocol::frontend::status::SessionMetaSummary;
        let named = StatusSnapshot {
            meta: Some(SessionMetaSummary {
                name: Some("alpha".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut last: Option<String> = None;
        sync(&named, &mut last);
        assert_eq!(last.as_deref(), Some("alpha"));
        // Swap to an unnamed session (meta with no name) -- sync must reset.
        let unnamed = StatusSnapshot {
            meta: Some(SessionMetaSummary::default()),
            ..Default::default()
        };
        sync(&unnamed, &mut last);
        assert_eq!(last.as_deref(), Some(DEFAULT_TITLE), "reset to default");
    }
}
