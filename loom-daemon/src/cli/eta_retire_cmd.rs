//! `loom-daemon eta retire` (#10525): retirement **proposals** from the
//! nightly folds. Prints them; with `--file`, files each new one as an issue
//! through `.loom/scripts/create-issue.sh`. Nothing is ever unregistered:
//! retiring a heuristic stays a code change (#10484).
//!
//! The rule and its evidence are [`loom_daemon::eta::shadow_lifecycle`]; this
//! file parses arguments, reads the registry and config, and talks to the
//! forge.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Result};
use chrono::Utc;

use loom_daemon::eta::shadow_lifecycle::{self as lifecycle, ProposalForge};
use loom_daemon::eta::{Kind, Registry, Tier};

#[derive(clap::Args)]
pub(crate) struct EtaRetireArgs {
    /// The Loom workspace whose folds and config to read. Defaults to the
    /// current directory.
    #[arg(long, value_name = "PATH")]
    pub repo_root: Option<PathBuf>,

    /// File each proposal not already filed (on this host or on the forge)
    /// as an issue. Without it this only prints.
    #[arg(long)]
    pub file: bool,

    /// Print the proposals as JSON.
    #[arg(long)]
    pub json: bool,
}

impl EtaRetireArgs {
    pub(crate) fn run(self) -> Result<()> {
        let root = super::eta_fleet_cmd::resolve_root(self.repo_root.clone());
        let registry = Registry::builtin();
        let config = loom_daemon::eta::config::read(&root);
        registry.check_budget(config.shadow_max_active)?;
        // The nightly folds score `land` only (#10492).
        let kind = Kind::Land;
        let current = registry.current(kind, config.current(kind)).id();
        let eligible: Vec<&str> = registry
            .for_kind(kind)
            .map(|h| h.id())
            .filter(|id| *id != current && registry.tier_of(id) == Some(Tier::Candidate))
            .collect();
        let days = lifecycle::load_days(&root, lifecycle::RETIREMENT_WINDOW_DAYS);
        let proposals = lifecycle::retirement_proposals(&days, current, &eligible);

        if self.json {
            println!("{}", serde_json::to_string_pretty(&proposals)?);
        } else {
            println!(
                "eta retire: {} proposal(s) from {} fold day(s) against {current}",
                proposals.len(),
                days.len()
            );
            for p in &proposals {
                println!(
                    "  {} (dominated by {}): paired pinball {:+.1}s, 95% CI {:+.1}s..{:+.1}s \
                     over {} day(s); evidence {}",
                    p.heuristic,
                    p.dominated_by,
                    p.mean_delta_pinball4_sec,
                    p.delta_ci95.0,
                    p.delta_ci95.1,
                    p.decided_days,
                    p.evidence_id
                );
            }
        }
        if !self.file || proposals.is_empty() {
            return Ok(());
        }
        let Some(script) = issue_script(&root) else {
            bail!("no executable .loom/scripts/create-issue.sh under {}", root.display());
        };
        // #9548: file only to a repository this installation manages and may
        // write; the verdict also names it, so the dedup search reads the
        // same repository the filing writes.
        let slug = match loom_daemon::write_scope::may_write_from(&root, None) {
            loom_daemon::write_scope::Verdict::Allow(nwo) => nwo,
            loom_daemon::write_scope::Verdict::Deny(why) => {
                bail!("refusing to file retirement proposals (#9548): {why}")
            }
        };
        let mut forge = ScriptForge {
            root: &root,
            script,
            slug,
        };
        let report = lifecycle::file_proposals(&root, &proposals, Utc::now(), &mut forge)
            .map_err(anyhow::Error::msg)?;
        eprintln!("eta retire: filed {:?}; already filed {:?}", report.filed, report.already);
        Ok(())
    }
}

fn issue_script(root: &Path) -> Option<PathBuf> {
    // The installed copy first, as `watchdog::escalate::resolve_issue_script`.
    [
        ".loom/scripts/create-issue.sh",
        "defaults/scripts/create-issue.sh",
    ]
    .into_iter()
    .map(|rel| root.join(rel))
    .find(|p| p.is_file())
}

/// The real forge: a REST search for the marker, `create-issue.sh` to file.
struct ScriptForge<'a> {
    root: &'a Path,
    script: PathBuf,
    slug: String,
}

impl ProposalForge for ScriptForge<'_> {
    fn find(&mut self, key: &str) -> Result<Option<String>, String> {
        let q = format!("\"{key}\" repo:{} is:issue in:body", self.slug);
        let out = Command::new(loom_daemon::write_scope::default_gh())
            .current_dir(self.root)
            .args(["api", "-X", "GET", "search/issues", "-f"])
            .arg(format!("q={q}"))
            .args(["--jq", ".items[0].html_url // empty"])
            .output()
            .map_err(|e| e.to_string())?;
        if !out.status.success() {
            return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
        }
        let url = String::from_utf8_lossy(&out.stdout).trim().to_string();
        Ok((!url.is_empty()).then_some(url))
    }

    fn file(&mut self, title: &str, body: &str) -> Result<String, String> {
        // `--repo`: the write names the vetted repository explicitly (#9548).
        // `--force`: the dedup is ours (the marker key), not the script's
        // title-similarity check.
        let out = Command::new(&self.script)
            .current_dir(self.root)
            .args([
                "--title",
                title,
                "--body",
                body,
                "--label",
                "loom:triage",
                "--repo",
                &self.slug,
                "--force",
            ])
            .output()
            .map_err(|e| e.to_string())?;
        if !out.status.success() {
            return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }
}
