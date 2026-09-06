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
    assert!(store.grant_for("nope", "user").is_empty());
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
            .set_grant("ego-browser", "user", vec!["a.b.c".into()])
            .expect("set grant");
    }
    let reloaded = SkillGrantStore::with_path(path.clone());
    assert_eq!(
        reloaded.grant_for("ego-browser", "user"),
        vec!["a.b.c".to_string()]
    );
    let _ = fs::remove_dir_all(path.parent().unwrap()).is_ok();
}

#[test]
fn test_set_and_read() {
    let store = fresh_store();
    store
        .set_grant("ego-browser", "user", vec!["a.b.c".into()])
        .expect("set grant");
    assert_eq!(
        store.grant_for("ego-browser", "user"),
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
        .set_grant("ego-browser", "user", vec!["a.b.c".into()])
        .expect("set grant");
    assert!(
        store.grant_for("ego-browser", "project").is_empty(),
        "project origin must not see user-origin grants"
    );
    assert!(
        store.grant_for("ego-browser", "agents").is_empty(),
        "agents origin must not see user-origin grants"
    );
    assert_eq!(
        store.grant_for("ego-browser", "user"),
        vec!["a.b.c".to_string()],
        "user origin must still see its own grants"
    );
}

#[test]
fn test_add_returns_new_services() {
    let store = fresh_store();
    store
        .set_grant("ego-browser", "user", vec!["a.b.c".into()])
        .expect("set grant");
    let added = store
        .add_grants(
            "ego-browser",
            "user",
            vec!["a.b.c".into(), "d.e.f".into(), "d.e.f".into()],
        )
        .expect("add grant");
    assert_eq!(added, vec!["d.e.f".to_string()]);
    assert_eq!(
        store.grant_for("ego-browser", "user"),
        vec!["a.b.c".to_string(), "d.e.f".to_string()]
    );
}

#[test]
fn test_add_failure_rolls_back() {
    let dir = env::temp_dir().join(format!("houyi-grant-error-{}", process::id()));
    let _cleanup = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("mkdir persistence test");
    let store = SkillGrantStore::with_path(dir.clone());
    let result = store.add_grants("ego-browser", "user", vec!["a.b.c".into()]);
    assert!(result.is_err());
    assert!(store.grant_for("ego-browser", "user").is_empty());
    let _cleanup = fs::remove_dir_all(&dir);
}

#[test]
fn test_denied_filtered() {
    let store = fresh_store();
    // Every Apple namespace service is filtered before persistence,
    // including runtime-suffixed names observed in deny logs.
    store
        .set_grant(
            "s1",
            "user",
            vec![
                "com.apple.pasteboard.1".into(),
                "com.apple.cfprefsd.daemon".into(),
                "a.b.c".into(),
            ],
        )
        .expect("set grant");
    let granted = store.grant_for("s1", "user");
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
    assert!(store.grant_for("any", "user").is_empty());
}

#[test]
fn test_resolve_deny_all_feeders() {
    let store = fresh_store();
    // Grant-store input is filtered at the namespace boundary.
    store
        .set_grant(
            "s1",
            "user",
            vec!["com.apple.pasteboard.1".into(), "x.y.z".into()],
        )
        .expect("set grant");
    // Frontmatter side with a deny-listed service + an overlap entry.
    let frontmatter = vec![
        "com.apple.cfprefsd.daemon".into(),
        "x.y.z".into(),
        "a.b.c".into(),
    ];
    let (mach, allow_launch) = store.resolve("s1", "user", &frontmatter, false, true);
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
    let (mach, allow_launch) = store.resolve("unknown", "user", &frontmatter, false, true);
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
    let (mach, allow_launch) = store.resolve("ego-browser", "user", &[], false, true);
    assert_eq!(
        mach,
        vec!["com.citrolabs.ego.lite.ego-browser".to_string()],
        "ego-browser profile declares its bootstrap mach service"
    );
    assert!(allow_launch, "ego-browser profile grants app launch");
}

#[test]
fn test_resolve_none_deny() {
    // None store: frontmatter alone, deny-list still applies to
    // suffixed variants.
    let frontmatter = vec!["a.b.c".into(), "com.apple.pasteboard.1".into()];
    let (mach, allow_launch) = resolve_entitlements(None, "any", "user", &frontmatter, true, true);
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
    store
        .set_grant("ego-browser", "project", vec!["user.approved.svc".into()])
        .expect("set grant");
    // Frontmatter declares a system service + a regular service, and
    // requests app launch. All of these must be ignored.
    let frontmatter = vec![
        "com.apple.pasteboard.1".into(),
        "attacker.declared.svc".into(),
    ];
    let (mach, allow_launch) = store.resolve("ego-browser", "project", &frontmatter, true, false);
    // Only the user-approved grant store service survives.
    assert_eq!(mach, vec!["user.approved.svc".to_string()]);
    assert!(!allow_launch, "untrusted source must not get app launch");
}

#[test]
fn test_resolve_untrusted_no_store() {
    // Untrusted + no store: nothing at all, even with frontmatter
    // declaring services and app launch.
    let frontmatter = vec!["a.b.c".into(), "d.e.f".into()];
    let (mach, allow_launch) = resolve_entitlements(None, "any", "user", &frontmatter, true, false);
    assert!(mach.is_empty());
    assert!(!allow_launch);
}

#[test]
fn test_entitlement_trust_boundary() {
    // Only managed and user origins are trusted for entitlements.
    assert!(is_entitlement_trusted_origin("managed"));
    assert!(is_entitlement_trusted_origin("user"));
    // All other origins — including agents, claude_eco, local —
    // are untrusted: a repo can ship .claude/skills or
    // .agents/skills, so these must go through deny-log discovery.
    assert!(!is_entitlement_trusted_origin("agents"));
    assert!(!is_entitlement_trusted_origin("claude_eco"));
    assert!(!is_entitlement_trusted_origin("local"));
    assert!(!is_entitlement_trusted_origin("project"));
    assert!(!is_entitlement_trusted_origin("mcp"));
    // Unknown origin fails closed.
    assert!(!is_entitlement_trusted_origin(""));
    assert!(!is_entitlement_trusted_origin("unknown"));
}
