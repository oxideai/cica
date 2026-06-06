//! Skills discovery and management.
//!
//! Skills are stored in the skills/ directory as subdirectories containing a SKILL.md file.
//! The SKILL.md file contains YAML frontmatter with name and description.

use anyhow::Result;
use std::path::{Path, PathBuf};

use crate::config;
use crate::setup;

/// A discovered skill
#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub category: String,
    pub when_to_use: String,
    pub location: PathBuf,
}

pub fn discover_skills() -> Result<Vec<Skill>> {
    let skills_dir = config::paths()?.skills_dir;

    if !skills_dir.exists() {
        return Ok(Vec::new());
    }

    let prep_deps = config::prep_skill_deps_locally(
        config::Config::load()
            .map(|c| c.deployment.provider)
            .unwrap_or(None),
    );

    let mut skills = Vec::new();
    collect_skills(&skills_dir, prep_deps, &mut skills)?;
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(skills)
}

/// Directory names never descended into during discovery (vendored deps, VCS,
/// docs, and any hidden dir).
fn is_excluded_dir(name: &str) -> bool {
    name.starts_with('.') || matches!(name, "node_modules" | "docs")
}

/// Recursively collect skills. A directory containing a `SKILL.md` is a leaf
/// entry (a skill or knowledge doc) and is not descended into further.
fn collect_skills(dir: &Path, prep_deps: bool, out: &mut Vec<Skill>) -> Result<()> {
    let skill_file = dir.join("SKILL.md");
    if skill_file.exists() {
        if let Ok(skill) = parse_skill(&skill_file) {
            if prep_deps {
                setup::ensure_skill_deps(dir);
            }
            out.push(skill);
        }
        return Ok(());
    }

    for entry in std::fs::read_dir(dir)?.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = entry.file_name();
        if is_excluded_dir(&name.to_string_lossy()) {
            continue;
        }
        collect_skills(&path, prep_deps, out)?;
    }
    Ok(())
}

fn parse_frontmatter(
    frontmatter: &str,
    name: &mut Option<String>,
    description: &mut Option<String>,
    category: &mut Option<String>,
    when_to_use: &mut Option<String>,
) {
    for line in frontmatter.lines() {
        let line = line.trim();
        if let Some(value) = line.strip_prefix("name:") {
            *name = Some(
                value
                    .trim()
                    .trim_matches('"')
                    .trim_matches('\'')
                    .to_string(),
            );
        } else if let Some(value) = line.strip_prefix("description:") {
            *description = Some(
                value
                    .trim()
                    .trim_matches('"')
                    .trim_matches('\'')
                    .to_string(),
            );
        } else if let Some(value) = line.strip_prefix("category:") {
            *category = Some(
                value
                    .trim()
                    .trim_matches('"')
                    .trim_matches('\'')
                    .to_string(),
            );
        } else if let Some(value) = line.strip_prefix("when_to_use:") {
            *when_to_use = Some(
                value
                    .trim()
                    .trim_matches('"')
                    .trim_matches('\'')
                    .to_string(),
            );
        }
    }
}

fn parse_skill(path: &PathBuf) -> Result<Skill> {
    let content = std::fs::read_to_string(path)?;

    let mut name = None;
    let mut description = None;
    let mut category = None;
    let mut when_to_use = None;

    if let Some(stripped) = content.strip_prefix("---")
        && let Some(end) = stripped.find("---")
    {
        let frontmatter = &stripped[..end];
        parse_frontmatter(
            frontmatter,
            &mut name,
            &mut description,
            &mut category,
            &mut when_to_use,
        );
    }

    let dir_name = path
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string();

    Ok(Skill {
        name: name.unwrap_or_else(|| dir_name.clone()),
        description: description.unwrap_or_else(|| format!("Skill: {}", dir_name)),
        category: category
            .filter(|c| !c.is_empty())
            .unwrap_or_else(|| "tool".to_string()),
        when_to_use: when_to_use.unwrap_or_default(),
        location: path.clone(),
    })
}

/// Format skills as XML for the system prompt. Locations are emitted relative
/// to `workspace` (the agent's cwd) so they resolve on whichever host runs the
/// agent (router or an ephemeral worker), falling back to the absolute path if
/// a skill lives outside the workspace. Skills are grouped by category in the
/// fixed order: tool, workflow, report, then any remaining categories sorted.
pub fn format_skills_xml(skills: &[Skill], workspace: &Path) -> String {
    if skills.is_empty() {
        return String::new();
    }

    // Fixed category order; any unknown categories follow, sorted.
    let mut categories: Vec<&str> = vec!["tool", "workflow", "report", "knowledge"];
    let mut extras: Vec<&str> = skills
        .iter()
        .map(|s| s.category.as_str())
        .filter(|c| !categories.contains(c))
        .collect();
    extras.sort_unstable();
    extras.dedup();
    categories.extend(extras);

    let mut xml = String::from("<available_skills>\n");
    for cat in categories {
        let in_cat: Vec<&Skill> = skills.iter().filter(|s| s.category == cat).collect();
        if in_cat.is_empty() {
            continue;
        }
        xml.push_str(&format!(
            "  <skill_group category=\"{}\">\n",
            escape_xml(cat)
        ));
        for skill in in_cat {
            xml.push_str("    <skill>\n");
            xml.push_str(&format!("      <name>{}</name>\n", escape_xml(&skill.name)));
            xml.push_str(&format!(
                "      <description>{}</description>\n",
                escape_xml(&skill.description)
            ));
            if !skill.when_to_use.is_empty() {
                xml.push_str(&format!(
                    "      <when_to_use>{}</when_to_use>\n",
                    escape_xml(&skill.when_to_use)
                ));
            }
            let location = skill
                .location
                .strip_prefix(workspace)
                .unwrap_or(&skill.location);
            xml.push_str(&format!(
                "      <location>{}</location>\n",
                location.display()
            ));
            xml.push_str("    </skill>\n");
        }
        xml.push_str("  </skill_group>\n");
    }
    xml.push_str("</available_skills>");
    xml
}

fn escape_xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_escape_xml() {
        assert_eq!(escape_xml("hello"), "hello");
        assert_eq!(escape_xml("a < b"), "a &lt; b");
        assert_eq!(escape_xml("a & b"), "a &amp; b");
    }

    #[test]
    fn format_skills_xml_emits_workspace_relative_location() {
        use std::path::PathBuf;
        let base = PathBuf::from("/data/cica");
        let skills = vec![Skill {
            name: "foo".to_string(),
            description: "does foo".to_string(),
            category: "tool".to_string(),
            when_to_use: String::new(),
            location: PathBuf::from("/data/cica/skills/foo/SKILL.md"),
        }];
        let xml = format_skills_xml(&skills, &base);
        assert!(
            xml.contains("<location>skills/foo/SKILL.md</location>"),
            "got: {xml}"
        );
        // The absolute path must NOT appear (would break on workers with a different base).
        assert!(!xml.contains("/data/cica/skills/foo/SKILL.md"));
        assert!(
            !xml.contains("<when_to_use>"),
            "empty when_to_use must not be emitted; got: {xml}"
        );
    }

    #[test]
    fn parse_skill_reads_category_and_when_to_use() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("demo");
        std::fs::create_dir_all(&skill_dir).unwrap();
        let md = "---\nname: demo\ncategory: workflow\ndescription: does demo\nwhen_to_use: when you demo\n---\n# Demo\n";
        std::fs::write(skill_dir.join("SKILL.md"), md).unwrap();
        let skill = parse_skill(&skill_dir.join("SKILL.md")).unwrap();
        assert_eq!(skill.category, "workflow");
        assert_eq!(skill.when_to_use, "when you demo");
    }

    #[test]
    fn parse_skill_defaults_category_to_tool() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("legacy");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: legacy\ndescription: old\n---\n",
        )
        .unwrap();
        let skill = parse_skill(&skill_dir.join("SKILL.md")).unwrap();
        assert_eq!(skill.category, "tool");
        assert_eq!(skill.when_to_use, "");
    }

    #[test]
    fn collect_skills_finds_nested_and_excludes_vendor_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let write_skill = |rel: &str, name: &str| {
            let d = root.join(rel);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(
                d.join("SKILL.md"),
                format!("---\nname: {name}\ncategory: tool\ndescription: d\nwhen_to_use: w\n---\n"),
            )
            .unwrap();
        };
        write_skill("root-db-query", "root-db-query");
        write_skill("knowledge/data-model", "data-model");
        write_skill("root-db-query/node_modules/pkg", "pkg");
        write_skill("docs/superpowers", "superpowers");

        let mut out = Vec::new();
        collect_skills(root, false, &mut out).unwrap();
        let mut names: Vec<&str> = out.iter().map(|s| s.name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, vec!["data-model", "root-db-query"]);
    }

    #[test]
    fn format_skills_xml_groups_knowledge_after_skills() {
        use std::path::PathBuf;
        let base = PathBuf::from("/ws");
        let skills = vec![
            Skill {
                name: "root-db-query".into(),
                description: "db".into(),
                category: "tool".into(),
                when_to_use: "data".into(),
                location: PathBuf::from("/ws/root-db-query/SKILL.md"),
            },
            Skill {
                name: "data-model".into(),
                description: "schema".into(),
                category: "knowledge".into(),
                when_to_use: "before SQL".into(),
                location: PathBuf::from("/ws/knowledge/data-model/SKILL.md"),
            },
        ];
        let xml = format_skills_xml(&skills, &base);
        assert!(xml.contains("category=\"knowledge\""), "got: {xml}");
        let tool = xml.find("category=\"tool\"").unwrap();
        let know = xml.find("category=\"knowledge\"").unwrap();
        assert!(tool < know, "knowledge should group after tool: {xml}");
        assert!(
            xml.contains("<location>knowledge/data-model/SKILL.md</location>"),
            "got: {xml}"
        );
    }

    #[test]
    fn format_skills_xml_groups_by_category_and_emits_when_to_use() {
        use std::path::PathBuf;
        let base = PathBuf::from("/ws");
        let skills = vec![
            Skill {
                name: "alpha".into(),
                description: "a tool".into(),
                category: "tool".into(),
                when_to_use: "use alpha for X".into(),
                location: PathBuf::from("/ws/alpha/SKILL.md"),
            },
            Skill {
                name: "beta".into(),
                description: "a workflow".into(),
                category: "workflow".into(),
                when_to_use: "use beta for Y".into(),
                location: PathBuf::from("/ws/beta/SKILL.md"),
            },
        ];
        let xml = format_skills_xml(&skills, &base);
        assert!(xml.contains("category=\"tool\""), "got: {xml}");
        assert!(xml.contains("category=\"workflow\""), "got: {xml}");
        assert!(
            xml.contains("<when_to_use>use alpha for X</when_to_use>"),
            "got: {xml}"
        );
        let t = xml.find("category=\"tool\"").unwrap();
        let w = xml.find("category=\"workflow\"").unwrap();
        assert!(t < w, "tool group should precede workflow group: {xml}");
        assert!(
            xml.contains("<when_to_use>use beta for Y</when_to_use>"),
            "got: {xml}"
        );
        assert!(
            !xml.contains("category=\"report\""),
            "no report skills -> no report group; got: {xml}"
        );
    }
}
