use super::*;
use std::env::temp_dir;
use std::fs::create_dir_all;
use std::fs::remove_dir_all;
use std::fs::remove_file;
use std::fs::write;
#[cfg(unix)]
use std::os::unix::fs::symlink;
use std::path::Path;
use std::path::PathBuf;
use std::process::id;

/// Build a skill dir fixture on disk with the given relative file
/// contents. SKILL.md is always written so the dir is valid. The
/// caller drops the returned path with remove_dir_all when done.
fn skill_dir_with(label: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir = temp_dir().join(format!("skill-res-{label}-{}", id()));
    drop(remove_dir_all(&dir));
    create_dir_all(&dir).expect("mkdir");
    for (rel, content) in files {
        let p = dir.join(rel);
        if let Some(parent) = p.parent() {
            create_dir_all(parent).expect("mkdir");
        }
        write(p, content).expect("write");
    }
    dir
}

#[test]
fn test_lists_scripts_and_reference() {
    let dir = skill_dir_with(
        "list",
        &[
            ("SKILL.md", "---\nname: x\n---\nbody\n"),
            ("scripts/deploy.py", "print('hi')\n"),
            ("reference.md", "# how to use\n"),
        ],
    );
    let res = list_resources(&dir);
    assert_eq!(res.len(), 2, "SKILL.md excluded: {:?}", res);
    let kinds: Vec<_> = res.iter().map(|r| r.kind).collect();
    assert!(
        kinds.contains(&ResourceKind::Script),
        "deploy.py is a script"
    );
    assert!(
        kinds.contains(&ResourceKind::Reference),
        "reference.md is reference"
    );
    drop(remove_dir_all(&dir));
}

#[test]
fn test_excludes_skill_md() {
    let dir = skill_dir_with("skillmd", &[("SKILL.md", "body\n")]);
    let res = list_resources(&dir);
    assert!(res.is_empty(), "SKILL.md alone yields no resources");
    drop(remove_dir_all(&dir));
}

#[test]
fn test_skips_hidden_segments() {
    let dir = skill_dir_with(
        "hidden",
        &[
            ("SKILL.md", "body\n"),
            (".git/config", "git\n"),
            (".DS_Store", "store\n"),
            ("scripts/.keep", "\n"),
            ("scripts/run.sh", "#!/bin/sh\necho\n"),
        ],
    );
    let res = list_resources(&dir);
    let paths: Vec<_> = res.iter().map(|r| r.rel_path.clone()).collect();
    assert!(paths.contains(&"scripts/run.sh".to_string()), "{paths:?}");
    assert!(
        !paths
            .iter()
            .any(|p| p.starts_with(".git") || p.contains(".DS_Store")),
        "hidden artifacts pruned: {paths:?}"
    );
    assert!(
        !paths.iter().any(|p| p.contains(".keep")),
        "hidden file under scripts pruned: {paths:?}"
    );
    drop(remove_dir_all(&dir));
}

#[test]
fn test_sorted_by_relative_path() {
    let dir = skill_dir_with(
        "sort",
        &[
            ("SKILL.md", "body\n"),
            ("zeta.py", "1\n"),
            ("alpha.py", "2\n"),
            ("scripts/mid.py", "3\n"),
            ("scripts/aa.py", "4\n"),
        ],
    );
    let res = list_resources(&dir);
    let paths: Vec<_> = res.iter().map(|r| r.rel_path.clone()).collect();
    assert_eq!(
        paths,
        vec![
            "alpha.py".to_string(),
            "scripts/aa.py".to_string(),
            "scripts/mid.py".to_string(),
            "zeta.py".to_string(),
        ],
        "sorted by relative path: {paths:?}"
    );
    drop(remove_dir_all(&dir));
}

#[test]
#[cfg(unix)]
fn test_skips_control_char_names() {
    // A path with a control char — in the file name or a parent
    // directory name — could forge an extra manifest line; such
    // degenerate entries are skipped, not mangled into the path.
    let dir = skill_dir_with("ctrl", &[("SKILL.md", "body\n"), ("ok.py", "1\n")]);
    write(dir.join("evil\ninject.py"), "x").expect("write");
    create_dir_all(dir.join("bad\n dir")).expect("mkdir");
    write(dir.join("bad\n dir/x.py"), "y").expect("write");
    let res = list_resources(&dir);
    let paths: Vec<_> = res.iter().map(|r| r.rel_path.clone()).collect();
    assert!(
        paths.contains(&"ok.py".to_string()),
        "clean file listed: {paths:?}"
    );
    assert!(
        !paths
            .iter()
            .any(|p| p.contains("inject") || p.contains("bad")),
        "control-char path (file or dir name) skipped: {paths:?}"
    );
    drop(remove_dir_all(&dir));
}

#[test]
fn test_missing_dir_is_empty() {
    let res = list_resources(Path::new("/nonexistent/skill/dir/no/exist"));
    assert!(res.is_empty(), "a missing dir yields an empty manifest");
}

#[test]
fn test_classify_kinds() {
    assert_eq!(
        classify("scripts/deploy.py", "deploy.py"),
        ResourceKind::Script
    );
    assert_eq!(classify("run.sh", "run.sh"), ResourceKind::Script);
    assert_eq!(
        classify("templates/scaffold.txt", "scaffold.txt"),
        ResourceKind::Template
    );
    assert_eq!(
        classify("reference.md", "reference.md"),
        ResourceKind::Reference
    );
    assert_eq!(classify("notes.md", "notes.md"), ResourceKind::Reference);
    assert_eq!(classify("data.csv", "data.csv"), ResourceKind::Data);
    // The bare "scripts" directory name is not a script file: a path
    // that is exactly the directory carries no file to run.
    assert_eq!(classify("scripts", "scripts"), ResourceKind::Data);
}

#[test]
fn test_manifest_eligibility() {
    let managed = SkillSource::new(SkillFamily::Houyi, SkillProvenance::Managed);
    let user = SkillSource::new(SkillFamily::Houyi, SkillProvenance::UserHome);
    let project = SkillSource::new(
        SkillFamily::Houyi,
        SkillProvenance::Project {
            root: PathBuf::from("/repo"),
        },
    );
    let ecosystem = SkillSource::new(SkillFamily::ClaudeEco, SkillProvenance::UserHome);
    let remote = SkillSource::new(
        SkillFamily::Mcp,
        SkillProvenance::Remote {
            server: "server".into(),
        },
    );
    assert!(is_manifest_eligible(&managed));
    assert!(is_manifest_eligible(&user));
    assert!(is_manifest_eligible(&project));
    assert!(!is_manifest_eligible(&ecosystem));
    assert!(!is_manifest_eligible(&remote));
}

#[test]
fn test_format_empty_manifest() {
    assert_eq!(format_manifest(&[]), "", "no resources -> no block");
}

#[test]
fn test_format_label_path() {
    let entries = vec![
        ResourceEntry {
            name: "deploy.py".into(),
            rel_path: "scripts/deploy.py".into(),
            kind: ResourceKind::Script,
        },
        ResourceEntry {
            name: "reference.md".into(),
            rel_path: "reference.md".into(),
            kind: ResourceKind::Reference,
        },
    ];
    let out = format_manifest(&entries);
    assert!(out.contains("Resources in this skill's directory"), "{out}");
    assert!(out.contains("- script: scripts/deploy.py"), "{out}");
    assert!(out.contains("- reference: reference.md"), "{out}");
}

/// PRUNED_DIRS (node_modules, target, etc.) are skipped so a symlink
/// to a large dependency tree does not flood the manifest.
#[test]
fn test_pruned_dirs_skipped() {
    let dir = skill_dir_with(
        "pruned",
        &[
            ("SKILL.md", "body\n"),
            ("scripts/run.sh", "#!/bin/sh\necho\n"),
            ("node_modules/large/deep/file.js", "x\n"),
            ("target/debug/app", "y\n"),
        ],
    );
    let res = list_resources(&dir);
    let paths: Vec<_> = res.iter().map(|r| r.rel_path.clone()).collect();
    assert!(
        paths.contains(&"scripts/run.sh".to_string()),
        "normal script listed: {paths:?}"
    );
    assert!(
        !paths.iter().any(|p| p.starts_with("node_modules")),
        "node_modules pruned: {paths:?}"
    );
    assert!(
        !paths.iter().any(|p| p.starts_with("target")),
        "target pruned: {paths:?}"
    );
    drop(remove_dir_all(&dir));
}

/// The entry cap prevents a skill dir with thousands of data files from
/// flooding the body with a listing the model cannot use.
#[test]
fn test_entry_cap_holds() {
    let dir = skill_dir_with("cap", &[("SKILL.md", "body\n")]);
    for i in 0..210 {
        write(dir.join(format!("f{i}.dat")), "x").expect("write");
    }
    let res = list_resources(&dir);
    assert!(
        res.len() <= MAX_RESOURCE_ENTRIES,
        "entry cap holds: {} > {}",
        res.len(),
        MAX_RESOURCE_ENTRIES
    );
    drop(remove_dir_all(&dir));
}

/// A symlink pointing outside the skill dir is not followed — the walk
/// is contained to the root's canonical subtree. Without this, a symlink
/// to /etc or the parent would list external files (info leak + budget
/// exhaustion).
#[cfg(unix)]
#[test]
fn test_symlink_escape_contained() {
    let dir = skill_dir_with(
        "escape",
        &[
            ("SKILL.md", "body\n"),
            ("scripts/run.sh", "#!/bin/sh\necho\n"),
        ],
    );
    // A symlink to the parent dir (escape attempt).
    symlink("..", dir.join("escape-link")).expect("symlink");
    // A file in the parent that should NOT appear in the manifest.
    let parent_secret = dir.parent().unwrap().join("secret.dat");
    write(&parent_secret, "sensitive").expect("write");
    let res = list_resources(&dir);
    let paths: Vec<_> = res.iter().map(|r| r.rel_path.clone()).collect();
    assert!(
        paths.contains(&"scripts/run.sh".to_string()),
        "skill's own files listed: {paths:?}"
    );
    assert!(
        !paths.iter().any(|p| p.starts_with("escape-link")),
        "escape symlink not followed: {paths:?}"
    );
    remove_file(&parent_secret).ok();
    drop(remove_dir_all(&dir));
}
