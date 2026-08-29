//! Skill resource manifest: files alongside the body in the skill
//! directory (reference notes, scripts, templates), listed by name +
//! path without loading content. Only machine-local or workspace-
//! trusted sources surface a manifest; shared-repo + remote sources
//! are excluded (a crafted file set must not reach the model unheard).

use std::path::Path;

use crate::definition::SkillSource;

/// A file in a skill directory that is not the SKILL.md body itself,
/// surfaced as a name + relative path + kind so the model can request
/// the file or run a script by path without a prior discovery call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceEntry {
    /// File name (e.g. "deploy.py").
    pub name: String,
    /// Path relative to the skill directory (e.g. "scripts/deploy.py").
    pub rel_path: String,
    pub kind: ResourceKind,
}

/// The role a resource file plays, inferred from its location + name.
/// Script entries mark executable commands; the others are reference
/// material the model may read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceKind {
    Script,
    Reference,
    Template,
    Data,
}

/// Whether a source may surface a resource manifest. Managed, user, and
/// project sources are machine-local or workspace-trusted (the trust gate
/// acknowledges the workspace before any project skill runs), so their
/// directories may be listed. Ecosystem-compat paths (shared repositories)
/// and remote servers are excluded.
pub fn is_manifest_eligible(source: &SkillSource) -> bool {
    matches!(
        source,
        SkillSource::Managed | SkillSource::User | SkillSource::Project
    )
}

/// Scan a skill directory for resource files. Returns one entry per file
/// except SKILL.md (the body, already loaded) and hidden files (editor +
/// VCS artifacts). Sorted by relative path for stable output. A missing
/// or unreadable directory yields an empty list — a manifest failure
/// must not break the body it accompanies.
pub fn list_resources(skill_dir: &Path) -> Vec<ResourceEntry> {
    let mut out = Vec::new();
    walk(skill_dir, skill_dir, &mut out);
    out.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    out
}

/// Recursive directory walk. Hidden segments (any path component starting
/// with a dot) are pruned so VCS metadata and editor artifacts do not
/// clutter the manifest.
fn walk(root: &Path, dir: &Path, out: &mut Vec<ResourceEntry>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(rel) = path.strip_prefix(root) else {
            continue;
        };
        let rel_str = rel.to_string_lossy();
        if rel_str
            .split(std::path::MAIN_SEPARATOR)
            .any(|seg| seg.starts_with('.'))
        {
            continue;
        }
        if path.is_dir() {
            walk(root, &path, out);
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == "SKILL.md" {
            continue;
        }
        // A control char anywhere in the relative path (file name or a
        // parent directory name) cannot be safely represented on one
        // manifest line — a crafted path could forge an extra entry. Skip
        // such degenerate files rather than mangle the path the model
        // would address.
        if rel_str.chars().any(|c| c.is_control()) {
            continue;
        }
        let kind = classify(&rel_str, &name);
        out.push(ResourceEntry {
            name,
            rel_path: rel_str.into(),
            kind,
        });
    }
}

/// Infer a file's role from its location + name. Files under a scripts
/// directory, or with a script extension anywhere, are Scripts; files
/// under templates are Templates; markdown is Reference; the rest is Data.
fn classify(rel: &str, name: &str) -> ResourceKind {
    let lower = name.to_ascii_lowercase();
    let first_seg = rel.split(std::path::MAIN_SEPARATOR).next().unwrap_or("");
    if first_seg == "scripts" || is_script_ext(&lower) {
        return ResourceKind::Script;
    }
    if first_seg == "templates" {
        return ResourceKind::Template;
    }
    if lower == "reference.md" || lower.ends_with(".md") {
        return ResourceKind::Reference;
    }
    ResourceKind::Data
}

fn is_script_ext(name: &str) -> bool {
    let exts = [
        ".py", ".sh", ".bash", ".js", ".mjs", ".ts", ".rb", ".pl", ".lua", ".go", ".rs",
    ];
    exts.iter().any(|e| name.ends_with(e))
}

/// Format the manifest as a compact block appended to the body. Returns
/// an empty string when there are no resources so the body stays clean.
pub fn format_manifest(entries: &[ResourceEntry]) -> String {
    if entries.is_empty() {
        return String::new();
    }
    let mut lines = String::from("\n\nResources in this skill's directory:\n");
    for e in entries {
        let label = match e.kind {
            ResourceKind::Script => "script",
            ResourceKind::Reference => "reference",
            ResourceKind::Template => "template",
            ResourceKind::Data => "file",
        };
        lines.push_str(&format!("- {label}: {}\n", e.rel_path));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a skill dir fixture on disk with the given relative file
    /// contents. SKILL.md is always written so the dir is valid. The
    /// caller drops the returned path with remove_dir_all when done.
    fn skill_dir_with(label: &str, files: &[(&str, &str)]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("skill-res-{label}-{}", std::process::id()));
        drop(std::fs::remove_dir_all(&dir));
        std::fs::create_dir_all(&dir).expect("mkdir");
        for (rel, content) in files {
            let p = dir.join(rel);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).expect("mkdir");
            }
            std::fs::write(p, content).expect("write");
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
        drop(std::fs::remove_dir_all(&dir));
    }

    #[test]
    fn test_excludes_skill_md() {
        let dir = skill_dir_with("skillmd", &[("SKILL.md", "body\n")]);
        let res = list_resources(&dir);
        assert!(res.is_empty(), "SKILL.md alone yields no resources");
        drop(std::fs::remove_dir_all(&dir));
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
        drop(std::fs::remove_dir_all(&dir));
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
        drop(std::fs::remove_dir_all(&dir));
    }

    #[test]
    fn test_skips_control_char_names() {
        // A path with a control char — in the file name or a parent
        // directory name — could forge an extra manifest line; such
        // degenerate entries are skipped, not mangled into the path.
        let dir = skill_dir_with("ctrl", &[("SKILL.md", "body\n"), ("ok.py", "1\n")]);
        std::fs::write(dir.join("evil\ninject.py"), "x").expect("write");
        std::fs::create_dir_all(dir.join("bad\n dir")).expect("mkdir");
        std::fs::write(dir.join("bad\n dir/x.py"), "y").expect("write");
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
        drop(std::fs::remove_dir_all(&dir));
    }

    #[test]
    fn test_missing_dir_is_empty() {
        let res = list_resources(std::path::Path::new("/nonexistent/skill/dir/no/exist"));
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
    }

    #[test]
    fn test_manifest_eligibility() {
        assert!(is_manifest_eligible(&SkillSource::Managed));
        assert!(is_manifest_eligible(&SkillSource::User));
        assert!(is_manifest_eligible(&SkillSource::Project));
        assert!(!is_manifest_eligible(&SkillSource::ClaudeEco));
        assert!(!is_manifest_eligible(&SkillSource::Agents));
        assert!(!is_manifest_eligible(&SkillSource::Mcp));
        assert!(!is_manifest_eligible(&SkillSource::Local));
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
}
