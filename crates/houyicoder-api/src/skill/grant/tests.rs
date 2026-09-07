use super::*;

/// A unique temp directory + skill-grants.json path for one test.
/// Uses pid + atomic counter so parallel tests in the same process
/// do not collide, and never touches the real user home directory.
fn fresh_grant_path() -> PathBuf {
    use std::sync::atomic::AtomicU32;
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = env::temp_dir().join(format!("houyi-skill-grant-{}-{n}", process::id()));
    let _ = fs::remove_dir_all(&dir).is_ok();
    fs::create_dir_all(&dir).expect("mkdir test dir");
    dir.join("skill-grants.json")
}

/// A store backed by a fresh temp path. For tests that need set_grant
/// to persist to disk without touching the real user home.
fn fresh_store() -> SkillGrantStore {
    let path = fresh_grant_path();
    SkillGrantStore {
        grants: Mutex::new(HashMap::new()),
        path,
    }
}

/// A store backed by a fresh temp path pre-loaded from a specific
/// file content (for corrupt-file tests etc.).
fn user_source() -> crate::skill::SkillSource {
    crate::skill::SkillSource::new(
        crate::skill::SkillFamily::Houyi,
        crate::skill::SkillProvenance::UserHome,
    )
}

fn user_subject(skill: &str) -> crate::skill::GrantSubject {
    user_source().grant_subject(skill)
}

fn project_source(root: &str) -> crate::skill::SkillSource {
    crate::skill::SkillSource::new(
        crate::skill::SkillFamily::Houyi,
        crate::skill::SkillProvenance::Project(crate::skill::ProjectIdentity::from_canonical_root(
            Path::new(root),
        )),
    )
}

fn fresh_store_with_content(content: &str) -> SkillGrantStore {
    let path = fresh_grant_path();
    fs::write(&path, content).expect("write test content");
    SkillGrantStore {
        grants: Mutex::new(load_grants(&path)),
        path,
    }
}

#[test]
fn test_grant_unknown_empty() {
    let store = fresh_store();
    assert!(store.grant_for(&user_subject("nope")).is_empty());
}

#[test]
fn test_grant_path_requires_home() {
    assert!(grant_path(None, None).is_err());
    assert!(grant_path(Some(OsString::new()), None).is_err());
    assert!(grant_path(Some(OsString::from("relative")), None).is_err());
    let home = env::temp_dir().join("grant-home");
    assert_eq!(
        grant_path(
            Some(OsString::from("relative")),
            Some(home.clone().into_os_string())
        )
        .expect("absolute fallback"),
        home.join(".houyicoder").join("skill-grants.json")
    );
}

#[test]
fn test_disk_round_trip() {
    let path = fresh_grant_path();
    {
        let store = SkillGrantStore::with_path(path.clone());
        store
            .set_grant(&user_subject("ego-browser"), vec!["a.b.c".into()])
            .expect("set grant");
    }
    let reloaded = SkillGrantStore::with_path(path.clone());
    assert_eq!(
        reloaded.grant_for(&user_subject("ego-browser")),
        vec!["a.b.c".to_string()]
    );
    let _ = fs::remove_dir_all(path.parent().unwrap()).is_ok();
}

#[test]
fn test_set_and_read() {
    let store = fresh_store();
    store
        .set_grant(&user_subject("ego-browser"), vec!["a.b.c".into()])
        .expect("set grant");
    assert_eq!(
        store.grant_for(&user_subject("ego-browser")),
        vec!["a.b.c".to_string()]
    );
}

/// The composite (skill, origin) key prevents a same-name untrusted
/// skill from consuming grants approved for a trusted/user copy.
/// A grant set under origin "user" must not be readable under
/// origin "project" — the central security property of origin-aware
/// grant keying.
#[test]
fn test_cross_origin_grant_isolation() {
    let store = fresh_store();
    store
        .set_grant(&user_subject("ego-browser"), vec!["a.b.c".into()])
        .expect("set grant");
    let project = project_source("/repo").grant_subject("ego-browser");
    assert!(
        store.grant_for(&project).is_empty(),
        "project provenance must not see user-home grants"
    );
    assert_eq!(
        store.grant_for(&user_subject("ego-browser")),
        vec!["a.b.c".to_string()],
        "user origin must still see its own grants"
    );
}

#[test]
fn test_add_returns_new_services() {
    let store = fresh_store();
    store
        .set_grant(&user_subject("ego-browser"), vec!["a.b.c".into()])
        .expect("set grant");
    let added = store
        .add_grants(
            &user_subject("ego-browser"),
            vec!["a.b.c".into(), "d.e.f".into(), "d.e.f".into()],
        )
        .expect("add grant");
    assert_eq!(added, vec!["d.e.f".to_string()]);
    assert_eq!(
        store.grant_for(&user_subject("ego-browser")),
        vec!["a.b.c".to_string(), "d.e.f".to_string()]
    );
}

#[test]
fn test_add_failure_rolls_back() {
    let dir = env::temp_dir().join(format!("houyi-grant-error-{}", process::id()));
    let _cleanup = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("mkdir persistence test");
    let store = SkillGrantStore::with_path(dir.clone());
    let result = store.add_grants(&user_subject("ego-browser"), vec!["a.b.c".into()]);
    assert!(result.is_err());
    assert!(store.grant_for(&user_subject("ego-browser")).is_empty());
    let _cleanup = fs::remove_dir_all(&dir);
}

#[test]
fn test_denied_filtered() {
    let store = fresh_store();
    // Every Apple namespace service is filtered before persistence,
    // including runtime-suffixed names observed in deny logs.
    store
        .set_grant(
            &user_subject("s1"),
            vec![
                "com.apple.pasteboard.1".into(),
                "com.apple.cfprefsd.daemon".into(),
                "a.b.c".into(),
            ],
        )
        .expect("set grant");
    let granted = store.grant_for(&user_subject("s1"));
    assert!(!granted.contains(&"com.apple.pasteboard.1".to_string()));
    assert!(!granted.contains(&"com.apple.cfprefsd.daemon".to_string()));
    assert!(granted.contains(&"a.b.c".to_string()));
}

#[test]
fn test_is_denied_apple_namespace() {
    assert!(is_denied("com.apple.pasteboard"));
    assert!(is_denied("com.apple.pasteboard.1"));
    assert!(is_denied("com.apple.systemadministration.writeconfig"));
    assert!(is_denied("com.apple.trustd"));
    assert!(is_denied("com.apple.appleeventsd"));
    assert!(is_denied("com.apple.pasteboardx"));
    assert!(is_denied("COM.APPLE.TCCD"));
}

#[test]
fn test_is_denied_namespace_boundary() {
    assert!(!is_denied("com.appleservice.example"));
    assert!(!is_denied("com.apple"));
    assert!(!is_denied("com.citrolabs.ego.lite.ego-browser"));
    assert!(!is_denied("com.houyi.test.entitlement"));
}

/// System services a sandboxed helper probes but does not need (observed
/// surfacing on an ego-browser run) are denied so the approval card
/// never offers them. The app-launch entitlement grants its own
/// LaunchServices lookups directly, so denying them here blocks only
/// the per-skill grant path.
#[test]
fn test_is_denied_system_probes() {
    assert!(is_denied("com.apple.analyticsd"));
    assert!(is_denied("com.apple.dock.server"));
    assert!(is_denied("com.apple.CoreServices.coreservicesd"));
    assert!(is_denied("com.apple.coreservices.quarantine-resolver"));
}

#[test]
fn test_corrupt_file_returns_empty() {
    let store = fresh_store_with_content("not valid json {{{");
    assert!(store.grant_for(&user_subject("any")).is_empty());
}

#[test]
fn test_resolve_deny_all_feeders() {
    let store = fresh_store();
    // Grant-store input is filtered at the namespace boundary.
    store
        .set_grant(
            &user_subject("s1"),
            vec!["com.apple.pasteboard.1".into(), "x.y.z".into()],
        )
        .expect("set grant");
    // Frontmatter side with a deny-listed service + an overlap entry.
    let frontmatter = vec![
        "com.apple.cfprefsd.daemon".into(),
        "x.y.z".into(),
        "a.b.c".into(),
    ];
    let (mach, allow_launch) = store.resolve("s1", &user_source(), &frontmatter, false);
    // Deny-list entries from all feeders gone, including suffixed.
    assert!(!mach.contains(&"com.apple.pasteboard.1".to_string()));
    assert!(!mach.contains(&"com.apple.cfprefsd.daemon".to_string()));
    // Union of survivors, no dup from overlap.
    assert_eq!(mach, vec!["x.y.z".to_string(), "a.b.c".to_string()]);
    assert!(!allow_launch);
}

#[test]
fn test_resolve_frontmatter_only() {
    let store = fresh_store();
    // Suffixed variant of a denied root must be filtered here too.
    let frontmatter = vec!["a.b.c".into(), "com.apple.pasteboard.1".into()];
    let (mach, allow_launch) = store.resolve("unknown", &user_source(), &frontmatter, false);
    assert_eq!(mach, vec!["a.b.c".to_string()]);
    assert!(!allow_launch);
}

#[test]
fn test_resolve_profile_hit() {
    let store = fresh_store();
    // ego-browser is in the compiled profile; no frontmatter or grant
    // store entry needed. The profile grants app launch (ego-browser
    // starts the ego lite app via LaunchServices) and the bootstrap
    // mach service the ego lite process connects to.
    let (mach, allow_launch) = store.resolve("ego-browser", &user_source(), &[], false);
    assert_eq!(
        mach,
        vec!["com.citrolabs.ego.lite.ego-browser".to_string()],
        "ego-browser profile declares its bootstrap mach service"
    );
    assert!(allow_launch, "ego-browser profile grants app launch");
    let ecosystem = crate::skill::SkillSource::new(
        crate::skill::SkillFamily::Agents,
        crate::skill::SkillProvenance::UserHome,
    );
    let (mach, allow_launch) =
        resolve_entitlements(None, "ego-browser", Some(&ecosystem), &[], false);
    assert_eq!(mach, vec!["com.citrolabs.ego.lite.ego-browser"]);
    assert!(
        allow_launch,
        "the host profile does not require persistence"
    );
}

#[test]
fn test_resolve_none_deny() {
    // None store: frontmatter alone, deny-list still applies to
    // suffixed variants.
    let frontmatter = vec!["a.b.c".into(), "com.apple.pasteboard.1".into()];
    let (mach, allow_launch) =
        resolve_entitlements(None, "any", Some(&user_source()), &frontmatter, true);
    assert_eq!(mach, vec!["a.b.c".to_string()]);
    assert!(allow_launch);
}

#[test]
fn test_untrusted_skips_declarations() {
    // A project-sourced skill naming itself ego-browser must NOT get
    // the compiled profile (allow_app_launch: true, mach services)
    // and must NOT get its own frontmatter declarations. Only the
    // grant store feeds in — and only with services the user already
    // explicitly approved.
    let store = fresh_store();
    // Pre-seed the grant store as if the user had approved one service
    // for this skill through the entitlement card.
    let project = project_source("/repo");
    store
        .set_grant(
            &project.grant_subject("ego-browser"),
            vec!["user.approved.svc".into()],
        )
        .expect("set grant");
    // Frontmatter declares a system service + a regular service, and
    // requests app launch. All of these must be ignored.
    let frontmatter = vec![
        "com.apple.pasteboard.1".into(),
        "attacker.declared.svc".into(),
    ];
    let (mach, allow_launch) = store.resolve("ego-browser", &project, &frontmatter, true);
    // Only the user-approved grant store service survives.
    assert_eq!(mach, vec!["user.approved.svc".to_string()]);
    assert!(!allow_launch, "untrusted source must not get app launch");
}

#[test]
fn test_resolve_untrusted_no_store() {
    // Untrusted + no store: nothing at all, even with frontmatter
    // declaring services and app launch.
    let frontmatter = vec!["a.b.c".into(), "d.e.f".into()];
    let (mach, allow_launch) = resolve_entitlements(None, "any", None, &frontmatter, true);
    assert!(mach.is_empty());
    assert!(!allow_launch);
}

#[test]
fn test_profile_ignores_frontmatter() {
    use crate::skill::{SkillFamily, SkillProvenance, SkillSource};

    let store = fresh_store();
    let source = SkillSource::new(SkillFamily::ClaudeEco, SkillProvenance::UserHome);
    let frontmatter = vec!["attacker.declared.svc".to_string()];
    let (mach, allow_launch) = store.resolve("ego-browser", &source, &frontmatter, true);
    assert_eq!(mach, vec!["com.citrolabs.ego.lite.ego-browser"]);
    assert!(allow_launch, "the host profile applies to user-home skills");
    assert!(
        !mach.contains(&"attacker.declared.svc".to_string()),
        "ecosystem frontmatter cannot self-grant services"
    );
}

#[test]
fn test_grants_isolate_provenance() {
    use crate::skill::{ProjectIdentity, SkillFamily, SkillProvenance, SkillSource};

    let store = fresh_store();
    let user = SkillSource::new(SkillFamily::Agents, SkillProvenance::UserHome);
    let project = SkillSource::new(
        SkillFamily::Agents,
        SkillProvenance::Project(ProjectIdentity::from_canonical_root(Path::new("/repo/one"))),
    );
    let user_subject = user.grant_subject("shared");
    let project_subject = project.grant_subject("shared");
    store
        .add_grants(&user_subject, vec!["user.service".into()])
        .expect("add user grant");
    store
        .add_grants(&project_subject, vec!["project.service".into()])
        .expect("add project grant");
    assert_eq!(store.grant_for(&user_subject), vec!["user.service"]);
    assert_eq!(store.grant_for(&project_subject), vec!["project.service"]);
}

#[test]
fn test_projects_isolate_grants() {
    use crate::skill::{GrantSubject, ProjectIdentity, SkillFamily, SkillProvenance, SkillSource};

    let one = SkillSource::new(
        SkillFamily::Houyi,
        SkillProvenance::Project(ProjectIdentity::from_canonical_root(Path::new("/repo/one"))),
    );
    let two = SkillSource::new(
        SkillFamily::Houyi,
        SkillProvenance::Project(ProjectIdentity::from_canonical_root(Path::new("/repo/two"))),
    );
    let subject = one.grant_subject("shared");
    assert_ne!(subject, two.grant_subject("shared"));
    assert_eq!(GrantSubject::from_json(&subject.to_json()), Some(subject));
}

#[cfg(windows)]
#[test]
fn test_project_identity_folds_case() {
    use crate::skill::ProjectIdentity;

    assert_eq!(
        ProjectIdentity::from_canonical_root(Path::new(r"C:\Repo")),
        ProjectIdentity::from_canonical_root(Path::new(r"c:\repo"))
    );
}

#[test]
fn test_subject_rejects_forgery() {
    use crate::skill::GrantSubject;

    for value in [
        serde_json::json!({"skill":"../x", "kind":"user_home", "identity":null}),
        serde_json::json!({"skill":"shared", "kind":"project", "identity":"short"}),
        serde_json::json!({"skill":"shared", "kind":"remote", "identity":""}),
    ] {
        assert!(GrantSubject::from_json(&value).is_none());
    }
}
