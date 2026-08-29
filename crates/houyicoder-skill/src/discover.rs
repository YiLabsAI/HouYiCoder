//! Skill discovery: scans skill directories across four directory families
//! and three levels, with precedence managed > project > user and
//! conflict handling (realpath dedup, precedence-based shadowing with
//! warn-level alert).
//!
//! Does NOT respect .gitignore (config dirs are often gitignored local
//! config that teams still want loaded).

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use super::definition::{SkillDefinition, SkillSource};
use super::parse;

/// Maximum recursion depth when walking for SKILL.md files.
const MAX_WALK_DEPTH: usize = 5;

/// Maximum number of skills to collect before stopping (bound).
const MAX_SKILL_COUNT: usize = 2000;

/// Directory names that are pruned during the walk (never recursed into).
const PRUNED_DIRS: &[&str] = &[".git", "node_modules", ".svn", "target"];

/// Config directory families, in precedence order within the same level
/// (highest first). Each is joined under a scan root to form the skills
/// directory: root/family/skills.
const CONFIG_DIR_FAMILIES: &[&str] = &[".houyicoder", ".claude", ".agents"];

/// System-level managed skills directory (enterprise policy push).
const MANAGED_DIR: &str = "/etc/houyicoder/skills";

/// Entry point: discover all skills from the filesystem.
///
/// Scans managed, user (when a home directory is given), and project
/// levels. Returns skills sorted by precedence (highest first). Same-name
/// skills across sources are resolved by precedence; the loser is shadowed
/// with a warn-level log. A None home skips the user level so a caller that
/// wants only the project + managed levels is not coupled to the process's
/// real home directory.
pub fn discover_skills(cwd: Option<&Path>, home: Option<&Path>) -> Vec<SkillDefinition> {
    let mut all: Vec<(SkillDefinition, u8)> = Vec::new();
    let mut seen_canonical: HashSet<PathBuf> = HashSet::new();
    let mut seen_names: HashSet<String> = HashSet::new();

    // Level 1: managed (highest precedence).
    collect_from_dir(
        Path::new(MANAGED_DIR),
        SkillSource::Managed,
        0,
        &mut all,
        &mut seen_canonical,
        &mut seen_names,
    );

    // Level 2: project (walk cwd up to git root).
    if let Some(cwd_raw) = cwd {
        // Canonicalize before walking so path comparisons against the
        // canonical git_root are correct (handles symlinks in cwd).
        let cwd = dunce::canonicalize(cwd_raw).unwrap_or_else(|_| cwd_raw.to_path_buf());
        let git_root = find_git_root(&cwd);
        let walk_dirs: Vec<PathBuf> = walk_up_to_root(&cwd, git_root.as_deref());
        for dir in &walk_dirs {
            for (i, family) in CONFIG_DIR_FAMILIES.iter().enumerate() {
                let skills_dir = dir.join(family).join("skills");
                let source = project_source_for_family(i);
                collect_from_dir(
                    &skills_dir,
                    source,
                    1,
                    &mut all,
                    &mut seen_canonical,
                    &mut seen_names,
                );
            }
        }
    }

    // Level 3: user (lowest precedence). Skipped when home is None so a
    // caller that wants only the project + managed levels (a hermetic
    // test) is not coupled to the process's real home directory.
    if let Some(home) = home {
        for (i, family) in CONFIG_DIR_FAMILIES.iter().enumerate() {
            let skills_dir = home.join(family).join("skills");
            let source = user_source_for_family(i);
            collect_from_dir(
                &skills_dir,
                source,
                2,
                &mut all,
                &mut seen_canonical,
                &mut seen_names,
            );
        }
    }

    // Sort by precedence (lowest u8 = highest priority), then by name.
    all.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.name.cmp(&b.0.name)));

    all.into_iter().map(|(def, _)| def).collect()
}

/// Collect and parse skills from a single skills directory.
#[allow(clippy::too_many_lines)]
fn collect_from_dir(
    skills_dir: &Path,
    source: SkillSource,
    precedence: u8,
    all: &mut Vec<(SkillDefinition, u8)>,
    seen_canonical: &mut HashSet<PathBuf>,
    seen_names: &mut HashSet<String>,
) {
    if !skills_dir.is_dir() {
        return;
    }

    let mut skill_paths = Vec::new();
    walk_for_skill_md(skills_dir, &mut skill_paths, 0);

    for skill_md_path in skill_paths {
        if all.len() >= MAX_SKILL_COUNT {
            tracing::warn!(
                dir = %skills_dir.display(),
                count = MAX_SKILL_COUNT,
                "skill count limit reached; stopping scan"
            );
            break;
        }

        // Dedup by canonical path (handles symlinks and overlapping roots).
        let canonical =
            dunce::canonicalize(&skill_md_path).unwrap_or_else(|_| skill_md_path.clone());
        if !seen_canonical.insert(canonical) {
            continue;
        }

        let skill_dir = skill_md_path.parent().unwrap_or(Path::new("."));
        let dir_name = skill_dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown");

        let text = match std::fs::read_to_string(&skill_md_path) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(path = %skill_md_path.display(), error = %e, "failed to read SKILL.md");
                continue;
            }
        };

        match parse::parse_skill(&text, dir_name, skill_dir, &skill_md_path, source.clone()) {
            Ok(def) => {
                // Name-based shadowing: first-seen (higher precedence) wins.
                if !seen_names.insert(def.name.clone()) {
                    tracing::warn!(
                        skill = %def.name,
                        source = ?source,
                        "skill shadowed by a higher-precedence source; skipped"
                    );
                    continue;
                }
                all.push((def, precedence));
            }
            Err(e) => {
                tracing::warn!(
                    path = %skill_md_path.display(),
                    error = %e,
                    "failed to parse skill; skipped"
                );
            }
        }
    }
}

/// Recursively walk a directory looking for SKILL.md files.
///
/// Visits entries in lexicographic order so name-collision handling
/// (first-seen-wins) is deterministic across filesystems.
fn walk_for_skill_md(dir: &Path, paths: &mut Vec<PathBuf>, depth: usize) {
    if depth > MAX_WALK_DEPTH {
        return;
    }

    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };

    let mut subdirs: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    subdirs.sort();

    for subdir in subdirs {
        // Prune .git, node_modules, etc.
        if let Some(name) = subdir.file_name().and_then(|n| n.to_str())
            && PRUNED_DIRS.contains(&name)
        {
            continue;
        }

        let skill_md = subdir.join("SKILL.md");
        if skill_md.is_file() {
            paths.push(skill_md);
        }
        walk_for_skill_md(&subdir, paths, depth + 1);
    }
}

/// Find the git repository root by walking up from start looking for a
/// .git file or directory. Does not require the git2 crate.
fn find_git_root(start: &Path) -> Option<PathBuf> {
    let canonical = dunce::canonicalize(start).ok()?;
    let mut current: Option<&Path> = Some(&canonical);
    while let Some(dir) = current {
        if dir.join(".git").exists() {
            return Some(dir.to_path_buf());
        }
        current = dir.parent();
    }
    None
}

/// Collect directories from start up to root (inclusive), root-first
/// (closer to cwd overrides parent dirs in project precedence).
fn walk_up_to_root(start: &Path, root: Option<&Path>) -> Vec<PathBuf> {
    let Some(root) = root else {
        return vec![start.to_path_buf()];
    };

    let mut dirs = Vec::new();
    let mut current = Some(start);
    while let Some(dir) = current {
        dirs.push(dir.to_path_buf());
        if dir == root {
            break;
        }
        current = dir.parent();
    }
    dirs
}

/// Map a config directory family index to a SkillSource for project level.
fn project_source_for_family(idx: usize) -> SkillSource {
    match idx {
        0 => SkillSource::Project,
        1 => SkillSource::ClaudeEco,
        2 => SkillSource::Agents,
        _ => SkillSource::Project,
    }
}

/// Map a config directory family index to a SkillSource for user level.
fn user_source_for_family(idx: usize) -> SkillSource {
    match idx {
        0 => SkillSource::User,
        1 => SkillSource::ClaudeEco,
        2 => SkillSource::Agents,
        _ => SkillSource::User,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn create_skill_md(dir: &Path, name: &str, description: &str) {
        fs::create_dir_all(dir).unwrap();
        fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {description}\n---\nbody\n"),
        )
        .unwrap();
    }

    #[test]
    fn test_discover_finds_skill() {
        let tmp = std::env::temp_dir().join(format!("skill-test-{}", std::process::id()));
        let skill_dir = tmp.join(".houyicoder").join("skills").join("my-skill");
        create_skill_md(&skill_dir, "my-skill", "Does a thing");

        let found = discover_skills(Some(&tmp), None);
        assert!(found.iter().any(|s| s.name == "my-skill"));

        drop(fs::remove_dir_all(&tmp));
    }

    #[test]
    fn test_discover_skips_pruned_dirs() {
        let tmp = std::env::temp_dir().join(format!("skill-prune-{}", std::process::id()));
        let git_skill = tmp
            .join(".houyicoder")
            .join("skills")
            .join(".git")
            .join("evil");
        create_skill_md(&git_skill, "evil", "should not load");
        let node_skill = tmp
            .join(".houyicoder")
            .join("skills")
            .join("node_modules")
            .join("evil2");
        create_skill_md(&node_skill, "evil2", "should not load");

        let found = discover_skills(Some(&tmp), None);
        assert!(
            !found.iter().any(|s| s.name == "evil"),
            ".git must be pruned"
        );
        assert!(
            !found.iter().any(|s| s.name == "evil2"),
            "node_modules must be pruned"
        );

        drop(fs::remove_dir_all(&tmp));
    }

    #[test]
    fn test_shadowing_precedence_wins() {
        let tmp = std::env::temp_dir().join(format!("skill-shadow-{}", std::process::id()));
        let proj = tmp.join(".houyicoder").join("skills").join("shared");
        create_skill_md(&proj, "shared", "from project");
        let found = discover_skills(Some(&tmp), None);
        let shared = found.iter().find(|s| s.name == "shared");
        assert!(shared.is_some());
        assert_eq!(shared.unwrap().description, "from project");

        drop(fs::remove_dir_all(&tmp));
    }

    #[test]
    fn test_walk_finds_nested() {
        let tmp = std::env::temp_dir().join(format!("skill-nested-{}", std::process::id()));
        let nested = tmp.join("a").join("b").join("c");
        create_skill_md(&nested, "nested", "deep");

        let mut paths = Vec::new();
        walk_for_skill_md(&tmp, &mut paths, 0);
        assert_eq!(paths.len(), 1);

        drop(fs::remove_dir_all(&tmp));
    }

    #[test]
    fn test_walk_depth_limit() {
        let tmp = std::env::temp_dir().join(format!("skill-depth-{}", std::process::id()));
        let deep = tmp
            .join("a")
            .join("b")
            .join("c")
            .join("d")
            .join("e")
            .join("f")
            .join("g");
        create_skill_md(&deep, "too-deep", "should not be found");

        let mut paths = Vec::new();
        walk_for_skill_md(&tmp, &mut paths, 0);
        assert!(
            paths.is_empty(),
            "deeper than MAX_WALK_DEPTH must not be found"
        );

        drop(fs::remove_dir_all(&tmp));
    }

    /// The three config-dir families are all scanned: a skill under
    /// .claude/skills (ecosystem-compat, zero-migration reuse) and one
    /// under .agents/skills (spec interop convention) are both discovered,
    /// not just the native config dir.
    #[test]
    fn test_discover_scans_all_families() {
        let tmp = std::env::temp_dir().join(format!("skill-families-{}", std::process::id()));
        create_skill_md(
            &tmp.join(".houyicoder").join("skills").join("native-skill"),
            "native-skill",
            "native",
        );
        create_skill_md(
            &tmp.join(".claude").join("skills").join("claude-skill"),
            "claude-skill",
            "claude eco",
        );
        create_skill_md(
            &tmp.join(".agents").join("skills").join("agents-skill"),
            "agents-skill",
            "spec",
        );

        let found = discover_skills(Some(&tmp), None);
        let names: Vec<&str> = found.iter().map(|s| s.name.as_str()).collect();
        assert!(
            names.contains(&"native-skill"),
            "native family scanned: {names:?}"
        );
        assert!(
            names.contains(&"claude-skill"),
            ".claude family scanned: {names:?}"
        );
        assert!(
            names.contains(&"agents-skill"),
            ".agents family scanned: {names:?}"
        );

        drop(fs::remove_dir_all(&tmp));
    }
}
