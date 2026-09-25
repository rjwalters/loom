//! Generates `.agents/skills/loom-<name>/SKILL.md` files from Loom's role
//! prompts (issue #8673, contract point 5).
//!
//! Why: `.loom/roles/<role>.md` (symlinked from `.claude/commands/loom/<role>.md`,
//! see `defaults/roles/README.md`) is Loom's single source of truth for role
//! prompts, but it is exposed to Claude Code ONLY as a `.claude/commands/loom/`
//! slash command. Codex, Kimi Code, Mistral Vibe, and Grok do not read
//! `.claude/commands/` — they natively discover skills at
//! `.agents/skills/<name>/SKILL.md`, the cross-vendor skills convention (see
//! `superset-sh/superset`'s `docs/agent-tooling.md`, whose provisioner this
//! module's ownership marker mirrors). This is the SECOND single-source
//! instruction surface alongside `generate-agents-md.sh` (which covers the
//! top-level repo guide, not the per-role skill files) — see
//! `runtime-adapters.md` §5.
//!
//! Per the shell-language policy (issue #7762, ADR-0018), this generator is a
//! native `loom-daemon` subcommand rather than a new shell script — see
//! `cli::agent_skills` for the CLI surface and
//! `defaults/scripts/generate-agent-skills.sh` for the thin stub that execs it.
//!
//! Each generated file carries a [`MARKER`] line immediately after its YAML
//! frontmatter. Install/resync tooling MUST check for that marker before
//! overwriting an installed `SKILL.md` — a file at the same path with no
//! marker (or one a consumer deliberately removed to detach a file from
//! generation) is consumer-authored and must be left alone, logged rather
//! than silently reaped.

use std::fs;
use std::path::{Path, PathBuf};

/// The line every generated `SKILL.md` carries immediately after its YAML
/// frontmatter. Install/resync tooling gates overwrites on this marker's
/// presence in the DESTINATION file — never on the source being generated.
pub const MARKER: &str = "<!-- loom-managed-skill -->";

/// One role or role sub-skill discovered under `<defaults>/roles/`.
#[derive(Debug, Clone)]
pub struct GeneratedSkill {
    /// The bare name, e.g. `"builder"` (frontmatter becomes `loom-builder`).
    pub name: String,
    /// Absolute path the generated content should be written to.
    pub out_path: PathBuf,
    /// The full generated file content (frontmatter + marker + provenance
    /// comment + the verbatim source body).
    pub content: String,
}

/// Discover role/sub-skill names from `<defaults_dir>/roles/*.md`, excluding
/// `README.md` (a docs index, not a role prompt) — the same set
/// `scripts/check-markdown-token-budget.sh` already measures as agent-facing
/// markdown, so the two surfaces never disagree on what counts. Sorted for
/// deterministic output.
pub fn discover_names(defaults_dir: &Path) -> std::io::Result<Vec<String>> {
    let roles_dir = defaults_dir.join("roles");
    let mut names = Vec::new();
    for entry in fs::read_dir(&roles_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if stem == "README" {
            continue;
        }
        names.push(stem.to_string());
    }
    names.sort();
    Ok(names)
}

/// Derive the one-line `description:` frontmatter field for a skill.
///
/// 1. If `json_sidecar` is present and parses with a non-empty `description`
///    field (the `defaults/roles/<name>.json` metadata sidecar, the same text
///    `defaults/roles/README.md`'s "Available Roles" table is drawn from),
///    use it.
/// 2. Otherwise derive it from the first paragraph of body text after the
///    file's `# ` heading, collapsed to a single line (a sub-skill file with
///    no `.json` sidecar, e.g. `builder-pr.md`).
#[must_use]
pub fn derive_description(source: &str, json_sidecar: Option<&str>) -> Option<String> {
    if let Some(json_text) = json_sidecar {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(json_text) {
            if let Some(d) = value.get("description").and_then(|d| d.as_str()) {
                let trimmed = d.trim();
                if !trimmed.is_empty() {
                    return Some(trimmed.to_string());
                }
            }
        }
    }
    first_paragraph_after_h1(source)
}

/// The first non-blank paragraph after the file's first `# ` heading,
/// collapsed to one line (internal whitespace runs squashed to single
/// spaces). Returns `None` when there is no heading or no body text follows.
fn first_paragraph_after_h1(source: &str) -> Option<String> {
    let mut lines = source.lines();
    for line in lines.by_ref() {
        if line.starts_with("# ") {
            break;
        }
    }
    let mut para: Vec<&str> = Vec::new();
    for line in lines {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            if !para.is_empty() {
                break;
            }
            continue;
        }
        para.push(trimmed);
    }
    if para.is_empty() {
        None
    } else {
        Some(para.join(" "))
    }
}

/// Escape backslashes and double quotes for a double-quoted YAML scalar.
#[must_use]
pub fn yaml_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Render the full generated `SKILL.md` content for role/sub-skill `name`.
#[must_use]
pub fn render_skill_md(name: &str, description: &str, source_content: &str) -> String {
    let esc = yaml_escape(description);
    format!(
        "---\n\
         name: loom-{name}\n\
         description: \"{esc}\"\n\
         ---\n\
         {MARKER}\n\
         <!-- GENERATED FILE — DO NOT EDIT DIRECTLY.\n\
         \x20    Produced by `loom-daemon generate-agent-skills` from\n\
         \x20    defaults/.claude/commands/loom/{name}.md (the same source Claude Code\n\
         \x20    reads as /loom:{name} via .claude/commands/loom/). This is the\n\
         \x20    cross-vendor skill-discovery surface (.agents/skills/<name>/SKILL.md)\n\
         \x20    read natively by Codex, Kimi Code, Mistral Vibe, and Grok — see\n\
         \x20    runtime-adapters.md §5. To change this file, edit the source above\n\
         \x20    and re-run the generator; CI (`loom-daemon generate-agent-skills\n\
         \x20    --check`) fails if this file is stale. -->\n\
         \n\
         {source_content}",
    )
}

/// Errors [`generate_all`] can return, distinguished so a caller can decide
/// what's actionable (a missing source is a real bug; a missing roles/ dir
/// means "not a Loom checkout").
#[derive(Debug)]
pub enum GenerateError {
    RolesDirUnreadable(String),
    MissingSource { name: String, path: PathBuf },
    MissingDescription { name: String },
}

impl std::fmt::Display for GenerateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GenerateError::RolesDirUnreadable(e) => {
                write!(f, "could not read the roles directory: {e}")
            }
            GenerateError::MissingSource { name, path } => {
                write!(f, "source not found for '{name}': {}", path.display())
            }
            GenerateError::MissingDescription { name } => {
                write!(f, "could not derive a description for '{name}'")
            }
        }
    }
}

impl std::error::Error for GenerateError {}

/// Generate every `SKILL.md` for the role/sub-skill set discovered under
/// `<defaults_dir>/roles/`. Pure — reads sources, returns rendered content;
/// callers decide whether to write, diff, or list.
pub fn generate_all(defaults_dir: &Path) -> Result<Vec<GeneratedSkill>, GenerateError> {
    let names = discover_names(defaults_dir)
        .map_err(|e| GenerateError::RolesDirUnreadable(e.to_string()))?;
    let commands_dir = defaults_dir.join(".claude").join("commands").join("loom");
    let roles_dir = defaults_dir.join("roles");
    let out_root = defaults_dir.join(".agents").join("skills");

    let mut out = Vec::with_capacity(names.len());
    for name in names {
        let src_path = commands_dir.join(format!("{name}.md"));
        let source_content =
            fs::read_to_string(&src_path).map_err(|_| GenerateError::MissingSource {
                name: name.clone(),
                path: src_path.clone(),
            })?;
        let json_path = roles_dir.join(format!("{name}.json"));
        let json_text = fs::read_to_string(&json_path).ok();
        let description = derive_description(&source_content, json_text.as_deref())
            .ok_or_else(|| GenerateError::MissingDescription { name: name.clone() })?;
        let content = render_skill_md(&name, &description, &source_content);
        let out_path = out_root.join(format!("loom-{name}")).join("SKILL.md");
        out.push(GeneratedSkill {
            name,
            out_path,
            content,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, content).unwrap();
    }

    #[test]
    fn description_prefers_json_sidecar() {
        let json = r#"{"name":"Builder","description":"Implements loom:issue work"}"#;
        let source = "# Development Worker\n\nSome other prose.\n";
        assert_eq!(
            derive_description(source, Some(json)).as_deref(),
            Some("Implements loom:issue work")
        );
    }

    #[test]
    fn description_falls_back_to_first_paragraph() {
        let source = "# Hermit Patterns Reference\n\nThis file contains detailed patterns.\nSecond line of the same paragraph.\n\nA second paragraph that must not be included.\n";
        assert_eq!(
            derive_description(source, None).as_deref(),
            Some("This file contains detailed patterns. Second line of the same paragraph.")
        );
    }

    #[test]
    fn description_falls_back_when_json_has_no_description_field() {
        let json = r#"{"name":"Builder"}"#;
        let source = "# Heading\n\nBody paragraph.\n";
        assert_eq!(derive_description(source, Some(json)).as_deref(), Some("Body paragraph."));
    }

    #[test]
    fn description_none_when_no_body_follows_heading() {
        let source = "# Heading\n";
        assert_eq!(derive_description(source, None), None);
    }

    #[test]
    fn yaml_escape_handles_quotes_and_backslashes() {
        assert_eq!(yaml_escape(r#"a "quoted" \path"#), r#"a \"quoted\" \\path"#);
    }

    #[test]
    fn render_skill_md_carries_marker_and_frontmatter() {
        let rendered =
            render_skill_md("builder", "Implements issues", "# Development Worker\n\nBody.\n");
        assert!(rendered.starts_with("---\nname: loom-builder\n"));
        assert!(rendered.contains("description: \"Implements issues\"\n"));
        assert!(rendered.contains(MARKER));
        assert!(rendered.contains("# Development Worker\n\nBody.\n"));
        // Marker must appear immediately after the closing frontmatter `---`.
        let marker_idx = rendered.find(MARKER).unwrap();
        let second_delim_idx = rendered.match_indices("---\n").nth(1).unwrap().0;
        let between = &rendered[second_delim_idx + 4..marker_idx];
        assert!(between.trim().is_empty());
    }

    #[test]
    fn generate_all_discovers_and_renders_every_role() {
        let tmp = tempfile::tempdir().unwrap();
        let defaults = tmp.path();
        write(&defaults.join("roles/README.md"), "# Loom Role Definitions\n");
        write(&defaults.join("roles/builder.md"), "symlink-placeholder");
        write(&defaults.join("roles/builder.json"), r#"{"description":"Implements issues"}"#);
        write(
            &defaults.join(".claude/commands/loom/builder.md"),
            "# Development Worker\n\nYou are a skilled software engineer.\n",
        );
        write(&defaults.join("roles/probe-protocol.md"), "symlink-placeholder");
        write(
            &defaults.join(".claude/commands/loom/probe-protocol.md"),
            "# Terminal Probe Protocol\n\nRespond to probes.\n",
        );

        let skills = generate_all(defaults).unwrap();
        let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["builder", "probe-protocol"]);

        let builder = skills.iter().find(|s| s.name == "builder").unwrap();
        assert!(builder.content.contains("name: loom-builder"));
        assert!(builder
            .content
            .contains("description: \"Implements issues\""));
        assert_eq!(builder.out_path, defaults.join(".agents/skills/loom-builder/SKILL.md"));

        let probe = skills.iter().find(|s| s.name == "probe-protocol").unwrap();
        assert!(probe
            .content
            .contains("description: \"Respond to probes.\""));
    }

    #[test]
    fn generate_all_errors_on_missing_source() {
        let tmp = tempfile::tempdir().unwrap();
        let defaults = tmp.path();
        write(&defaults.join("roles/ghost.md"), "symlink-placeholder");
        let err = generate_all(defaults).unwrap_err();
        assert!(matches!(err, GenerateError::MissingSource { .. }));
    }

    #[test]
    fn generate_all_excludes_readme() {
        let tmp = tempfile::tempdir().unwrap();
        let defaults = tmp.path();
        write(&defaults.join("roles/README.md"), "# index\n");
        let skills = generate_all(defaults).unwrap();
        assert!(skills.is_empty());
    }
}
