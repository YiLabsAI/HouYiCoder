//! Skill resource manifest: files alongside the body in the skill
//! directory (reference notes, scripts, templates), listed by name +
//! path without loading content. Only machine-local or workspace-
//! trusted sources surface a manifest; shared-repo + remote sources
//! are excluded (a crafted file set must not reach the model unheard).

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::definition::{SkillFamily, SkillProvenance, SkillSource};
use crate::discover::{MAX_WALK_DEPTH, PRUNED_DIRS};
use std::fs::canonicalize;
use std::fs::read_dir;

/// Cap on manifest entries so a skill dir with thousands of data files does
/// not flood the body with a listing the model cannot use.
const MAX_RESOURCE_ENTRIES: usize = 200;

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
    source.family == SkillFamily::Houyi
        && matches!(
            source.provenance,
            SkillProvenance::Managed | SkillProvenance::UserHome | SkillProvenance::Project { .. }
        )
}

/// Scan a skill directory for resource files. Returns one entry per file
/// except SKILL.md (the body, already loaded) and hidden files (editor +
/// VCS artifacts). Sorted by relative path for stable output. A missing
/// or unreadable directory yields an empty list — a manifest failure
/// must not break the body it accompanies.
pub fn list_resources(skill_dir: &Path) -> Vec<ResourceEntry> {
    let mut out = Vec::new();
    let root_canon = canonicalize(skill_dir).unwrap_or_else(|_| skill_dir.to_path_buf());
    let mut seen: HashSet<PathBuf> = HashSet::new();
    // Pre-insert the root so a symlink back to the root does not re-visit it
    // and duplicate every file under a different rel_path.
    seen.insert(root_canon.clone());
    walk(skill_dir, &root_canon, skill_dir, 0, &mut seen, &mut out);
    out.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    out
}

/// Recursive directory walk with depth + entry caps, canonical-path dedup,
/// and symlink-escape containment. A directory whose canonical path is NOT
/// inside the root's canonical subtree is skipped — a symlink pointing
/// outside the skill dir would otherwise list external files (info leak +
/// budget exhaustion). Hidden segments (dot-prefixed) are pruned so VCS
/// metadata + editor artifacts do not clutter the manifest. PRUNED_DIRS
/// (node_modules, target, etc.) are skipped defensively.
fn walk(
    root: &Path,
    root_canon: &Path,
    dir: &Path,
    depth: usize,
    seen: &mut HashSet<PathBuf>,
    out: &mut Vec<ResourceEntry>,
) {
    if depth > MAX_WALK_DEPTH || out.len() >= MAX_RESOURCE_ENTRIES {
        return;
    }
    let Ok(entries) = read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if out.len() >= MAX_RESOURCE_ENTRIES {
            break;
        }
        let path = entry.path();
        let Ok(rel) = path.strip_prefix(root) else {
            continue;
        };
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        if rel_str.split('/').any(|seg| seg.starts_with('.')) {
            continue;
        }
        if path.is_dir() {
            if let Some(name_str) = entry.file_name().to_str()
                && PRUNED_DIRS.contains(&name_str)
            {
                continue;
            }
            let canon = canonicalize(&path).unwrap_or_else(|_| path.clone());
            // Contain escapes: a symlink pointing outside the skill dir
            // would list external files. Only follow dirs inside the root.
            if !canon.starts_with(root_canon) {
                continue;
            }
            if !seen.insert(canon) {
                continue;
            }
            walk(root, root_canon, &path, depth + 1, seen, out);
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == "SKILL.md" {
            continue;
        }
        if rel_str.chars().any(|c| c.is_control()) {
            continue;
        }
        let kind = classify(&rel_str, &name);
        out.push(ResourceEntry {
            name,
            rel_path: rel_str,
            kind,
        });
    }
}

/// Infer a file's role from its location + name. Files under a scripts
/// directory, or with a script extension anywhere, are Scripts; files
/// under templates are Templates; markdown is Reference; the rest is Data.
pub(crate) fn classify(rel: &str, name: &str) -> ResourceKind {
    let lower = name.to_ascii_lowercase();
    let first_seg = rel.split('/').next().unwrap_or("");
    let under_scripts = first_seg == "scripts" && rel.contains('/');
    if under_scripts || is_script_ext(&lower) {
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
mod tests;
