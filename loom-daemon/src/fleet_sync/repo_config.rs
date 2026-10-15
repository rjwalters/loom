//! The per-repo fleet tier half of a fleet-sync pass (#11738).
//!
//! The store's compiled `fleet.json` may carry a `config` object on each
//! `repos[]` record: that repo's resolved per-repo config, rendered by the
//! fleet config repo. This module materializes it, for every **desired** repo
//! whose clone exists, as `<repo>/.loom-local/fleet-repo.json`
//! ([`crate::config_resolver::FLEET_REPO_CONFIG_REL`]), the file the resolver
//! merges above the repo's committed config and below `.loom-local/local.json`.
//!
//! It runs inside the roster half of [`super::run_pass`], on the same
//! fail-closed snapshot the roster was planned from: the repo set is a firewall
//! input (a `firewall: true` record is never desired), so it is never taken
//! from a cached snapshot the forge did not confirm. When the roster is skipped
//! or fails, so is this.
//!
//! Writes follow the machine and host-local tiers exactly: drift is semantic,
//! a write keeps one timestamped backup, and nothing is written unless the
//! pass's [`Mode`] is [`Mode::Write`] (the startup pass, or a timer pass with
//! `fleet.autoApply`). Two deliberate differences:
//!
//! - **No record config ⇒ the file is removed** (kept as the backup). The file
//!   is owned by the fleet sync, so taking a value out of the fleet config must
//!   take effect rather than leave the last rendered value behind.
//! - **No lossy-reduction guard.** That guard exists because the machine-tier
//!   file once carried hand-written blocks the store never had (2am#1653). This
//!   file has no other writer, so a key the store dropped is a key it means to
//!   drop.

use std::path::{Path, PathBuf};

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{describe_drift, Mode};
use crate::fleet_store::fetch::Snapshot;
use crate::fleet_store::render::{self, Drift, RepoTarget};

/// What one pass found for one repo's per-repo fleet tier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RepoTierReport {
    /// The roster record's name.
    pub name: String,
    /// The repo's `.loom-local/fleet-repo.json`.
    pub path: PathBuf,
    /// The top-level keys the store sets for this repo; empty when it sets
    /// none (the file is then due for removal).
    pub keys: Vec<String>,
    /// Whether the file is on disk after this pass, i.e. whether the tier is
    /// part of the repo's effective config right now.
    pub present: bool,
    /// Whether the file differed from the store at the start of the pass.
    pub drifted: bool,
    /// Whether this pass wrote it.
    pub wrote: bool,
    /// Whether this pass removed it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub removed: bool,
    /// One line describing the drift, when there was any.
    pub detail: Option<String>,
}

/// The per-repo fleet tier across every desired, cloned repo.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RepoConfigPass {
    /// One report per repo that has a per-repo config or a file on disk;
    /// repos with neither are omitted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repos: Vec<RepoTierReport>,
    /// Why the half could not be completed, when it could not be.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// How many repos failed in this pass; [`Self::error`] carries the first
    /// message plus this count when it is more than one.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub failures: usize,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

impl RepoConfigPass {
    /// Nothing to report: no store per-repo config and no file on disk. The
    /// snapshot then omits the field, so a host whose store has no per-repo
    /// config writes the same snapshot it did before #11738.
    #[must_use]
    pub fn is_unset(&self) -> bool {
        self.repos.is_empty() && self.error.is_none()
    }

    /// Whether any repo's file differs from the store.
    #[must_use]
    pub fn drifted(&self) -> bool {
        self.repos.iter().any(|r| r.drifted)
    }
}

/// Render the per-repo fleet tier from `snapshot` (the roster's own fail-closed
/// snapshot) and apply it under `mode`. Never returns `Err`.
pub(super) fn pass(
    snapshot: &Snapshot,
    home: &Path,
    is_cloned: &dyn Fn(&Path) -> bool,
    mode: Mode,
    now: DateTime<Utc>,
) -> RepoConfigPass {
    let commit = Some(snapshot.manifest.commit.clone());
    apply(render::repo_targets(snapshot, home, is_cloned), mode, now, &commit)
}

/// [`pass`] once the targets are rendered: a seam so tests need no snapshot.
pub(super) fn apply(
    targets: Result<Vec<RepoTarget>>,
    mode: Mode,
    now: DateTime<Utc>,
    commit: &Option<String>,
) -> RepoConfigPass {
    let targets = match targets {
        Ok(t) => t,
        Err(e) => {
            return RepoConfigPass {
                error: Some(format!("{e:#}")),
                ..RepoConfigPass::default()
            }
        }
    };
    let stamp = now.format("%Y%m%dT%H%M%SZ").to_string();
    let mut out = RepoConfigPass::default();
    let mut failures: Vec<String> = Vec::new();
    for t in &targets {
        let drift = render::repo_drift(t);
        let on_disk = std::fs::symlink_metadata(&t.path).is_ok();
        if t.value.is_none() && !on_disk {
            continue; // nothing set, nothing to remove: not worth a line
        }
        let drifted = drift != Drift::InSync;
        let mut report = RepoTierReport {
            name: t.name.clone(),
            path: t.path.clone(),
            keys: top_level_keys(t.value.as_ref()),
            present: on_disk,
            drifted,
            wrote: false,
            removed: false,
            detail: describe_drift(&drift),
        };
        if mode == Mode::Write && drifted {
            failures.extend(write_one(t, &stamp, commit, &mut report));
        }
        out.repos.push(report);
    }
    // Report the first failure, and how many more there were, rather than
    // letting the last one overwrite the rest.
    out.failures = failures.len();
    out.error = failures.first().map(|first| match failures.len() {
        1 => first.clone(),
        n => format!("{first} (+{} more repo failures)", n - 1),
    });
    out
}

fn write_one(
    t: &RepoTarget,
    stamp: &str,
    commit: &Option<String>,
    report: &mut RepoTierReport,
) -> Option<String> {
    let result = match t.target() {
        Some(target) => {
            let keys = crate::config_journal::restart_keys(&target, true);
            render::write(&target, stamp).map(|(wrote, backup)| {
                crate::config_journal::record_rendered(wrote, &target, backup, commit, &keys);
                report.wrote = wrote;
                report.present = true;
            })
        }
        None => render::remove(&t.path, stamp).map(|(removed, backup)| {
            crate::config_journal::record_removed(
                removed,
                render::Tier::Repo.name(),
                &t.path,
                backup,
                commit,
            );
            report.removed = removed;
            report.present = false;
        }),
    };
    result
        .err()
        .map(|e| format!("{} for {}: {e:#}", render::Tier::Repo.name(), t.name))
}

fn top_level_keys(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_object)
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default()
}

/// The per-repo fleet tier lines of `loom-daemon status`'s `Fleet store:`
/// block. Empty when there is nothing to report.
#[must_use]
pub fn lines(p: &RepoConfigPass) -> Vec<String> {
    let tier = render::Tier::Repo.name();
    let mut lines: Vec<String> = p
        .repos
        .iter()
        .map(|r| {
            let verb = if r.removed {
                "removed"
            } else if r.wrote {
                "rendered"
            } else if r.drifted {
                "DRIFT"
            } else {
                "applied"
            };
            let keys = match r.keys.len() {
                0 => "no keys in the store".to_string(),
                1 => format!("1 key: {}", r.keys[0]),
                n => format!("{n} keys: {}", r.keys.join(", ")),
            };
            let detail = match (&r.detail, r.wrote || r.removed) {
                (Some(d), false) => format!("; {d}"),
                _ => String::new(),
            };
            format!("  {tier}: {} {verb} — {} ({keys}{detail})", r.name, r.path.display())
        })
        .collect();
    if let Some(e) = &p.error {
        lines.push(format!("  {tier}: ERROR — {e}"));
    }
    lines
}

#[cfg(test)]
#[path = "tests/repo_config.rs"]
mod tests;
