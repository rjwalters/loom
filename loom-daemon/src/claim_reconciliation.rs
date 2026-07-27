//! Daemon-startup reconciliation of stale `loom:building` claims across every
//! managed workspace (Issue #3953, acceptance criterion 3/4).
//!
//! The persisted [`crate::sweep_journal`] gives a fresh daemon (post-restart,
//! post-rate-limit-kill, post-upgrade) an authoritative liveness source that
//! survives the in-memory [`crate::sweep_registry::SweepRegistry`] being wiped
//! clean. This module is the consumer that turns that evidence into action at
//! startup: for every registered workspace, list the open `loom:building`
//! issues and decide, per issue, whether the claim is still backed by a live
//! sweep.
//!
//! ## Decision rule
//!
//! - A journal entry recording a **live** PID ⇒ [`ReconcileAction::Keep`] —
//!   the claim is genuinely in-flight.
//! - A journal entry recording a **dead** PID ⇒
//!   [`ReconcileAction::Reclaim`] — the sweep behind this claim is provably
//!   gone.
//! - **No** journal entry at all (the claim predates this journal, or was
//!   made by a process that never wrote one) ⇒ reclaim only once the label
//!   has been stale longer than [`resolve_stale_hours`] (`updated_at` age),
//!   otherwise `Keep`. This mirrors the Python tool's label-age grace period
//!   philosophy (#3651): absence of evidence is not, by itself, proof of
//!   orphanhood — only *aged* absence is.
//! - No age evidence either (an issue whose `updatedAt` is somehow
//!   unavailable) ⇒ fail safe to `Keep`.
//!
//! The pure [`decide`] / [`plan`] functions take no forge/PID dependencies —
//! they are fully unit-testable. The `gh`/label-flip glue lives in
//! [`forge::reconcile_workspace`], which mirrors the `WorkSource`/adapter split
//! established by [`crate::work_finder::forge`] and
//! [`crate::epic_supervisor::forge`] (those adapters are likewise not
//! unit-tested directly — they are thin `Command` wrappers around `gh`).

use chrono::{DateTime, Utc};

use crate::sweep_journal::{self, JournalEntry, SweepJournal};

/// Env var to disable the startup reconciliation pass entirely (kill switch,
/// not a feature gate — this is corrective crash recovery in the same spirit
/// as the daemon's existing unconditional stale-claim release in
/// `activity::release_stale_claims` and `SweepRegistry::reconstruct`, so it
/// defaults to ON). `0`/`false`/`no`/`off` disables; anything else (including
/// unset) leaves it enabled.
pub const RECONCILE_ENABLED_ENV: &str = "LOOM_STALE_CLAIM_RECONCILE";

/// Env var overriding the no-record staleness threshold, in hours.
pub const STALE_HOURS_ENV: &str = "LOOM_STALE_BUILDING_HOURS";

/// Default no-record staleness threshold: a `loom:building` issue with no
/// journal entry at all must be untouched for this many hours before it is
/// considered abandoned. Deliberately generous — a claim made by a
/// pre-journal daemon, or a CLI-driven sweep with no daemon involvement at
/// all, should get a wide berth before automatic reclaim.
pub const DEFAULT_STALE_BUILDING_HOURS: f64 = 4.0;

/// Bound on how many `loom:building` issues one reconciliation pass inspects
/// per workspace (defense in depth against an unexpectedly huge backlog
/// turning startup into a `gh`-API storm).
pub const MAX_ISSUES_PER_WORKSPACE: u32 = 100;

/// Resolve whether the startup reconciliation pass is enabled.
#[must_use]
pub fn reconciliation_enabled() -> bool {
    match std::env::var(RECONCILE_ENABLED_ENV) {
        Ok(v) => !matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "false" | "no" | "off"),
        Err(_) => true,
    }
}

/// Resolve the no-record staleness threshold (hours) from
/// [`STALE_HOURS_ENV`], falling back to [`DEFAULT_STALE_BUILDING_HOURS`] for
/// an absent, unparseable, or non-positive value.
#[must_use]
pub fn resolve_stale_hours() -> f64 {
    std::env::var(STALE_HOURS_ENV)
        .ok()
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|v| *v > 0.0)
        .unwrap_or(DEFAULT_STALE_BUILDING_HOURS)
}

/// A `loom:building` issue reported by the forge, trimmed to the fields the
/// reconciliation decision needs.
#[derive(Debug, Clone, PartialEq)]
pub struct BuildingIssue {
    pub number: u32,
    /// Parsed `updatedAt` timestamp, when available.
    pub updated_at: Option<DateTime<Utc>>,
}

/// Why a claim is being reclaimed — carried through to the log line so an
/// operator reading the daemon log can tell a dead-process reclaim from a
/// no-record/stale-age reclaim.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ReclaimReason {
    /// The journal recorded a PID for this issue and it is no longer alive.
    DeadPid { pid: u32 },
    /// No journal record exists, and the issue's `loom:building` label has
    /// been present longer than the staleness threshold.
    NoRecordStale { age_hours: f64 },
}

/// The reconciliation decision for one issue.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ReconcileAction {
    /// No liveness concern — leave the claim alone.
    Keep,
    /// Flip `loom:building` back to `loom:issue`.
    Reclaim(ReclaimReason),
}

/// Pure decision function — no I/O, fully unit-testable. See the module docs
/// for the decision rule.
#[must_use]
pub fn decide(
    issue: &BuildingIssue,
    journal_entry: Option<&JournalEntry>,
    is_alive: &dyn Fn(u32) -> bool,
    stale_hours: f64,
    now: DateTime<Utc>,
) -> ReconcileAction {
    if let Some(entry) = journal_entry {
        return if is_alive(entry.pid) {
            ReconcileAction::Keep
        } else {
            ReconcileAction::Reclaim(ReclaimReason::DeadPid { pid: entry.pid })
        };
    }

    match issue.updated_at {
        Some(updated) => {
            let age_hours = (now - updated).num_seconds() as f64 / 3600.0;
            if age_hours >= stale_hours {
                ReconcileAction::Reclaim(ReclaimReason::NoRecordStale { age_hours })
            } else {
                ReconcileAction::Keep
            }
        }
        // No journal evidence AND no age evidence: fail safe, never reclaim
        // on a total absence of information.
        None => ReconcileAction::Keep,
    }
}

/// Plan reconciliation decisions for every `issue` in `issues`, given an
/// already-loaded `journal`. Performs no I/O.
///
/// IMPORTANT (#3975): `journal` must be the **raw, unpruned** journal.
/// Pruning dead-PID entries before calling this function would erase the
/// exact evidence the `DeadPid` branch of [`decide`] needs to fire its
/// unconditional, immediate reclaim -- see [`forge::reconcile_workspace`]
/// for the caller that got this wrong before #3975.
#[must_use]
pub fn plan(
    repo: &str,
    issues: &[BuildingIssue],
    journal: &SweepJournal,
    is_alive: &dyn Fn(u32) -> bool,
    stale_hours: f64,
    now: DateTime<Utc>,
) -> Vec<(u32, ReconcileAction)> {
    issues
        .iter()
        .map(|issue| {
            let entry = sweep_journal::find(journal, repo, issue.number);
            (issue.number, decide(issue, entry, is_alive, stale_hours, now))
        })
        .collect()
}

/// `gh`/label-flip glue. Not unit-tested directly (mirrors
/// [`crate::work_finder::forge`] / [`crate::epic_supervisor::forge`]) — the
/// decision logic above is the fully-covered surface; this module is a thin,
/// best-effort `Command` wrapper.
pub mod forge {
    use super::{
        plan, resolve_stale_hours, BuildingIssue, ReconcileAction, MAX_ISSUES_PER_WORKSPACE,
    };
    use crate::sweep_journal;
    use anyhow::{anyhow, Context, Result};
    use serde::Deserialize;
    use std::path::Path;
    use std::process::{Command, Stdio};

    #[derive(Debug, Deserialize)]
    struct GhBuildingIssue {
        number: u32,
        #[serde(rename = "updatedAt", default)]
        updated_at: Option<String>,
    }

    fn list_building_issues(gh_bin: &Path, root: &Path) -> Result<Vec<BuildingIssue>> {
        let mut cmd = Command::new(gh_bin);
        cmd.arg("issue")
            .arg("list")
            .arg("--label")
            .arg("loom:building")
            .arg("--state")
            .arg("open")
            .arg("--limit")
            .arg(MAX_ISSUES_PER_WORKSPACE.to_string())
            .arg("--json")
            .arg("number,updatedAt");
        cmd.current_dir(root);
        if let Ok(repo) = std::env::var("LOOM_REPO") {
            cmd.arg("--repo").arg(repo);
        }
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        let out = cmd
            .output()
            .with_context(|| format!("failed to invoke {}", gh_bin.display()))?;
        if !out.status.success() {
            return Err(anyhow!(
                "gh issue list --label loom:building failed in {}: {}",
                root.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        let rows: Vec<GhBuildingIssue> =
            serde_json::from_slice(&out.stdout).context("parse gh issue list JSON")?;
        Ok(rows
            .into_iter()
            .map(|r| BuildingIssue {
                number: r.number,
                updated_at: r
                    .updated_at
                    .as_deref()
                    .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                    .map(|dt| dt.with_timezone(&chrono::Utc)),
            })
            .collect())
    }

    fn reclaim(gh_bin: &Path, root: &Path, issue: u32) -> Result<()> {
        let mut cmd = Command::new(gh_bin);
        cmd.arg("issue")
            .arg("edit")
            .arg(issue.to_string())
            .arg("--remove-label")
            .arg("loom:building")
            .arg("--add-label")
            .arg("loom:issue");
        cmd.current_dir(root);
        if let Ok(repo) = std::env::var("LOOM_REPO") {
            cmd.arg("--repo").arg(repo);
        }
        cmd.stdout(Stdio::null()).stderr(Stdio::piped());
        let out = cmd
            .output()
            .with_context(|| format!("failed to invoke {}", gh_bin.display()))?;
        if !out.status.success() {
            return Err(anyhow!(
                "gh issue edit failed for #{issue} in {}: {}",
                root.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(())
    }

    /// Reconcile stale `loom:building` claims for one registered workspace
    /// `root`, using the machine-level journal as the liveness source.
    /// Best-effort and bounded: any `gh` failure is logged at `warn` and this
    /// workspace's pass returns `(0, 0)` rather than propagating an error (one
    /// repo's forge hiccup must never block the daemon's startup, nor the
    /// other registered workspaces).
    ///
    /// Returns `(checked, reclaimed)` — the number of `loom:building` issues
    /// inspected and the number actually reclaimed, for the caller's summary
    /// log line.
    pub fn reconcile_workspace(gh_bin: &Path, root: &Path) -> (usize, usize) {
        let repo = root.display().to_string();

        let issues = match list_building_issues(gh_bin, root) {
            Ok(v) => v,
            Err(e) => {
                log::warn!("claim_reconciliation: {}: {e}", root.display());
                return (0, 0);
            }
        };
        if issues.is_empty() {
            return (0, 0);
        }

        let journal_path = match sweep_journal::default_journal_path() {
            Ok(p) => p,
            Err(e) => {
                log::warn!("claim_reconciliation: cannot resolve journal path: {e}");
                return (0, 0);
            }
        };
        // IMPORTANT (#3975): decide FIRST against the raw, unpruned journal.
        // A prior version pruned dead-PID entries here before calling
        // `plan()`/`decide()` -- which erased the exact evidence the
        // `DeadPid` branch needs to fire its *unconditional*, immediate
        // reclaim. With the entry gone, every claim silently fell through
        // to the `NoRecordStale` branch and its much longer
        // `stale_hours` grace period, so a claim whose sweep had *just*
        // died (recent label, dead PID) was wrongly kept for hours instead
        // of reclaimed right away. Pruning now happens only *after*
        // deciding, via the per-reclaimed-issue `remove_sweep` below --
        // dead entries for issues this pass didn't touch are cleaned up
        // lazily by the next `record_sweep`/`upsert` elsewhere (which
        // still prunes the whole journal), so the file never accumulates
        // an unbounded graveyard.
        let journal = sweep_journal::load(&journal_path);

        let stale_hours = resolve_stale_hours();
        let now = chrono::Utc::now();
        let decisions =
            plan(&repo, &issues, &journal, &crate::sweep_registry::is_pid_alive, stale_hours, now);
        let checked = decisions.len();

        let mut reclaimed = 0usize;
        for (issue_number, action) in decisions {
            let ReconcileAction::Reclaim(reason) = action else {
                continue;
            };
            match reclaim(gh_bin, root, issue_number) {
                Ok(()) => {
                    reclaimed += 1;
                    log::info!(
                        "claim_reconciliation: reclaimed loom:building -> loom:issue for #{issue_number} \
                         in {} ({:?})",
                        root.display(),
                        reason
                    );
                    // Best-effort tidy-up: drop the (now stale-or-absent)
                    // journal entry so the next pass doesn't re-derive it.
                    let _ = sweep_journal::remove_sweep(&repo, issue_number);
                }
                Err(e) => {
                    log::warn!(
                        "claim_reconciliation: failed to reclaim #{issue_number} in {}: {e}",
                        root.display()
                    );
                }
            }
        }

        (checked, reclaimed)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use chrono::Duration;
    use serial_test::serial;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;

    fn issue(number: u32, updated_at: Option<DateTime<Utc>>) -> BuildingIssue {
        BuildingIssue { number, updated_at }
    }

    fn journal_entry(repo: &str, issue: u32, pid: u32) -> JournalEntry {
        JournalEntry {
            repo: repo.to_string(),
            issue,
            pid,
            started_at: Utc::now(),
        }
    }

    #[test]
    fn decide_keeps_when_journal_entry_pid_alive() {
        let now = Utc::now();
        let entry = journal_entry("/repo/a", 42, 111);
        let action = decide(&issue(42, None), Some(&entry), &|_| true, 4.0, now);
        assert_eq!(action, ReconcileAction::Keep);
    }

    #[test]
    fn decide_reclaims_when_journal_entry_pid_dead() {
        let now = Utc::now();
        let entry = journal_entry("/repo/a", 42, 111);
        let action = decide(&issue(42, None), Some(&entry), &|_| false, 4.0, now);
        assert_eq!(action, ReconcileAction::Reclaim(ReclaimReason::DeadPid { pid: 111 }));
    }

    #[test]
    fn decide_keeps_when_no_record_and_within_grace() {
        let now = Utc::now();
        let recent = now - Duration::hours(1);
        let action = decide(&issue(42, Some(recent)), None, &|_| true, 4.0, now);
        assert_eq!(action, ReconcileAction::Keep);
    }

    #[test]
    fn decide_reclaims_when_no_record_and_past_stale_threshold() {
        let now = Utc::now();
        let old = now - Duration::hours(5);
        let action = decide(&issue(42, Some(old)), None, &|_| true, 4.0, now);
        match action {
            ReconcileAction::Reclaim(ReclaimReason::NoRecordStale { age_hours }) => {
                assert!(age_hours >= 4.0);
            }
            other => panic!("expected NoRecordStale reclaim, got {other:?}"),
        }
    }

    #[test]
    fn decide_keeps_at_exact_boundary_minus_epsilon() {
        let now = Utc::now();
        // Just under the threshold: still within grace.
        let almost = now - Duration::minutes(239); // 3h59m < 4h
        let action = decide(&issue(42, Some(almost)), None, &|_| true, 4.0, now);
        assert_eq!(action, ReconcileAction::Keep);
    }

    #[test]
    fn decide_keeps_when_no_record_and_no_age_evidence() {
        let now = Utc::now();
        let action = decide(&issue(42, None), None, &|_| true, 4.0, now);
        assert_eq!(action, ReconcileAction::Keep, "fail-safe: no evidence => Keep");
    }

    #[test]
    fn decide_dead_pid_overrides_label_age() {
        // Even a freshly-labeled issue must be reclaimed once its recorded
        // PID is provably dead — the journal is authoritative when present.
        let now = Utc::now();
        let entry = journal_entry("/repo/a", 42, 111);
        let fresh_issue = issue(42, Some(now - Duration::minutes(1)));
        let action = decide(&fresh_issue, Some(&entry), &|_| false, 4.0, now);
        assert_eq!(action, ReconcileAction::Reclaim(ReclaimReason::DeadPid { pid: 111 }));
    }

    #[test]
    fn plan_maps_each_issue_to_its_own_journal_entry() {
        let now = Utc::now();
        let mut journal = SweepJournal::default();
        journal.entries.push(journal_entry("/repo/a", 1, 111)); // will be dead
        journal.entries.push(journal_entry("/repo/a", 2, 222)); // will be alive
                                                                // #3 has no journal entry and is stale.
        let issues = vec![
            issue(1, None),
            issue(2, None),
            issue(3, Some(now - Duration::hours(10))),
        ];

        let decisions = plan("/repo/a", &issues, &journal, &|pid| pid == 222, 4.0, now);

        assert_eq!(decisions.len(), 3);
        assert_eq!(
            decisions[0],
            (1, ReconcileAction::Reclaim(ReclaimReason::DeadPid { pid: 111 }))
        );
        assert_eq!(decisions[1], (2, ReconcileAction::Keep));
        match decisions[2] {
            (3, ReconcileAction::Reclaim(ReclaimReason::NoRecordStale { .. })) => {}
            ref other => panic!("expected #3 to be a NoRecordStale reclaim, got {other:?}"),
        }
    }

    #[test]
    fn plan_scopes_journal_lookup_by_repo_string() {
        // Same issue number, different repos — must not cross-contaminate.
        let now = Utc::now();
        let mut journal = SweepJournal::default();
        journal.entries.push(journal_entry("/repo/other", 42, 111));

        let issues = vec![issue(42, Some(now - Duration::hours(10)))];
        let decisions = plan("/repo/a", &issues, &journal, &|_| true, 4.0, now);

        // No entry under "/repo/a" -> falls through to the age check, which
        // is stale here, so it reclaims (not "Keep" from the other repo's
        // live pid).
        match decisions[0] {
            (42, ReconcileAction::Reclaim(ReclaimReason::NoRecordStale { .. })) => {}
            ref other => panic!("expected repo-scoped NoRecordStale reclaim, got {other:?}"),
        }
    }

    #[test]
    #[serial]
    fn reconciliation_enabled_resolves_env_precedence() {
        std::env::remove_var(RECONCILE_ENABLED_ENV);
        assert!(reconciliation_enabled(), "defaults to enabled");

        for off in ["0", "false", "no", "off", "OFF", "False"] {
            std::env::set_var(RECONCILE_ENABLED_ENV, off);
            assert!(!reconciliation_enabled(), "{off} should disable");
        }

        std::env::set_var(RECONCILE_ENABLED_ENV, "1");
        assert!(reconciliation_enabled());

        std::env::remove_var(RECONCILE_ENABLED_ENV);
    }

    #[test]
    #[serial]
    fn resolve_stale_hours_defaults_and_overrides() {
        std::env::remove_var(STALE_HOURS_ENV);
        assert!((resolve_stale_hours() - DEFAULT_STALE_BUILDING_HOURS).abs() < f64::EPSILON);

        std::env::set_var(STALE_HOURS_ENV, "2.5");
        assert!((resolve_stale_hours() - 2.5).abs() < f64::EPSILON);

        // Non-positive / unparseable falls back to the default.
        std::env::set_var(STALE_HOURS_ENV, "0");
        assert!((resolve_stale_hours() - DEFAULT_STALE_BUILDING_HOURS).abs() < f64::EPSILON);
        std::env::set_var(STALE_HOURS_ENV, "garbage");
        assert!((resolve_stale_hours() - DEFAULT_STALE_BUILDING_HOURS).abs() < f64::EPSILON);

        std::env::remove_var(STALE_HOURS_ENV);
    }

    /// Regression test for #3975: `reconcile_workspace` used to prune dead
    /// journal entries *before* deciding, which erased the exact evidence the
    /// `DeadPid` branch needs. A claim with a provably-dead recorded PID must
    /// be reclaimed immediately by the daemon's own startup pass, even when
    /// the `loom:building` label is only seconds old (well inside the
    /// `NoRecordStale` grace window) -- two incidents (#6170/#6173 in a
    /// downstream workspace) were exactly this: SIGTERMed sweeps left a dead
    /// PID in the journal, and the pre-decide prune silently downgraded them
    /// to "no record", so they sat un-reclaimed for hours.
    #[test]
    #[serial]
    fn reconcile_workspace_reclaims_dead_pid_entry_even_when_label_is_fresh() {
        let dir = tempdir().unwrap();
        let repo_root = dir.path().join("repo");
        std::fs::create_dir_all(&repo_root).unwrap();
        let repo_str = repo_root.display().to_string();

        let journal_path = dir.path().join("sweeps.json");
        std::env::set_var(sweep_journal::JOURNAL_PATH_ENV, &journal_path);

        // Seed a dead-PID entry (pid 0 is always dead per `is_pid_alive`).
        let mut journal = SweepJournal::default();
        journal.entries.push(journal_entry(&repo_str, 99, 0));
        sweep_journal::save(&journal_path, &journal).unwrap();

        // Fake `gh`: `issue list` reports one loom:building issue labeled
        // *just now* -- fresh enough that the NoRecordStale (age-based) path
        // would say Keep. Only the DeadPid evidence should trigger a reclaim.
        let gh_log = dir.path().join("gh-invocations.log");
        let fake_gh = dir.path().join("fake-gh.sh");
        let now = Utc::now().to_rfc3339();
        let script = format!(
            r#"#!/usr/bin/env bash
printf '%s\n' "$*" >> "{log}"
if [ "$1" = "issue" ] && [ "$2" = "list" ]; then
  echo '[{{"number":99,"updatedAt":"{now}"}}]'
  exit 0
fi
exit 0
"#,
            log = gh_log.display(),
            now = now,
        );
        std::fs::write(&fake_gh, &script).unwrap();
        #[cfg(unix)]
        {
            let mut perms = std::fs::metadata(&fake_gh).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&fake_gh, perms).unwrap();
        }

        let (checked, reclaimed) = forge::reconcile_workspace(&fake_gh, &repo_root);

        assert_eq!(checked, 1);
        assert_eq!(
            reclaimed, 1,
            "a dead-PID journal entry must be reclaimed immediately regardless of \
             how fresh the loom:building label is (#3975)"
        );

        let gh_calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
        assert!(
            gh_calls.contains("issue edit 99 --remove-label loom:building --add-label loom:issue"),
            "expected reclaim to flip labels for #99; got: {gh_calls:?}"
        );

        // The reclaimed issue's journal entry is cleaned up as part of
        // recovery -- confirms cleanup still happens, just after (not
        // before) the decision that needed the evidence.
        let after = sweep_journal::load(&journal_path);
        assert!(sweep_journal::find(&after, &repo_str, 99).is_none());

        std::env::remove_var(sweep_journal::JOURNAL_PATH_ENV);
    }
}
