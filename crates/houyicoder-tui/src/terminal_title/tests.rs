use super::*;
use houyicoder_protocol::frontend::status::SessionDescriptorSummary;

#[test]
fn test_missing_descriptor() {
    let snap = StatusSnapshot::default();
    assert_eq!(title_for(&snap).as_deref(), Some(DEFAULT_TITLE));
}

#[test]
fn test_unnamed_descriptor() {
    let snap = StatusSnapshot {
        descriptor: Some(SessionDescriptorSummary {
            name: None,
            ..Default::default()
        }),
        ..Default::default()
    };
    assert_eq!(title_for(&snap).as_deref(), Some(DEFAULT_TITLE));
}

#[test]
fn test_named_snapshot_titles_name() {
    let snap = StatusSnapshot {
        descriptor: Some(SessionDescriptorSummary {
            name: Some("fix-login".into()),
            ..Default::default()
        }),
        ..Default::default()
    };
    assert_eq!(title_for(&snap).as_deref(), Some("fix-login"));
}

/// An empty/whitespace name falls back to the default title (not blank,
/// not None -- the descriptor exists, the name just is not set).
#[test]
fn test_empty_name_uses_default() {
    let snap = StatusSnapshot {
        descriptor: Some(SessionDescriptorSummary {
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
    let snap = StatusSnapshot {
        descriptor: Some(SessionDescriptorSummary {
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
    let snap = StatusSnapshot {
        descriptor: Some(SessionDescriptorSummary {
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
    let named = StatusSnapshot {
        descriptor: Some(SessionDescriptorSummary {
            name: Some("alpha".into()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut last: Option<String> = None;
    sync(&named, &mut last);
    assert_eq!(last.as_deref(), Some("alpha"));
    let unnamed = StatusSnapshot {
        descriptor: Some(SessionDescriptorSummary::default()),
        ..Default::default()
    };
    sync(&unnamed, &mut last);
    assert_eq!(last.as_deref(), Some(DEFAULT_TITLE), "reset to default");
}
