//! `loom-daemon generate-agent-skills` — the CLI surface over
//! [`loom_daemon::agent_skills`] (issue #8673, contract point 5).
//!
//! Backs `defaults/scripts/generate-agent-skills.sh`, a Shape-A stub per the
//! shell-language policy (issue #7762, ADR-0018): this subcommand is brand
//! new logic, so it is native Rust from the start rather than a script that
//! would later need porting.

use anyhow::{Context, Result};
use loom_daemon::agent_skills;
use loom_daemon::repo_root;
use std::fs;
use std::path::PathBuf;

#[derive(clap::Args)]
pub(crate) struct AgentSkillsArgs {
    /// Verify every generated file is up to date; write nothing. Exit 1 (via
    /// an error) when any file is missing or stale.
    #[arg(long, conflicts_with = "list")]
    pub check: bool,

    /// Print "loom-<name> -> <path>" for every discovered role/sub-skill;
    /// write nothing.
    #[arg(long, conflicts_with = "check")]
    pub list: bool,

    /// The Loom `defaults/` directory to read from and write into. Defaults
    /// to `<worktree root>/defaults` (the worktree containing the current
    /// directory — issue #8499's "which tree am I reasoning about" resolver,
    /// so a Builder's own worktree content is used, not the main checkout's).
    #[arg(long, value_name = "PATH")]
    pub defaults: Option<PathBuf>,
}

impl AgentSkillsArgs {
    pub(crate) fn run(self) -> Result<()> {
        let defaults_dir = match self.defaults {
            Some(p) => p,
            None => {
                let root = repo_root::find_worktree_root_from_cwd().context(
                    "not inside a Loom checkout (no ancestor with both .git and .loom/) — pass --defaults explicitly",
                )?;
                root.join("defaults")
            }
        };

        let skills = agent_skills::generate_all(&defaults_dir).map_err(|e| anyhow::anyhow!(e))?;

        if self.list {
            for skill in &skills {
                println!("loom-{} -> {}", skill.name, skill.out_path.display());
            }
            return Ok(());
        }

        if self.check {
            let mut stale_paths = Vec::new();
            for skill in &skills {
                match fs::read_to_string(&skill.out_path) {
                    Ok(existing) if existing == skill.content => {}
                    Ok(_) => stale_paths.push(("STALE", skill.out_path.clone())),
                    Err(_) => stale_paths.push(("MISSING", skill.out_path.clone())),
                }
            }
            if stale_paths.is_empty() {
                println!(
                    "generate-agent-skills: OK — defaults/.agents/skills/loom-*/SKILL.md is in sync with defaults/roles/."
                );
                return Ok(());
            }
            for (verb, path) in &stale_paths {
                println!("{verb}: {}", path.display());
            }
            anyhow::bail!(
                "{} generated .agents/skills/loom-<name>/SKILL.md file(s) stale or missing — run `loom-daemon generate-agent-skills` and commit the result",
                stale_paths.len()
            );
        }

        for skill in &skills {
            if let Some(parent) = skill.out_path.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("failed to create {}", parent.display()))?;
            }
            fs::write(&skill.out_path, &skill.content)
                .with_context(|| format!("failed to write {}", skill.out_path.display()))?;
            eprintln!("generate-agent-skills: wrote {}", skill.out_path.display());
        }
        Ok(())
    }
}
