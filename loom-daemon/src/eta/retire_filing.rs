//! Filing retirement proposals (#10525): the shared path behind
//! `loom-daemon eta retire --file` and the captain's nightly fold task
//! (`observability::eta_nightly_folds`, `nightlyFolds.retirementFiling`).
//!
//! The rule and its evidence are [`super::shadow_lifecycle`]. This module
//! reads the saved folds, resolves the candidates, gates on the folds' owner
//! (the explicit `fleet.etaAuthority`, else `fleet.captain`, fail-closed;
//! #10918) and talks to the forge. It never unregisters a heuristic:
//! retiring one stays a code change (#10484).

use std::path::{Path, PathBuf};
use std::process::Command;

use chrono::{DateTime, Utc};

use super::shadow_lifecycle::{self as lifecycle, FilingReport, ProposalForge, RetirementProposal};
use super::{Kind, Registry, Tier};
use crate::eta::job_owner::Owner;
use crate::fleet_captain::CaptainGate;

/// The singleton job name filing is gated under (`fleet.captain`).
pub const JOB_NAME: &str = "eta-retire-file";

/// The proposals the saved folds support, with what they were computed from.
#[derive(Debug, Clone)]
pub struct Proposed {
    /// The `current` `land` heuristic the folds are compared against.
    pub current: String,
    /// Fold days read.
    pub days: usize,
    /// The proposals, ordered by heuristic id.
    pub proposals: Vec<RetirementProposal>,
}

/// Read the saved nightly folds under `root` and derive the proposals.
///
/// # Errors
/// The registry is over the configured shadow budget.
pub fn proposals_for_root(root: &Path) -> Result<Proposed, String> {
    let registry = Registry::builtin();
    let config = super::config::read(root);
    registry
        .check_budget(config.shadow_max_active)
        .map_err(|e| e.to_string())?;
    // The nightly folds score `land` only (#10492).
    let kind = Kind::Land;
    let current = registry.current(kind, config.current(kind)).id();
    let eligible: Vec<&str> = registry
        .for_kind(kind)
        .map(|h| h.id())
        .filter(|id| *id != current && registry.tier_of(id) == Some(Tier::Candidate))
        .collect();
    let days = lifecycle::load_days(root, lifecycle::RETIREMENT_WINDOW_DAYS);
    let proposals = lifecycle::retirement_proposals(&days, current, &eligible);
    Ok(Proposed {
        current: current.to_string(),
        days: days.len(),
        proposals,
    })
}

/// Filing is a singleton job: refused unless this host is the declared fleet
/// captain (no captain declared also refuses).
///
/// # Errors
/// The gate is not armed.
pub fn require_captain(gate: &CaptainGate) -> Result<(), String> {
    if gate.is_armed() {
        return Ok(());
    }
    Err(format!("refusing to file retirement proposals: {}", gate.message(JOB_NAME)))
}

/// Filing follows the nightly folds (#10918): the explicit ETA authority
/// files, every other host refuses; with no explicit authority,
/// [`require_captain`].
///
/// # Errors
/// This host is not the owner.
pub fn require_owner(owner: &Owner) -> Result<(), String> {
    match owner {
        Owner::Authority => Ok(()),
        Owner::AuthorityElsewhere { authority } => Err(format!(
            "refusing to file retirement proposals: the ETA authority ({authority}, \
             fleet.etaAuthority) folds and files (#10918)"
        )),
        Owner::Captain(gate) => require_captain(gate),
    }
}

/// File `proposals` through `forge`, **after** the owner gate. Search-then-
/// create is not atomic on the forge, so only one host files (#8848).
///
/// # Errors
/// The gate refuses, or [`lifecycle::file_proposals`] fails.
pub fn file_gated(
    root: &Path,
    owner: &Owner,
    proposals: &[RetirementProposal],
    now: DateTime<Utc>,
    forge: &mut dyn ProposalForge,
) -> Result<FilingReport, String> {
    require_owner(owner)?;
    lifecycle::file_proposals(root, proposals, now, forge)
}

/// The production filing: the real forge, the write-scope check and the
/// captain gate for `host_id`. Used by the CLI and the nightly task.
///
/// # Errors
/// Any refusal ([`file_gated`], no issue script, write scope) or filing error.
pub fn file_for_root(
    root: &Path,
    host_id: &str,
    proposals: &[RetirementProposal],
) -> Result<FilingReport, String> {
    let owner = crate::eta::job_owner::resolve_for_root(root, host_id);
    // Refuse before anything else: a non-owner does not even look.
    require_owner(&owner)?;
    let Some(script) = issue_script(root) else {
        return Err(format!(
            "no executable .loom/scripts/create-issue.sh under {}",
            root.display()
        ));
    };
    // #9548: file only to a repository this installation manages and may
    // write; the verdict also names it, so the dedup search reads the same
    // repository the filing writes.
    let slug = match crate::write_scope::may_write_from(root, None) {
        crate::write_scope::Verdict::Allow(nwo) => nwo,
        crate::write_scope::Verdict::Deny(why) => {
            return Err(format!("refusing to file retirement proposals (#9548): {why}"))
        }
    };
    let mut forge = ScriptForge { root, script, slug };
    file_gated(root, &owner, proposals, Utc::now(), &mut forge)
}

/// The scheduled run (`nightlyFolds.retirementFiling`): derive the proposals
/// from the saved folds and file the new ones. Nothing to do is not an error.
///
/// # Errors
/// As [`proposals_for_root`] and [`file_for_root`].
pub fn run_scheduled(root: &Path, host_id: &str) -> Result<FilingReport, String> {
    let proposed = proposals_for_root(root)?;
    if proposed.proposals.is_empty() {
        return Ok(FilingReport::default());
    }
    file_for_root(root, host_id, &proposed.proposals)
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
        let out = Command::new(crate::write_scope::default_gh())
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
        let policy = crate::comment_trust::TrustPolicy::for_root(self.root);
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
    policy: &crate::comment_trust::TrustPolicy,
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
    use crate::comment_trust::TrustPolicy;

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
    fn an_explicit_authority_files_and_the_captain_refuses() {
        use crate::eta::job_owner::resolve;
        assert!(require_owner(&resolve(Some("w1"), Some("cap"), "w1")).is_ok());
        let err = require_owner(&resolve(Some("w1"), Some("cap"), "cap")).unwrap_err();
        assert!(err.contains("ETA authority (w1"), "{err}");
        assert!(
            require_owner(&resolve(None, Some("cap"), "cap")).is_ok(),
            "unchanged without it"
        );
        assert!(require_owner(&resolve(None, None, "cap")).is_err(), "still fail-closed");
    }

    #[test]
    fn only_the_declared_captain_files() {
        use crate::fleet_captain::resolve_gate;
        assert!(require_captain(&resolve_gate(Some("w1"), "w1")).is_ok());
        let other = require_captain(&resolve_gate(Some("w1"), "w2")).unwrap_err();
        assert!(other.contains("captain is w1"), "{other}");
        // No captain declared refuses too: never "everywhere".
        assert!(require_captain(&resolve_gate(None, "w1")).is_err());
    }
}
