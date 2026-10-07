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

/// The singleton job name `--file` is gated under (`fleet.captain`).
const JOB_NAME: &str = "eta-retire-file";

#[derive(clap::Args)]
pub(crate) struct EtaRetireArgs {
    /// The Loom workspace whose folds and config to read. Defaults to the
    /// current directory.
    #[arg(long, value_name = "PATH")]
    pub repo_root: Option<PathBuf>,

    /// File each proposal not already filed (on this host or on the forge)
    /// as an issue. Only the fleet captain files (`fleet.captain`); any other
    /// host refuses. Without it this only prints.
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
        // Search-then-create is not atomic on the forge, so two hosts could
        // both find nothing and both file. Only the fleet captain files
        // (#8848): one owner, so the race cannot occur by construction.
        require_captain(&loom_daemon::fleet_captain::resolve_gate_for_root(
            &root,
            &loom_daemon::sweep_registry::host_identity(),
        ))?;
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

/// Filing is a singleton job: refused unless this host is the declared fleet
/// captain (or no captain is declared, which also refuses).
fn require_captain(gate: &loom_daemon::fleet_captain::CaptainGate) -> Result<()> {
    if gate.is_armed() {
        return Ok(());
    }
    bail!("refusing to file retirement proposals: {}", gate.message(JOB_NAME))
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
            .args(["-f", "per_page=100"])
            .output()
            .map_err(|e| e.to_string())?;
        if !out.status.success() {
            return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
        }
        // The marker changes Loom's behaviour, so only a trusted author's
        // issue counts as "already filed" (`comment-trust.md`, #9548).
        let policy = loom_daemon::comment_trust::TrustPolicy::for_root(self.root);
        first_trusted_url(&policy, &out.stdout)
    }

    fn file(&mut self, title: &str, body: &str) -> Result<String, String> {
        // `--repo`: the write names the vetted repository explicitly (#9548).
        // `--force`: the dedup is ours (the marker key, with the captain as
        // the only filer), not the script's title-similarity check.
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

/// The `html_url` of the first search hit `policy` believes. An unparseable
/// reply is an error (the filing refuses), never "no hit".
fn first_trusted_url(
    policy: &loom_daemon::comment_trust::TrustPolicy,
    search: &[u8],
) -> Result<Option<String>, String> {
    let v: serde_json::Value =
        serde_json::from_slice(search).map_err(|e| format!("unreadable search reply: {e}"))?;
    let items = v
        .get("items")
        .and_then(serde_json::Value::as_array)
        .ok_or("search reply has no `items`")?;
    Ok(items
        .iter()
        .filter(|item| policy.trusts_json(item))
        .find_map(|item| item.get("html_url").and_then(serde_json::Value::as_str))
        .map(str::to_string))
}

#[cfg(test)]
mod tests {
    use super::*;
    use loom_daemon::comment_trust::TrustPolicy;
    use loom_daemon::fleet_captain::resolve_gate;

    fn policy() -> TrustPolicy {
        TrustPolicy::for_root(Path::new("/nonexistent-loom-root"))
    }

    fn hit(url: &str, login: &str, assoc: &str) -> serde_json::Value {
        serde_json::json!({
            "html_url": url,
            "user": {"login": login, "type": "User"},
            "author_association": assoc,
        })
    }

    #[test]
    fn an_untrusted_issue_carrying_the_marker_is_not_already_filed() {
        let reply = serde_json::json!({"items": [hit("https://x/1", "mallory", "NONE")]});
        let got = first_trusted_url(&policy(), reply.to_string().as_bytes());
        assert_eq!(got, Ok(None));
    }

    #[test]
    fn a_trusted_hit_after_an_untrusted_one_counts() {
        let reply = serde_json::json!({"items": [
            hit("https://x/1", "mallory", "NONE"),
            hit("https://x/2", "owner", "OWNER"),
        ]});
        let got = first_trusted_url(&policy(), reply.to_string().as_bytes());
        assert_eq!(got, Ok(Some("https://x/2".to_string())));
    }

    #[test]
    fn an_unreadable_reply_refuses_rather_than_reading_as_no_hit() {
        assert!(first_trusted_url(&policy(), b"not json").is_err());
        assert!(first_trusted_url(&policy(), b"{}").is_err());
    }

    #[test]
    fn only_the_declared_captain_files() {
        assert!(require_captain(&resolve_gate(Some("w1"), "w1")).is_ok());
        let other = require_captain(&resolve_gate(Some("w1"), "w2")).unwrap_err();
        assert!(other.to_string().contains("captain is w1"), "{other}");
        // No captain declared refuses too: never "everywhere".
        assert!(require_captain(&resolve_gate(None, "w1")).is_err());
    }
}
