//! Archive, restore, scoped replacement, and batch scope reconciliation tests.

use std::env::temp_dir;
use std::fs::{File, FileTimes, create_dir_all, read_to_string, remove_dir_all, write};
use std::path::PathBuf;
use std::process::id;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use houyicoder_api::memory::{MemoryProvider, MemoryWriteOutcome};
use houyicoder_context::{MemoryEntry, MemoryError, MemoryOrigin, MemoryScope, MemoryType};

use super::super::MarkdownMemoryProvider;

struct MultiFixture {
    workspace: PathBuf,
    user: PathBuf,
    project: PathBuf,
    auto: PathBuf,
    provider: MarkdownMemoryProvider,
}

impl MultiFixture {
    fn new(tag: &str) -> Self {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let workspace = temp_dir().join(format!("mem-archive-{tag}-{seq}-{}", id()));
        let user = workspace.join("user");
        let project = workspace.join("project");
        let auto = workspace.join("auto");
        for d in [&user, &project, &auto] {
            create_dir_all(d).expect("mkdir");
        }
        let provider =
            MarkdownMemoryProvider::new_multi(vec![user.clone(), project.clone(), auto.clone()]);
        Self {
            workspace,
            user,
            project,
            auto,
            provider,
        }
    }
}

impl Drop for MultiFixture {
    fn drop(&mut self) {
        drop(remove_dir_all(&self.workspace));
    }
}

#[test]
fn test_archive_restore_keeps_stats() {
    let fx = MultiFixture::new("restore");
    fx.provider
        .add(MemoryEntry::new(
            "lesson-a",
            "Lesson body.",
            MemoryType::Feedback,
        ))
        .expect("add");
    fx.provider.record_recall_hits(&["lesson-a".to_string()]);
    fx.provider.record_gate_violation("lesson-a");

    fx.provider
        .archive_memory("lesson-a", MemoryScope::Auto, MemoryOrigin::PrimaryAgent)
        .expect("archive");
    assert!(!fx.auto.join("lesson-a.md").exists());
    assert!(fx.auto.join("archive").join("lesson-a.md").is_file());
    assert!(
        fx.provider
            .list_memories()
            .iter()
            .all(|s| s.key != "lesson-a"),
        "archived entry leaves active listing"
    );

    let archived = fx.provider.list_archived();
    assert_eq!(archived.len(), 1);
    assert_eq!(archived[0].key, "lesson-a");
    let shown = fx
        .provider
        .show_archived("lesson-a", MemoryScope::Auto)
        .expect("show archived");
    assert_eq!(shown.content, "Lesson body.");

    fx.provider
        .restore_memory("lesson-a", MemoryScope::Auto)
        .expect("restore");
    assert!(fx.auto.join("lesson-a.md").is_file());
    assert!(fx.provider.list_archived().is_empty());
    let stats = fx.provider.read_recall_stats();
    let row = stats.iter().find(|s| s.key == "lesson-a").expect("stats");
    assert_eq!(row.recall_hits, 1);
    assert_eq!(row.gate_violations, 1);
    assert_eq!(row.occurrences, 1);

    fx.provider
        .archive_memory("lesson-a", MemoryScope::Auto, MemoryOrigin::PrimaryAgent)
        .expect("archive again");
    fx.provider
        .delete_archived("lesson-a", MemoryScope::Auto)
        .expect("delete archived");
    assert!(fx.provider.list_archived().is_empty());
    assert!(
        fx.provider
            .read_recall_stats()
            .iter()
            .all(|s| s.key != "lesson-a"),
        "deleting archived entry prunes stats when no copy remains"
    );
    assert!(
        fx.provider
            .delete_archived("lesson-a", MemoryScope::Auto)
            .is_err()
    );
}

#[test]
fn test_replace_preserves_scope() {
    let fx = MultiFixture::new("replace-guard");
    fx.provider
        .add_in_scope(
            MemoryEntry::new("user-pref", "User secret rule.", MemoryType::User)
                .with_origin(MemoryOrigin::User),
            MemoryScope::User,
        )
        .expect("seed user");
    fx.provider
        .add(
            MemoryEntry::new("old-auto", "Old auto rule.", MemoryType::Feedback)
                .with_origin(MemoryOrigin::Extractor),
        )
        .expect("seed auto");
    fx.provider.record_recall_hits(&["old-auto".to_string()]);
    fx.provider.record_gate_violation("old-auto");

    // 1. Background Dream tries to replace both User entry and its own target key:
    //    User entry must stay untouched, and target key must not swallow itself.
    let dream_entry = MemoryEntry::new("merged-rule", "Merged body.", MemoryType::Feedback)
        .with_origin(MemoryOrigin::Dream);
    let out = fx
        .provider
        .replace_in_scope(
            dream_entry,
            MemoryScope::Project,
            &[
                "merged-rule".to_string(),
                "user-pref".to_string(),
                "old-auto".to_string(),
            ],
        )
        .expect("replace");
    assert_eq!(out, MemoryWriteOutcome::Created);
    assert!(
        fx.project.join("merged-rule.md").is_file(),
        "scoped replace honors target project scope and never swallows self"
    );
    assert!(
        fx.user.join("user-pref.md").is_file(),
        "background replace must never archive user-scope entries"
    );
    assert!(
        fx.auto.join("archive").join("old-auto.md").is_file(),
        "auto predecessor moves to archive"
    );

    let stats = fx.provider.read_recall_stats();
    let merged = stats
        .iter()
        .find(|s| s.key == "merged-rule")
        .expect("merged stats");
    assert_eq!(merged.recall_hits, 1);
    assert_eq!(merged.gate_violations, 1);
    assert_eq!(merged.occurrences, 2);
}

#[test]
fn test_reconcile_batch_moves_once() {
    let mut fx = MultiFixture::new("reconcile");
    fx.provider
        .add(MemoryEntry::new(
            "hot-feedback",
            "Rule body.",
            MemoryType::Feedback,
        ))
        .expect("add feedback");
    fx.provider.record_gate_violation("hot-feedback");
    fx.provider.record_gate_violation("hot-feedback");

    fx.provider
        .add(
            MemoryEntry::new("stale-proj", "Finished task.", MemoryType::Project)
                .with_origin(MemoryOrigin::Extractor),
        )
        .expect("add stale project");
    let stale_path = fx.auto.join("stale-proj.md");
    let old_time = SystemTime::now() - Duration::from_secs(40 * 86_400);
    File::options()
        .write(true)
        .open(&stale_path)
        .expect("open")
        .set_times(FileTimes::new().set_modified(old_time))
        .expect("stamp old mtime");

    assert!(!fx.provider.auto_expire());
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // By default auto_expire is false, so stale-proj is spared while hot-feedback promotes.
    let default_changes = fx.provider.reconcile_scopes(now);
    assert_eq!(default_changes.len(), 1);
    assert!(fx.project.join("hot-feedback.md").is_file());
    assert!(fx.auto.join("stale-proj.md").is_file());

    // When auto_expire is enabled, stale-proj expires into archive/.
    fx.provider.set_auto_expire(true);
    assert!(fx.provider.auto_expire());
    let changes = fx.provider.reconcile_scopes(now);
    assert_eq!(changes.len(), 1);
    assert!(fx.auto.join("archive").join("stale-proj.md").is_file());
}

#[test]
fn test_demote_and_user_guard() {
    let fx = MultiFixture::new("demote-guard");
    fx.provider
        .add_in_scope(
            MemoryEntry::new("cold-promoted", "Decayed rule.", MemoryType::Feedback)
                .with_origin(MemoryOrigin::Dream),
            MemoryScope::Project,
        )
        .expect("seed project");
    let cold_path = fx.project.join("cold-promoted.md");
    let old_time = SystemTime::now() - Duration::from_secs(65 * 86_400);
    File::options()
        .write(true)
        .open(&cold_path)
        .expect("open")
        .set_times(FileTimes::new().set_modified(old_time))
        .expect("stamp");

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let changes = fx.provider.reconcile_scopes(now);
    assert_eq!(changes.len(), 1);
    assert!(fx.auto.join("cold-promoted.md").is_file());
    assert!(!fx.project.join("cold-promoted.md").exists());

    // Background writer cannot overwrite a User-origin file.
    fx.provider
        .add(
            MemoryEntry::new("hand-note", "User authored.", MemoryType::User)
                .with_origin(MemoryOrigin::User),
        )
        .expect("seed user origin");
    let bg_attempt = fx.provider.add_if_changed(
        MemoryEntry::new("hand-note", "Extractor overwrite.", MemoryType::User)
            .with_origin(MemoryOrigin::Extractor),
    );
    assert!(
        bg_attempt.is_err(),
        "background overwrite of user origin must fail"
    );
    assert_eq!(
        fx.provider.show_memory("hand-note").unwrap().content,
        "User authored."
    );

    // Background writer can update a PrimaryAgent file, but preserves its origin.
    fx.provider
        .add(
            MemoryEntry::new("agent-note", "Agent v1.", MemoryType::Feedback)
                .with_origin(MemoryOrigin::PrimaryAgent),
        )
        .expect("seed primary agent origin");
    let bg_update = fx.provider.add_if_changed(
        MemoryEntry::new(
            "agent-note",
            "Extractor updated text.",
            MemoryType::Feedback,
        )
        .with_origin(MemoryOrigin::Extractor),
    );
    assert_eq!(
        bg_update.expect("update succeeds"),
        MemoryWriteOutcome::Updated
    );
    let updated = fx.provider.show_memory("agent-note").expect("found");
    assert_eq!(updated.content, "Extractor updated text.");
    assert_eq!(
        updated.origin,
        MemoryOrigin::PrimaryAgent,
        "background update preserves the stored PrimaryAgent origin"
    );
}

#[test]
fn test_project_type_promotes() {
    let fx = MultiFixture::new("proj-promo");
    fx.provider
        .add(MemoryEntry::new(
            "arch-decision",
            "Project architecture fact.",
            MemoryType::Project,
        ))
        .expect("add project memory");
    fx.provider.record_recall_hits(&["arch-decision".into()]);
    // Simulate 8 recall hits
    for _ in 0..7 {
        fx.provider.record_recall_hits(&["arch-decision".into()]);
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let changes = fx.provider.reconcile_scopes(now);
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].key, "arch-decision");
    assert_eq!(changes[0].scope, MemoryScope::Project);
    assert!(fx.project.join("arch-decision.md").is_file());

    // Reference type in auto scope is not promoted even with hits
    fx.provider
        .add(MemoryEntry::new(
            "ref-book",
            "Reference material.",
            MemoryType::Reference,
        ))
        .expect("add reference memory");
    for _ in 0..8 {
        fx.provider.record_recall_hits(&["ref-book".into()]);
    }
    let ref_changes = fx.provider.reconcile_scopes(now);
    assert!(ref_changes.is_empty());
}

#[test]
fn test_provider_expire_builder() {
    let p = MarkdownMemoryProvider::new(temp_dir().join("expire-bld")).with_auto_expire(true);
    assert!(p.auto_expire());
}

/// A background requester cannot archive a topic authored by the user or the
/// primary agent, while a foreground requester on the same path can.
#[test]
fn test_archive_origin_guard() {
    let fx = MultiFixture::new("archive-guard");
    fx.provider
        .add(
            MemoryEntry::new("user-note", "User body.", MemoryType::User)
                .with_origin(MemoryOrigin::User),
        )
        .expect("seed user");
    fx.provider
        .add(
            MemoryEntry::new("agent-note", "Agent body.", MemoryType::Feedback)
                .with_origin(MemoryOrigin::PrimaryAgent),
        )
        .expect("seed agent");

    assert!(matches!(
        fx.provider
            .archive_memory("user-note", MemoryScope::Auto, MemoryOrigin::Dream),
        Err(MemoryError::InvalidPath(_))
    ));
    assert!(fx.auto.join("user-note.md").is_file());

    assert!(matches!(
        fx.provider
            .archive_memory("agent-note", MemoryScope::Auto, MemoryOrigin::Dream),
        Err(MemoryError::InvalidPath(_))
    ));
    assert!(fx.auto.join("agent-note.md").is_file());
    assert!(fx.provider.list_archived().is_empty());

    fx.provider
        .archive_memory("agent-note", MemoryScope::Auto, MemoryOrigin::PrimaryAgent)
        .expect("foreground requester archives its own topic");
    assert!(!fx.auto.join("agent-note.md").exists());
    assert!(fx.auto.join("archive").join("agent-note.md").is_file());
}

/// A hand-written file without frontmatter parses as corrupt and defaults to
/// User provenance, so background passes cannot overwrite, supersede, archive,
/// or delete it, while the user can still forget it explicitly.
#[test]
fn test_corrupt_note_fail_closed() {
    let fx = MultiFixture::new("corrupt-guard");
    let raw_path = fx.auto.join("plain-note.md");
    write(&raw_path, "hand-written plain markdown\n").expect("seed raw note");

    let overwrite = fx.provider.add_if_changed(
        MemoryEntry::new("plain-note", "machine replacement", MemoryType::Project)
            .with_origin(MemoryOrigin::Extractor),
    );
    assert!(matches!(overwrite, Err(MemoryError::InvalidPath(_))));
    assert_eq!(
        read_to_string(&raw_path).expect("read raw note"),
        "hand-written plain markdown\n"
    );

    fx.provider
        .replace_in_scope(
            MemoryEntry::new("merged-auto", "Consolidated.", MemoryType::Feedback)
                .with_origin(MemoryOrigin::Dream),
            MemoryScope::Auto,
            &["plain-note".to_string()],
        )
        .expect("merged write lands without superseding the raw note");
    assert!(raw_path.is_file());
    assert!(!fx.auto.join("archive").join("plain-note.md").exists());

    assert!(matches!(
        fx.provider
            .archive_memory("plain-note", MemoryScope::Auto, MemoryOrigin::Dream),
        Err(MemoryError::InvalidPath(_))
    ));
    assert!(matches!(
        fx.provider.delete_memory("plain-note", MemoryOrigin::Dream),
        Err(MemoryError::InvalidPath(_))
    ));
    assert!(raw_path.is_file());

    fx.provider
        .delete_memory_in_scope("plain-note", MemoryScope::Auto, MemoryOrigin::User)
        .expect("the user can forget a hand-written note");
    assert!(!raw_path.exists());
}
