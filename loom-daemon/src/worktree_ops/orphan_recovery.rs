//! `loom-daemon recover-orphans`: native port of `loom-recover-orphans`
//! (`orphan_recovery.py`).
//!
//! Detects two kinds of orphaned state and (with `--recover`) fixes them:
//!
//! - `untracked_building`: an issue carries `loom:building` but no live
//!   sweep is tracking it.
//! - `stale_heartbeat`: a `.loom/spawn-loop-state.json::running` entry's
//!   heartbeat is stale and its recorded PID is dead.
//!
//! Liveness evidence is gathered from every available source and unioned
//! (never intersected) — see [`gather_liveness_evidence`]. The fail-safe
//! invariant from issue #3651 is preserved exactly: **absent evidence means
//! treat every claim as ALIVE**, never as orphaned.
//!
//! An **open linked PR is itself liveness evidence** (issue #5511): no
//! `loom:building` reset happens until the forge's closes-graph has *verified*
//! that no open PR references the issue — see [`open_linked_pr_blocks_reset`].

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use super::claim_file::has_valid_claim;
use super::gh;
use super::gh::OpenPrProbe;
use super::liveness::{active_locked_issues, locks_dir};
use super::spawn_loop_state::{read_spawn_loop_state, SpawnLoopState};

pub const DEFAULT_HEARTBEAT_STALE_THRESHOLD_SECS: i64 = 300;
pub const DEFAULT_LABEL_GRACE_PERIOD_SECS: i64 = 600;
pub const DEFAULT_STALE_BUILDING_HOURS: f64 = 4.0;
pub const ORPHAN_COMMENT_DEDUP_SECONDS: i64 = 300;

/// Paths that mark an uncommitted change as "regenerable build output" rather
/// than real work — [`cleanup_stale_worktree`] uses this to decide whether a
/// zero-commits-ahead worktree's dirty tree is safe to discard outright, and
/// [`super::clean::reclaim_worktree_artifacts`] (issue #5187) reuses the same
/// list to select which top-level directories a **kept** worktree's build
/// artifacts can be reclaimed from without removing the worktree itself.
///
/// Hoisted to module scope (was a `cleanup_stale_worktree`-local const) so a
/// second consumer never drifts from this one — see #5187.
pub(crate) const BUILD_ARTIFACT_PATTERNS: &[&str] = &[
    "node_modules",
    "pnpm-lock.yaml",
    ".venv",
    "target/",
    "Cargo.lock",
    "coverage/",
    ".loom-checkpoint",
    ".loom-in-use",
];

fn heartbeat_stale_threshold() -> i64 {
    std::env::var("LOOM_HEARTBEAT_STALE_THRESHOLD")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_HEARTBEAT_STALE_THRESHOLD_SECS)
}

fn label_grace_period() -> i64 {
    std::env::var("LOOM_LABEL_GRACE_PERIOD")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_LABEL_GRACE_PERIOD_SECS)
}

/// Mirrors `orphan_recovery.py::_get_stale_building_hours`: env override must
/// be a positive float or the default is used.
fn stale_building_hours() -> f64 {
    std::env::var(crate::claim_reconciliation::STALE_HOURS_ENV)
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|v| *v > 0.0)
        .unwrap_or(DEFAULT_STALE_BUILDING_HOURS)
}

fn pid_alive(pid: u32) -> bool {
    crate::sweep_registry::is_pid_alive(pid)
}

/// A detected orphan.
#[derive(Debug, Clone)]
pub struct OrphanEntry {
    // "untracked_building" | "stale_heartbeat" | "stale_reviewing_pr" |
    // "stale_treating_pr" (the last two: Issue #6167, PR-side claim overlays)
    pub kind: &'static str,
    pub issue: Option<u32>,
    /// The PR number, for the PR-side claim kinds (`stale_reviewing_pr` /
    /// `stale_treating_pr`). `None` for every issue-side kind.
    pub pr: Option<u32>,
    pub pid: Option<u32>,
    pub title: Option<String>,
    pub reason: String,
    pub age_seconds: Option<i64>,
}

/// A recovery action taken.
#[derive(Debug, Clone)]
pub struct RecoveryEntry {
    // "reset_issue_label" | "cleanup_stale_worktree" | "reclaim_pr_claim"
    pub action: &'static str,
    pub issue: Option<u32>,
    /// The PR number, for `reclaim_pr_claim` (Issue #6167). `None` for every
    /// issue-side action.
    pub pr: Option<u32>,
    pub reason: String,
}

/// A `loom:building` issue the staleness gate skipped (visible even without
/// `--verbose`, issue #3975).
#[derive(Debug, Clone)]
pub struct WatchedEntry {
    pub issue: u32,
    pub title: Option<String>,
    pub reason: &'static str,
    pub age_seconds: Option<i64>,
    pub threshold_seconds: f64,
}

#[derive(Debug, Default)]
pub struct OrphanRecoveryResult {
    pub orphaned: Vec<OrphanEntry>,
    pub recovered: Vec<RecoveryEntry>,
    pub watched: Vec<WatchedEntry>,
    pub recover_mode: bool,
    /// Dependency failures that made the assessment incomplete (e.g. the
    /// `gh issue list --label loom:building` query failed). Non-empty means
    /// "could not assess", which is NOT the same claim as "found nothing" —
    /// the report must say so and the CLI must exit non-zero (issue #5140).
    pub assessment_errors: Vec<String>,
}

impl OrphanRecoveryResult {
    /// Whether a dependency needed to enumerate claims failed, making the
    /// result inconclusive rather than empty.
    #[must_use]
    pub fn assessment_failed(&self) -> bool {
        !self.assessment_errors.is_empty()
    }
}

/// Authoritative evidence of which sweeps are alive right now.
#[derive(Debug, Default)]
pub struct LivenessEvidence {
    pub available: bool,
    pub live_issues: HashSet<u32>,
    pub sources: Vec<&'static str>,
    pub journal_present: bool,
    pub journal_issues: HashSet<u32>,
}

/// Best-effort daemon-registry query. Mirrors
/// `orphan_recovery.py::_query_daemon_live_issues`, which is (deliberately)
/// a permanent no-op stub: the CLI runs as a standalone process with no
/// socket to a possibly-running daemon, so this is never a contributing
/// source today. Returns `None` ("not a source"), never an empty set
/// ("daemon says nothing is live") — those are different claims.
fn query_daemon_live_issues(_repo_root: &Path) -> Option<HashSet<u32>> {
    None
}

/// Gather liveness evidence from every available source and union them.
/// Mirrors `orphan_recovery.py::gather_liveness_evidence`.
#[must_use]
pub fn gather_liveness_evidence(
    spawn_loop_state: &SpawnLoopState,
    repo_root: &Path,
) -> LivenessEvidence {
    let mut live: HashSet<u32> = HashSet::new();
    let mut sources: Vec<&'static str> = Vec::new();
    let mut journal_present = false;
    let mut journal_issues: HashSet<u32> = HashSet::new();

    if spawn_loop_state.present {
        sources.push("spawn-loop-state.json");
        live.extend(
            spawn_loop_state
                .running
                .iter()
                .filter(|t| t.issue != 0)
                .map(|t| t.issue),
        );
    }

    if let Some(daemon_issues) = query_daemon_live_issues(repo_root) {
        sources.push("loom-daemon");
        live.extend(daemon_issues);
    }

    let locked = active_locked_issues(repo_root);
    if !locked.is_empty() {
        sources.push(".loom/locks");
        live.extend(locked);
    }

    if let Ok(journal_path) = crate::sweep_journal::default_journal_path() {
        if journal_path.exists() {
            journal_present = true;
            sources.push("sweep-journal");
            let journal = crate::sweep_journal::load(&journal_path);
            let repo_str = repo_root.display().to_string();
            let repo_resolved = repo_root.canonicalize().ok();
            for entry in &journal.entries {
                let matches = entry.repo == repo_str
                    || repo_resolved.as_ref().is_some_and(|r| {
                        std::path::Path::new(&entry.repo)
                            .canonicalize()
                            .ok()
                            .as_ref()
                            == Some(r)
                    });
                if !matches {
                    continue;
                }
                journal_issues.insert(entry.issue);
                if pid_alive(entry.pid) {
                    live.insert(entry.issue);
                }
            }
        }
    }

    LivenessEvidence {
        available: !sources.is_empty(),
        live_issues: live,
        sources,
        journal_present,
        journal_issues,
    }
}

/// Whether an open linked PR forbids resetting `issue`'s `loom:building`
/// label back to `loom:issue` (issue #5511).
///
/// Before #5511 this path had **no PR-existence check at all**: its only
/// "is someone still working this?" signals were the daemon registry, a
/// file-based claim lock, and a spawn-loop journal entry — none of which
/// consult the forge. That is how issue #5501 got reset to `loom:issue` while
/// PR #5507 (`Closes #5501`) was open and being actively treated by Doctor,
/// leaving live work looking unclaimed and inviting a duplicate Builder claim.
///
/// The probe uses the forge's authoritative closes-graph (so `Closes #N` in a
/// PR body counts, whatever the branch is named) and is **fail-safe in the same
/// direction as the rest of this module**: only a VERIFIED "no open linked PR"
/// permits a reset. A probe failure (forge outage, wedged/missing `gh`,
/// unresolvable repo) blocks the reset exactly like a verified open PR would,
/// mirroring the `evidence.available == false` short-circuit in
/// [`check_untracked_building`] — absent evidence means treat the claim as
/// ALIVE (#3651).
fn open_linked_pr_blocks_reset(repo_root: &Path, issue: u32) -> bool {
    match gh::probe_open_linked_pr(repo_root, issue) {
        OpenPrProbe::Open(pr) => {
            eprintln!(
                "Skipping recovery for issue #{issue}: PR #{pr} is open and linked to it \
                 (someone is still working this) -- see #5511"
            );
            true
        }
        OpenPrProbe::ProbeFailed => {
            eprintln!(
                "Skipping recovery for issue #{issue}: could not verify whether a linked PR \
                 is open (probe failed) -- failing safe and treating the claim as ALIVE (#5511)"
            );
            true
        }
        OpenPrProbe::NoneOpen => false,
    }
}

/// Cross-reference `loom:building` issues against `evidence`. Mirrors
/// `orphan_recovery.py::check_untracked_building`, including the #3975
/// "watched" bookkeeping and the #3953 dual staleness-threshold selection.
pub fn check_untracked_building(
    evidence: &LivenessEvidence,
    result: &mut OrphanRecoveryResult,
    repo_root: &Path,
    label_grace_period_secs: i64,
    verbose: bool,
) {
    if !evidence.available {
        if verbose {
            eprintln!(
                "No authoritative liveness source available (no spawn-loop-state.json, no \
                 reachable loom-daemon registry, no .loom/locks/issue-<N>/ locks) — refusing \
                 to flag any loom:building issue as orphaned (fail-safe: absent liveness data \
                 means treat claims as ALIVE, not orphaned). See issue #3651."
            );
        }
        return;
    }

    let building_issues = match gh::list_building_issues(repo_root) {
        Ok(v) => v,
        Err(e) => {
            // A failed query is NOT evidence that nothing is claimed. Record
            // it so the report says "could not assess" instead of the
            // reassuring "No orphaned tasks found", and so the CLI exits
            // non-zero (issue #5140).
            eprintln!("Failed to list loom:building issues: {e}");
            result.assessment_errors.push(format!(
                "could not enumerate loom:building claims in {}: {e}",
                repo_root.display()
            ));
            return;
        }
    };
    if building_issues.is_empty() {
        return;
    }

    for issue_data in building_issues {
        let issue_num = issue_data.number;
        let issue_title = issue_data.title;

        if evidence.live_issues.contains(&issue_num) {
            continue;
        }

        if has_valid_claim(repo_root, issue_num) {
            continue;
        }

        let has_journal_entry = evidence.journal_issues.contains(&issue_num);
        let (threshold_seconds, reason): (f64, &'static str) =
            if evidence.journal_present && !has_journal_entry {
                (stale_building_hours() * 3600.0, "no_journal_record_stale")
            } else if has_journal_entry {
                (label_grace_period_secs as f64, "journal_pid_dead")
            } else {
                (label_grace_period_secs as f64, "no_spawn_loop_entry")
            };

        if threshold_seconds > 0.0 {
            let label_age = gh::building_label_age_seconds(repo_root, issue_num);
            if let Some(age) = label_age {
                if (age as f64) < threshold_seconds {
                    result.watched.push(WatchedEntry {
                        issue: issue_num,
                        title: Some(issue_title.clone()),
                        reason,
                        age_seconds: Some(age),
                        threshold_seconds,
                    });
                    continue;
                }
            }
        }

        // #5511: last gate before declaring the claim orphaned — ask the forge
        // whether an open PR is linked to this issue. Deliberately placed AFTER
        // the staleness/watched gate so it costs one GraphQL round-trip only for
        // issues that are actually about to be flagged, not for every
        // `loom:building` issue on every pass.
        if open_linked_pr_blocks_reset(repo_root, issue_num) {
            continue;
        }

        result.orphaned.push(OrphanEntry {
            kind: "untracked_building",
            issue: Some(issue_num),
            pr: None,
            pid: None,
            title: Some(issue_title),
            reason: reason.to_string(),
            age_seconds: None,
        });
    }
}

/// Flag `.loom/spawn-loop-state.json::running` entries whose heartbeat is
/// stale and PID is dead. Mirrors `orphan_recovery.py::check_stale_heartbeats`.
pub fn check_stale_heartbeats(
    spawn_loop_state: &SpawnLoopState,
    result: &mut OrphanRecoveryResult,
    heartbeat_threshold_secs: i64,
) {
    for task in &spawn_loop_state.running {
        let Some(hb) = &task.last_heartbeat else {
            continue;
        };
        let Ok(dt) = chrono::DateTime::parse_from_rfc3339(hb) else {
            continue;
        };
        let age = chrono::Utc::now()
            .signed_duration_since(dt.with_timezone(&chrono::Utc))
            .num_seconds();
        if age <= heartbeat_threshold_secs {
            continue;
        }
        if pid_alive(task.pid) {
            continue;
        }
        result.orphaned.push(OrphanEntry {
            kind: "stale_heartbeat",
            issue: if task.issue != 0 {
                Some(task.issue)
            } else {
                None
            },
            pr: None,
            pid: Some(task.pid),
            title: None,
            reason: "heartbeat_stale".to_string(),
            age_seconds: Some(age),
        });
    }
}

/// Best-effort stale-worktree cleanup for a recovered issue (0 commits ahead
/// of main, only build-artifact uncommitted changes). Mirrors
/// `orphan_recovery.py::_cleanup_stale_worktree`.
fn cleanup_stale_worktree(repo_root: &Path, issue: u32) -> bool {
    let worktree_path =
        crate::worktree_root::worktree_root(repo_root).join(super::naming::worktree_name(issue));
    if !worktree_path.is_dir() {
        return false;
    }

    let log_out = std::process::Command::new("git")
        .args(["-C"])
        .arg(&worktree_path)
        .args(["log", "--oneline", "origin/main..HEAD"])
        .output();
    let Ok(log_out) = log_out else { return false };
    if !log_out.status.success() {
        return false;
    }
    if !String::from_utf8_lossy(&log_out.stdout).trim().is_empty() {
        return false;
    }

    let status_out = std::process::Command::new("git")
        .args(["-C"])
        .arg(&worktree_path)
        .args(["status", "--porcelain"])
        .output();
    let Ok(status_out) = status_out else {
        return false;
    };
    if !status_out.status.success() {
        return false;
    }
    for line in String::from_utf8_lossy(&status_out.stdout).trim().lines() {
        // porcelain lines look like "XY path" (or "XY orig -> new" for renames);
        // take the path portion after the two-char status + space.
        let path_part = line.get(3..).unwrap_or(line);
        if !BUILD_ARTIFACT_PATTERNS
            .iter()
            .any(|pat| path_part.contains(pat))
        {
            return false;
        }
    }

    let branch_out = std::process::Command::new("git")
        .args(["-C"])
        .arg(&worktree_path)
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output();
    let branch = branch_out
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();

    let removed = std::process::Command::new("git")
        .args(["worktree", "remove"])
        .arg(&worktree_path)
        .arg("--force")
        .current_dir(repo_root)
        .status()
        .is_ok_and(|s| s.success());
    if !removed {
        return false;
    }

    // #5950: `loom-recover-orphans` is the removal path most likely to be
    // mistaken for one of the others — it targets exactly the `loom:building`
    // issues a live Builder holds, and it deletes the branch both locally and
    // on the remote below. Its own guards (0 commits ahead of origin/main,
    // build-artifact-only dirt) are what make that safe; the ledger entry is
    // what makes it *attributable* after the fact.
    super::removal_log::record(
        repo_root,
        "loom-recover-orphans",
        &worktree_path,
        if branch.is_empty() {
            None
        } else {
            Some(branch.as_str())
        },
        "stale_worktree_for_orphaned_issue",
    );

    if !branch.is_empty() && branch != "main" {
        let _ = std::process::Command::new("git")
            .args(["-C"])
            .arg(repo_root)
            .args(["branch", "-D", &branch])
            .status();
        let _ = std::process::Command::new("git")
            .args(["-C"])
            .arg(repo_root)
            .args(["push", "origin", "--delete", &branch])
            .status();
    }
    true
}

/// Reset an orphaned issue's label back to `loom:issue` (+ best-effort
/// stale-worktree cleanup + a dedup'd comment). Mirrors
/// `orphan_recovery.py::recover_issue`.
pub fn recover_issue(
    repo_root: &Path,
    issue: u32,
    reason: &str,
    result: &mut OrphanRecoveryResult,
    label_grace_period_secs: i64,
) {
    if label_grace_period_secs > 0 {
        if let Some(age) = gh::building_label_age_seconds(repo_root, issue) {
            if age < label_grace_period_secs {
                eprintln!(
                    "Skipping recovery for issue #{issue}: loom:building label applied {age}s ago \
                     (grace period: {label_grace_period_secs}s)"
                );
                return;
            }
        }
    }

    if has_valid_claim(repo_root, issue) {
        eprintln!("Skipping recovery for issue #{issue}: valid file-based claim exists");
        return;
    }

    // #5511 defense in depth: `check_untracked_building` already gates on this,
    // but `recover_issue` is a `pub` entry point that can be (and is) called
    // directly, bypassing the flagging pass. Checked BEFORE
    // `cleanup_stale_worktree` as well as before `gh::edit_labels`, because
    // deleting a live worker's worktree/branch is at least as destructive as
    // flipping its label.
    if open_linked_pr_blocks_reset(repo_root, issue) {
        return;
    }

    let worktree_cleaned = cleanup_stale_worktree(repo_root, issue);
    if worktree_cleaned {
        result.recovered.push(RecoveryEntry {
            action: "cleanup_stale_worktree",
            issue: Some(issue),
            pr: None,
            reason: reason.to_string(),
        });
    }

    if let Err(e) = gh::edit_labels(repo_root, issue, "loom:building", "loom:issue") {
        eprintln!("Failed to update labels for issue #{issue}: {e}");
        return;
    }

    let ts = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let mut actions = vec![
        "- Removed `loom:building` label".to_string(),
        "- Added `loom:issue` label to return to ready queue".to_string(),
    ];
    if worktree_cleaned {
        actions.push("- Cleaned up stale worktree and branches".to_string());
    }
    let comment = format!(
        "## Orphan Recovery\n\n\
         This issue was automatically recovered from an orphaned state.\n\n\
         **Reason**: {reason}\n\
         **What happened**:\n\
         - The spawn-loop task that was working on this issue crashed or was terminated\n\
         - The issue was left in `loom:building` state with no active worker\n\n\
         **Action taken**:\n{}\n\n\
         This issue is now available for a new sweep to pick up.\n\n\
         ---\n\
         *Recovered by loom-recover-orphans at {ts}*",
        actions.join("\n")
    );

    if !gh::has_recent_orphan_comment(repo_root, issue, ORPHAN_COMMENT_DEDUP_SECONDS) {
        if let Err(e) = gh::comment(repo_root, issue, &comment) {
            eprintln!("Failed to add comment to issue #{issue}: {e}");
        }
    }

    result.recovered.push(RecoveryEntry {
        action: "reset_issue_label",
        issue: Some(issue),
        pr: None,
        reason: reason.to_string(),
    });
    println!("Recovered issue #{issue}");
}

/// The `gh` binary to invoke for the PR-side claim pass. Honors `LOOM_GH_BIN`
/// (tests / overrides), the same seam [`super::gh`]'s own `gh_bin()` uses, so
/// a fixture can steer this pass without mutating the process-wide `PATH`.
fn pr_claim_gh_bin() -> PathBuf {
    PathBuf::from(std::env::var("LOOM_GH_BIN").unwrap_or_else(|_| "gh".to_string()))
}

/// Format a [`crate::claim_reconciliation::PrReclaimReason`] as a short,
/// machine-readable token — consistent with the snake_case reason strings
/// [`check_untracked_building`] already emits (`no_spawn_loop_entry`,
/// `journal_pid_dead`, ...), rather than the enum's `Debug` shape.
fn format_pr_reclaim_reason(reason: crate::claim_reconciliation::PrReclaimReason) -> String {
    use crate::claim_reconciliation::PrReclaimReason;
    match reason {
        PrReclaimReason::DeadPid { pid } => format!("dead_pid:{pid}"),
        PrReclaimReason::DeadRunRegistry { pid } => format!("dead_run_registry:{pid}"),
        PrReclaimReason::Aged { age_minutes } => format!("aged:{age_minutes:.1}m"),
    }
}

/// Detect (and, if `recover`, reclaim) stale PR-side `loom:reviewing`
/// (Judge) / `loom:treating` (Doctor) claims — the PR-side analogue of
/// [`check_untracked_building`] (Issue #6167).
///
/// Delegates entirely to
/// [`crate::claim_reconciliation::forge::reconcile_pr_claims_report`] so the
/// staleness threshold (`LOOM_STALE_REVIEWING_MINUTES` /
/// `LOOM_STALE_TREATING_MINUTES`) and liveness discipline (journal +
/// checkpoint→run-registry join, never stripping a claim backed by a live
/// pid, age-gated on `claim_labeled_at`/substantive-comment freshness — see
/// `claim_reconciliation`'s module docs) are defined in exactly one place,
/// shared with the daemon's own periodic backstop
/// ([`crate::claim_reconciliation::run_reconciliation_pass`]) and
/// judge.md's/doctor.md's agent-side "Stale `loom:reviewing`/`loom:treating`
/// Claim Check". `recover-orphaned-shepherds.sh` (this module's CLI
/// consumer) previously only ran the issue-side pass below — a dead Judge's
/// `loom:reviewing` claim on an otherwise-actionable PR had no scripted
/// recovery path at all until an agent happened to review that exact PR.
///
/// `recover=false` performs the identical detection pass with no `gh pr
/// edit` calls — every reported entry is detection-only, mirroring the
/// issue-side dry-run contract.
pub fn check_stale_pr_claims(repo_root: &Path, result: &mut OrphanRecoveryResult, recover: bool) {
    let gh_bin = pr_claim_gh_bin();
    let (_checked, outcomes) =
        crate::claim_reconciliation::forge::reconcile_pr_claims_report(&gh_bin, repo_root, recover);

    for outcome in outcomes {
        let kind = if outcome.label == "loom:treating" {
            "stale_treating_pr"
        } else {
            "stale_reviewing_pr"
        };
        let reason = format_pr_reclaim_reason(outcome.reason);

        result.orphaned.push(OrphanEntry {
            kind,
            issue: None,
            pr: Some(outcome.pr_number),
            pid: None,
            title: None,
            reason: reason.clone(),
            age_seconds: None,
        });

        if outcome.reclaimed {
            result.recovered.push(RecoveryEntry {
                action: "reclaim_pr_claim",
                issue: None,
                pr: Some(outcome.pr_number),
                reason: format!("{} ({reason})", outcome.label),
            });
        }
    }
}

/// Run all detection phases and, if `recover`, perform recovery. Mirrors
/// `orphan_recovery.py::run_orphan_recovery`.
pub fn run_orphan_recovery(repo_root: &Path, recover: bool, verbose: bool) -> OrphanRecoveryResult {
    let mut result = OrphanRecoveryResult {
        recover_mode: recover,
        ..Default::default()
    };
    let heartbeat_threshold = heartbeat_stale_threshold();
    let grace_period = label_grace_period();

    let spawn_loop_state = read_spawn_loop_state(repo_root);
    let evidence = gather_liveness_evidence(&spawn_loop_state, repo_root);

    if verbose {
        if evidence.available {
            let mut live: Vec<_> = evidence.live_issues.iter().collect();
            live.sort();
            eprintln!(
                "Liveness sources: {} (live issues: {:?})",
                evidence.sources.join(", "),
                live
            );
        } else {
            eprintln!(
                "No authoritative liveness source found — untracked-building cross-check will \
                 fail safe (emit zero orphans). See #3651."
            );
        }
    }

    check_untracked_building(&evidence, &mut result, repo_root, grace_period, verbose);
    check_stale_heartbeats(&spawn_loop_state, &mut result, heartbeat_threshold);
    // Issue #6167: PR-side `loom:reviewing`/`loom:treating` claims are
    // detected (and, when `recover`, reclaimed) inline here rather than via
    // the issue-only recovery loop below — a PR-side reclaim is a different
    // gh mutation (a claim label removal, not an issue label swap) and
    // `check_stale_pr_claims` already respects `recover` itself.
    check_stale_pr_claims(repo_root, &mut result, recover);

    if !recover {
        return result;
    }

    let orphans: Vec<(u32, String)> = result
        .orphaned
        .iter()
        .filter_map(|o| o.issue.map(|n| (n, o.reason.clone())))
        .collect();
    for (issue, reason) in orphans {
        recover_issue(repo_root, issue, &reason, &mut result, grace_period);
    }

    result
}

fn format_duration(seconds: i64) -> String {
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m", seconds / 60)
    } else {
        format!("{}h{}m", seconds / 3600, (seconds % 3600) / 60)
    }
}

#[must_use]
pub fn format_result_human(result: &OrphanRecoveryResult) -> String {
    let mut lines: Vec<String> = Vec::new();
    if result.assessment_failed() {
        // Never print a success-shaped summary after a dependency failed
        // (issue #5140): an operator or watch loop reading "No orphaned tasks
        // found" would conclude nothing is stranded.
        lines.push("Could not assess orphaned tasks -- results are INCOMPLETE".to_string());
        for e in &result.assessment_errors {
            lines.push(format!("  [error] {e}"));
        }
        if !result.orphaned.is_empty() {
            lines.push(String::new());
            lines.push(format!(
                "{} orphaned task(s) detected before the failure (partial):",
                result.orphaned.len()
            ));
            for orphan in &result.orphaned {
                if let Some(pr) = orphan.pr {
                    lines.push(format!("  [{}] PR #{}: {}", orphan.kind, pr, orphan.reason));
                } else {
                    lines.push(format!(
                        "  [{}] #{}: {}",
                        orphan.kind,
                        orphan.issue.unwrap_or(0),
                        orphan.reason
                    ));
                }
            }
        }
    } else if result.orphaned.is_empty() {
        lines.push("No orphaned tasks found".to_string());
    } else {
        lines.push(format!("Found {} orphaned task(s)", result.orphaned.len()));
        lines.push(String::new());
        for orphan in &result.orphaned {
            match orphan.kind {
                "untracked_building" => lines.push(format!(
                    "  [{}] #{}: {} -- no active spawn-loop task",
                    orphan.kind,
                    orphan.issue.unwrap_or(0),
                    orphan.title.as_deref().unwrap_or("no title")
                )),
                "stale_heartbeat" => lines.push(format!(
                    "  [{}] issue #{} (pid {}): heartbeat stale ({})",
                    orphan.kind,
                    orphan.issue.unwrap_or(0),
                    orphan.pid.unwrap_or(0),
                    format_duration(orphan.age_seconds.unwrap_or(0))
                )),
                "stale_reviewing_pr" | "stale_treating_pr" => lines.push(format!(
                    "  [{}] PR #{}: {} -- dead claimant, no verdict",
                    orphan.kind,
                    orphan.pr.unwrap_or(0),
                    orphan.reason
                )),
                _ => {}
            }
        }
        if result.recover_mode {
            lines.push(String::new());
            lines.push(format!("Recovered {} item(s)", result.recovered.len()));
        } else {
            lines.push(String::new());
            lines.push("Run with --recover to fix these issues".to_string());
        }
    }

    if !result.watched.is_empty() {
        lines.push(String::new());
        lines.push(format!(
            "{} claim(s) seen but not yet stale enough to reclaim:",
            result.watched.len()
        ));
        for w in &result.watched {
            let age = w.age_seconds.unwrap_or(0);
            let threshold = w.threshold_seconds as i64;
            let remaining = (threshold - age).max(0);
            lines.push(format!(
                "  [watched] #{}: {} -- skipped ({}): label age {}, threshold {}, eligible in {}",
                w.issue,
                w.title.as_deref().unwrap_or("no title"),
                w.reason,
                format_duration(age),
                format_duration(threshold),
                format_duration(remaining)
            ));
        }
    }

    lines.join("\n")
}

#[must_use]
pub fn format_result_json(result: &OrphanRecoveryResult) -> String {
    let orphaned: Vec<_> = result
        .orphaned
        .iter()
        .map(|o| {
            let mut m = serde_json::Map::new();
            m.insert("type".to_string(), o.kind.into());
            m.insert("reason".to_string(), o.reason.clone().into());
            if let Some(i) = o.issue {
                m.insert("issue".to_string(), i.into());
            }
            if let Some(pr) = o.pr {
                m.insert("pr".to_string(), pr.into());
            }
            if let Some(p) = o.pid {
                m.insert("pid".to_string(), p.into());
            }
            if let Some(t) = &o.title {
                m.insert("title".to_string(), t.clone().into());
            }
            if let Some(a) = o.age_seconds {
                m.insert("age_seconds".to_string(), a.into());
            }
            serde_json::Value::Object(m)
        })
        .collect();
    let recovered: Vec<_> = result
        .recovered
        .iter()
        .map(|r| {
            let mut m = serde_json::Map::new();
            m.insert("action".to_string(), r.action.into());
            m.insert("reason".to_string(), r.reason.clone().into());
            if let Some(i) = r.issue {
                m.insert("issue".to_string(), i.into());
            }
            if let Some(pr) = r.pr {
                m.insert("pr".to_string(), pr.into());
            }
            serde_json::Value::Object(m)
        })
        .collect();
    let watched: Vec<_> = result
        .watched
        .iter()
        .map(|w| {
            let mut m = serde_json::Map::new();
            m.insert("issue".to_string(), w.issue.into());
            m.insert("reason".to_string(), w.reason.into());
            m.insert("threshold_seconds".to_string(), w.threshold_seconds.into());
            if let Some(t) = &w.title {
                m.insert("title".to_string(), t.clone().into());
            }
            if let Some(a) = w.age_seconds {
                m.insert("age_seconds".to_string(), a.into());
            }
            serde_json::Value::Object(m)
        })
        .collect();

    let obj = serde_json::json!({
        "orphaned": orphaned,
        "recovered": recovered,
        "watched": watched,
        "total_orphaned": result.orphaned.len(),
        "total_recovered": result.recovered.len(),
        "total_watched": result.watched.len(),
        "recover_mode": result.recover_mode,
        // #5140: a consumer must be able to tell "assessed, found nothing"
        // from "could not assess" — `total_orphaned: 0` alone cannot.
        "assessment_failed": result.assessment_failed(),
        "assessment_errors": result.assessment_errors,
    });
    serde_json::to_string_pretty(&obj).unwrap_or_default()
}

/// Path to the spawn-loop claim-lock directory (re-exported for the CLI's
/// help text / diagnostics).
#[must_use]
pub fn locks_dir_for(repo_root: &Path) -> PathBuf {
    locks_dir(repo_root)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serial_test::serial;
    use tempfile::tempdir;

    fn absent_state() -> SpawnLoopState {
        SpawnLoopState::default()
    }

    /// Point the machine-level sweep journal at a path scoped to this test's
    /// tempdir (guaranteed not to exist), so `gather_liveness_evidence`
    /// doesn't pick up this dev machine's real `~/.loom/sweeps.json` as an
    /// unexpected evidence source. Mirrors the journal-path isolation pattern
    /// used by `claim_reconciliation`'s own tests.
    fn with_isolated_journal_path<T>(dir: &std::path::Path, f: impl FnOnce() -> T) -> T {
        let journal_path = dir.join("sweeps.json");
        std::env::set_var(crate::sweep_journal::JOURNAL_PATH_ENV, &journal_path);
        let result = f();
        std::env::remove_var(crate::sweep_journal::JOURNAL_PATH_ENV);
        result
    }

    #[test]
    #[serial]
    fn no_evidence_sources_means_unavailable() {
        let dir = tempdir().unwrap();
        let evidence = with_isolated_journal_path(dir.path(), || {
            gather_liveness_evidence(&absent_state(), dir.path())
        });
        assert!(!evidence.available, "fail-safe: no sources present must mean unavailable");
        assert!(evidence.live_issues.is_empty());
    }

    #[test]
    #[serial]
    fn locks_dir_contributes_evidence() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(locks_dir(dir.path()).join("issue-7")).unwrap();
        let evidence = with_isolated_journal_path(dir.path(), || {
            gather_liveness_evidence(&absent_state(), dir.path())
        });
        assert!(evidence.available);
        assert!(evidence.live_issues.contains(&7));
        assert!(evidence.sources.contains(&".loom/locks"));
    }

    #[test]
    #[serial]
    fn spawn_loop_state_present_contributes_evidence_even_if_empty() {
        let dir = tempdir().unwrap();
        let state = SpawnLoopState {
            running: Vec::new(),
            present: true,
        };
        let evidence =
            with_isolated_journal_path(dir.path(), || gather_liveness_evidence(&state, dir.path()));
        assert!(evidence.available);
        assert!(evidence.sources.contains(&"spawn-loop-state.json"));
    }

    #[test]
    fn check_untracked_building_emits_nothing_with_no_evidence() {
        let dir = tempdir().unwrap();
        let mut result = OrphanRecoveryResult::default();
        let evidence = LivenessEvidence::default();
        // No gh call is even attempted (fail-safe short-circuit) so this is
        // safe to run without a real `gh`/network — it must return with zero
        // orphans and zero watched entries.
        check_untracked_building(&evidence, &mut result, dir.path(), 600, false);
        assert!(result.orphaned.is_empty());
        assert!(result.watched.is_empty());
    }

    #[test]
    fn stale_heartbeat_with_dead_pid_is_orphaned() {
        let mut result = OrphanRecoveryResult::default();
        let state = SpawnLoopState {
            present: true,
            running: vec![super::super::spawn_loop_state::SpawnLoopTask {
                issue: 42,
                pid: 0, // pid 0 is never alive per sweep_registry::is_pid_alive
                last_heartbeat: Some(
                    (chrono::Utc::now() - chrono::Duration::seconds(600)).to_rfc3339(),
                ),
            }],
        };
        check_stale_heartbeats(&state, &mut result, 300);
        assert_eq!(result.orphaned.len(), 1);
        assert_eq!(result.orphaned[0].kind, "stale_heartbeat");
        assert_eq!(result.orphaned[0].issue, Some(42));
    }

    #[test]
    fn fresh_heartbeat_is_not_orphaned() {
        let mut result = OrphanRecoveryResult::default();
        let state = SpawnLoopState {
            present: true,
            running: vec![super::super::spawn_loop_state::SpawnLoopTask {
                issue: 42,
                pid: 0,
                last_heartbeat: Some(chrono::Utc::now().to_rfc3339()),
            }],
        };
        check_stale_heartbeats(&state, &mut result, 300);
        assert!(result.orphaned.is_empty());
    }

    #[test]
    fn format_result_human_reports_zero_orphans() {
        let result = OrphanRecoveryResult::default();
        assert_eq!(format_result_human(&result), "No orphaned tasks found");
    }

    /// #5140: a failed `gh issue list` must never be summarized as a clean
    /// bill of health.
    #[test]
    fn format_result_human_never_reports_all_clear_after_query_failure() {
        let mut result = OrphanRecoveryResult::default();
        result
            .assessment_errors
            .push("could not enumerate loom:building claims in /home/x: boom".to_string());
        let out = format_result_human(&result);
        assert!(
            !out.contains("No orphaned tasks found"),
            "false all-clear after a failed query: {out}"
        );
        assert!(out.contains("Could not assess orphaned tasks"), "{out}");
        assert!(out.contains("boom"), "{out}");
        assert!(result.assessment_failed());
    }

    #[test]
    fn format_result_json_flags_assessment_failure() {
        let mut result = OrphanRecoveryResult::default();
        result.assessment_errors.push("boom".to_string());
        let parsed: serde_json::Value = serde_json::from_str(&format_result_json(&result)).unwrap();
        assert_eq!(parsed["assessment_failed"], true);
        assert_eq!(parsed["total_orphaned"], 0);
        assert_eq!(parsed["assessment_errors"][0], "boom");
    }

    #[test]
    fn format_result_json_reports_success_when_assessment_completed() {
        let result = OrphanRecoveryResult::default();
        let parsed: serde_json::Value = serde_json::from_str(&format_result_json(&result)).unwrap();
        assert_eq!(parsed["assessment_failed"], false);
    }

    /// #5140: the failing `gh issue list` path records an assessment error
    /// instead of returning silently (which read as "nothing is orphaned").
    #[cfg(unix)]
    #[test]
    #[serial]
    fn check_untracked_building_records_error_when_gh_query_fails() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let fake_gh = bin.join("gh");
        std::fs::write(&fake_gh, "#!/bin/sh\necho 'boom' >&2\nexit 1\n").unwrap();
        std::fs::set_permissions(&fake_gh, std::fs::Permissions::from_mode(0o755)).unwrap();

        // Steered through `LOOM_GH_BIN` rather than `PATH` (#5511): prepending
        // to the process-wide `PATH` races with every concurrently-running
        // test's `Command` spawn, which showed up as spurious "git: No such
        // file or directory" failures elsewhere in the suite.
        std::env::set_var("LOOM_GH_BIN", &fake_gh);

        let mut result = OrphanRecoveryResult::default();
        let evidence = LivenessEvidence {
            available: true,
            sources: vec!["spawn-loop-state.json"],
            ..Default::default()
        };
        check_untracked_building(&evidence, &mut result, dir.path(), 600, false);

        std::env::remove_var("LOOM_GH_BIN");

        assert!(result.assessment_failed(), "query failure must be recorded");
        assert!(result.orphaned.is_empty());
        assert!(
            !format_result_human(&result).contains("No orphaned tasks found"),
            "must not report a false all-clear"
        );
    }

    // ------------------------------------------------------------------
    // #5511: an open linked PR is liveness evidence
    // ------------------------------------------------------------------

    /// A closes-graph GraphQL payload with the given node list.
    fn closes_graph(nodes: &str) -> String {
        format!(
            r#"{{"data":{{"repository":{{"issue":{{"closedByPullRequestsReferences":{{"nodes":[{nodes}]}}}}}}}}}}"#
        )
    }

    /// Install a fake `gh` (via `LOOM_GH_BIN`, never `PATH` — see
    /// [`super::gh`]'s `gh_bin`) that answers every call orphan recovery makes
    /// for one stale `loom:building` issue (#5511's fixture):
    ///
    /// - `issue list` -> exactly issue #5501, `loom:building`
    /// - `api repos/.../events` -> a 2020 `loom:building` timestamp, so every
    ///   staleness threshold is comfortably exceeded
    /// - `repo view` -> `rjwalters/loom` (owner/repo resolution for the probe)
    /// - `api graphql` -> `graphql_payload` with exit `graphql_exit` (the
    ///   closes-graph open-linked-PR probe)
    ///
    /// Returns a [`FakeGh`] guard that clears `LOOM_GH_BIN` on drop, carrying
    /// the path of a log file recording every `gh` invocation so a test can
    /// assert that `issue edit` was — or was NOT — reached.
    #[cfg(unix)]
    fn install_fake_gh(dir: &Path, graphql_payload: &str, graphql_exit: i32) -> FakeGh {
        use std::os::unix::fs::PermissionsExt;

        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let log = dir.join("gh-invocations.log");
        let script = format!(
            "#!/bin/sh\n\
             echo \"$@\" >> '{log}'\n\
             if [ \"$1\" = \"issue\" ] && [ \"$2\" = \"list\" ]; then\n\
             printf '%s' '[{{\"number\":5501,\"title\":\"live work\"}}]'\n\
             exit 0\n\
             fi\n\
             if [ \"$1\" = \"repo\" ]; then printf 'rjwalters/loom\\n'; exit 0; fi\n\
             if [ \"$1\" = \"api\" ] && [ \"$2\" = \"graphql\" ]; then\n\
             printf '%s' '{payload}'\n\
             exit {exit_code}\n\
             fi\n\
             if [ \"$1\" = \"api\" ]; then printf '2020-01-01T00:00:00Z\\n'; exit 0; fi\n\
             exit 0\n",
            log = log.display(),
            payload = graphql_payload,
            exit_code = graphql_exit,
        );
        let fake_gh = bin.join("gh");
        std::fs::write(&fake_gh, script).unwrap();
        std::fs::set_permissions(&fake_gh, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::env::set_var("LOOM_GH_BIN", &fake_gh);
        FakeGh { log }
    }

    /// RAII guard for [`install_fake_gh`]: unsets `LOOM_GH_BIN` on drop so a
    /// failing assertion cannot leak the fake into the next test.
    struct FakeGh {
        log: PathBuf,
    }

    impl FakeGh {
        /// Every `gh` invocation the fixture saw, one per line.
        fn calls(&self) -> String {
            std::fs::read_to_string(&self.log).unwrap_or_default()
        }
    }

    impl Drop for FakeGh {
        fn drop(&mut self) {
            std::env::remove_var("LOOM_GH_BIN");
        }
    }

    /// Evidence shaped like the #5501 incident: a liveness source exists (so the
    /// #3651 short-circuit does not fire), but the issue is in none of the live
    /// sets and has no journal record -> `no_spawn_loop_entry`, past threshold.
    fn evidence_without_the_issue() -> LivenessEvidence {
        LivenessEvidence {
            available: true,
            sources: vec!["spawn-loop-state.json"],
            ..Default::default()
        }
    }

    /// (a) The #5501 regression: an issue whose only sign of life is an OPEN
    /// linked PR must never be flagged orphaned, however stale its label and
    /// however absent its claim lock / journal entry.
    #[cfg(unix)]
    #[test]
    #[serial]
    fn open_linked_pr_is_never_flagged_orphaned() {
        let dir = tempdir().unwrap();
        let _gh =
            install_fake_gh(dir.path(), &closes_graph(r#"{"number":5507,"state":"OPEN"}"#), 0);

        let mut result = OrphanRecoveryResult::default();
        check_untracked_building(
            &evidence_without_the_issue(),
            &mut result,
            dir.path(),
            600,
            false,
        );

        assert!(
            result.orphaned.is_empty(),
            "an issue with an open linked PR must not be orphaned: {:?}",
            result.orphaned
        );
        assert!(!result.assessment_failed());
    }

    /// (a, defense in depth) `recover_issue` is a `pub` entry point that can be
    /// called without the flagging pass — it must refuse the label flip on its
    /// own when an open linked PR exists.
    #[cfg(unix)]
    #[test]
    #[serial]
    fn recover_issue_refuses_to_reset_an_issue_with_an_open_linked_pr() {
        let dir = tempdir().unwrap();
        let gh = install_fake_gh(dir.path(), &closes_graph(r#"{"number":5507,"state":"OPEN"}"#), 0);

        let mut result = OrphanRecoveryResult::default();
        recover_issue(dir.path(), 5501, "no_spawn_loop_entry", &mut result, 600);

        let calls = gh.calls();
        assert!(
            !calls.contains("issue edit"),
            "recover_issue must not flip labels while a linked PR is open; gh calls:\n{calls}"
        );
        assert!(result.recovered.is_empty());
    }

    /// (b) Unchanged behavior: a verified absence of any linked PR still lets
    /// recovery proceed exactly as before.
    #[cfg(unix)]
    #[test]
    #[serial]
    fn no_linked_pr_still_orphans_and_resets() {
        let dir = tempdir().unwrap();
        let gh = install_fake_gh(dir.path(), &closes_graph(""), 0);

        let mut result = OrphanRecoveryResult::default();
        check_untracked_building(
            &evidence_without_the_issue(),
            &mut result,
            dir.path(),
            600,
            false,
        );
        let mut recovery = OrphanRecoveryResult::default();
        recover_issue(dir.path(), 5501, "no_spawn_loop_entry", &mut recovery, 600);

        assert_eq!(result.orphaned.len(), 1, "{:?}", result.orphaned);
        assert_eq!(result.orphaned[0].issue, Some(5501));
        assert_eq!(result.orphaned[0].reason, "no_spawn_loop_entry");
        assert!(
            gh.calls().contains("issue edit"),
            "a verified absence of a linked PR must still reset the label"
        );
        assert!(recovery
            .recovered
            .iter()
            .any(|r| r.action == "reset_issue_label"));
    }

    /// (c) Fail-safe: a probe failure (forge outage / wedged `gh`) is NOT a
    /// verified absence, so it must block the reset rather than greenlight it.
    #[cfg(unix)]
    #[test]
    #[serial]
    fn pr_probe_failure_blocks_recovery() {
        let dir = tempdir().unwrap();
        let gh = install_fake_gh(dir.path(), "gh: rate limit exceeded", 1);

        let mut result = OrphanRecoveryResult::default();
        check_untracked_building(
            &evidence_without_the_issue(),
            &mut result,
            dir.path(),
            600,
            false,
        );
        let mut recovery = OrphanRecoveryResult::default();
        recover_issue(dir.path(), 5501, "no_spawn_loop_entry", &mut recovery, 600);

        assert!(
            result.orphaned.is_empty(),
            "an unverifiable PR probe must fail toward ALIVE (#3651/#5511)"
        );
        assert!(
            !gh.calls().contains("issue edit"),
            "an unverifiable PR probe must not flip labels"
        );
        assert!(recovery.recovered.is_empty());
    }

    /// (d) Only `state == "OPEN"` counts. A MERGED PR still comes back from the
    /// closes-graph even with `includeClosedPrs:false`, so treating any linked
    /// PR as "open" would wedge orphan recovery forever on every issue whose PR
    /// already merged.
    #[cfg(unix)]
    #[test]
    #[serial]
    fn merged_linked_pr_does_not_block_recovery() {
        let dir = tempdir().unwrap();
        let gh =
            install_fake_gh(dir.path(), &closes_graph(r#"{"number":5507,"state":"MERGED"}"#), 0);

        let mut result = OrphanRecoveryResult::default();
        check_untracked_building(
            &evidence_without_the_issue(),
            &mut result,
            dir.path(),
            600,
            false,
        );
        let mut recovery = OrphanRecoveryResult::default();
        recover_issue(dir.path(), 5501, "no_spawn_loop_entry", &mut recovery, 600);

        assert_eq!(
            result.orphaned.len(),
            1,
            "a MERGED linked PR is not an open PR: {:?}",
            result.orphaned
        );
        assert!(gh.calls().contains("issue edit"));
        assert!(recovery
            .recovered
            .iter()
            .any(|r| r.action == "reset_issue_label"));
    }

    #[test]
    fn format_result_json_round_trips_totals() {
        let mut result = OrphanRecoveryResult::default();
        result.orphaned.push(OrphanEntry {
            kind: "untracked_building",
            issue: Some(1),
            pr: None,
            pid: None,
            title: Some("t".to_string()),
            reason: "no_spawn_loop_entry".to_string(),
            age_seconds: None,
        });
        let json = format_result_json(&result);
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["total_orphaned"], 1);
    }

    // ------------------------------------------------------------------
    // #6167: stale PR-side `loom:reviewing`/`loom:treating` claim recovery
    // ------------------------------------------------------------------

    /// Install a fake `gh` (via `LOOM_GH_BIN`) that answers every call
    /// [`check_stale_pr_claims`]'s underlying
    /// `claim_reconciliation::forge::reconcile_pr_claims_report` makes for
    /// one PR: `pr list` (any `--label`, mirroring the identical simplifying
    /// assumption `claim_reconciliation.rs`'s own `write_fake_gh_pr` fixture
    /// makes — both the `loom:reviewing` and `loom:treating` passes see the
    /// same fixture PR), `pr view` (labels for the safety-net backfill
    /// check), and a catch-all `exit 0` for the `api .../timeline` /
    /// `api .../comments` freshness probes (so `decide_pr` falls back to
    /// `updatedAt` — deliberate, keeps this fixture from needing to model
    /// the claim-labeled-at/comment freshness signal).
    #[cfg(unix)]
    fn install_fake_gh_pr(
        dir: &Path,
        pr_number: u32,
        updated_at: &str,
        head_ref_name: &str,
        extra_labels: &[&str],
    ) -> FakeGh {
        use std::os::unix::fs::PermissionsExt;

        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let log = dir.join("gh-invocations-pr.log");
        let labels_json = extra_labels
            .iter()
            .map(|l| format!(r#"{{"name":"{l}"}}"#))
            .collect::<Vec<_>>()
            .join(",");
        let script = format!(
            "#!/usr/bin/env bash\n\
             printf '%s\\n' \"$*\" >> '{log}'\n\
             if [ \"$1\" = \"pr\" ] && [ \"$2\" = \"list\" ]; then\n\
             echo '[{{\"number\":{pr_number},\"updatedAt\":\"{updated_at}\",\"headRefName\":\"{head_ref_name}\"}}]'\n\
             exit 0\n\
             fi\n\
             if [ \"$1\" = \"pr\" ] && [ \"$2\" = \"view\" ]; then\n\
             echo '{{\"labels\":[{labels_json}]}}'\n\
             exit 0\n\
             fi\n\
             exit 0\n",
            log = log.display(),
        );
        let fake_gh = bin.join("gh");
        std::fs::write(&fake_gh, script).unwrap();
        std::fs::set_permissions(&fake_gh, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::env::set_var("LOOM_GH_BIN", &fake_gh);
        FakeGh { log }
    }

    /// Dry-run (`recover=false`) reports a stale `loom:reviewing` claim
    /// without issuing any mutating `gh` call (AC1 detection + the
    /// `recover-orphans` dry-run contract).
    #[cfg(unix)]
    #[test]
    #[serial]
    fn check_stale_pr_claims_dry_run_reports_without_mutating() {
        let dir = tempdir().unwrap();
        let old = (chrono::Utc::now() - chrono::Duration::minutes(90)).to_rfc3339();
        let gh = install_fake_gh_pr(dir.path(), 700, &old, "some-random-branch", &[]);

        let mut result = OrphanRecoveryResult::default();
        with_isolated_journal_path(dir.path(), || {
            check_stale_pr_claims(dir.path(), &mut result, false);
        });

        assert!(
            result
                .orphaned
                .iter()
                .any(|o| o.kind == "stale_reviewing_pr" && o.pr == Some(700)),
            "expected a stale_reviewing_pr orphan for PR #700: {:?}",
            result.orphaned
        );
        assert!(
            result.recovered.is_empty(),
            "dry-run must never reclaim: {:?}",
            result.recovered
        );
        assert!(
            !gh.calls().contains("--remove-label"),
            "dry-run must not remove any claim label: {}",
            gh.calls()
        );
    }

    /// `recover=true` reclaims a stale `loom:reviewing` claim and (no state
    /// label present) backfills `loom:review-requested`, mirroring
    /// `forge::reclaim_pr`'s safety net (AC1 recovery).
    #[cfg(unix)]
    #[test]
    #[serial]
    fn check_stale_pr_claims_recover_reclaims_and_backfills() {
        let dir = tempdir().unwrap();
        let old = (chrono::Utc::now() - chrono::Duration::minutes(90)).to_rfc3339();
        let gh = install_fake_gh_pr(dir.path(), 701, &old, "some-random-branch", &[]);

        let mut result = OrphanRecoveryResult::default();
        with_isolated_journal_path(dir.path(), || {
            check_stale_pr_claims(dir.path(), &mut result, true);
        });

        assert!(
            result
                .recovered
                .iter()
                .any(|r| r.action == "reclaim_pr_claim" && r.pr == Some(701)),
            "expected a reclaim_pr_claim recovery entry for PR #701: {:?}",
            result.recovered
        );
        let calls = gh.calls();
        assert!(
            calls.contains("pr edit 701 --remove-label loom:reviewing"),
            "expected loom:reviewing to be removed from #701; got: {calls:?}"
        );
        assert!(
            calls.contains("pr edit 701 --add-label loom:review-requested"),
            "expected the safety net to add loom:review-requested to #701; got: {calls:?}"
        );
    }

    /// A fresh claim (age well under the staleness threshold) must never be
    /// reclaimed — the never-strip-a-live-worker discipline (AC2), reusing
    /// the identical `decide_pr` liveness/staleness logic the daemon
    /// backstop and judge.md's own check already share.
    #[cfg(unix)]
    #[test]
    #[serial]
    fn check_stale_pr_claims_never_reclaims_a_fresh_claim() {
        let dir = tempdir().unwrap();
        let fresh = chrono::Utc::now().to_rfc3339();
        let gh = install_fake_gh_pr(
            dir.path(),
            702,
            &fresh,
            "some-random-branch",
            &["loom:review-requested"],
        );

        let mut result = OrphanRecoveryResult::default();
        with_isolated_journal_path(dir.path(), || {
            check_stale_pr_claims(dir.path(), &mut result, true);
        });

        assert!(
            result.orphaned.is_empty(),
            "a fresh PR-side claim must not be flagged orphaned: {:?}",
            result.orphaned
        );
        assert!(
            result.recovered.is_empty(),
            "a fresh PR-side claim must never be reclaimed: {:?}",
            result.recovered
        );
        assert!(
            !gh.calls().contains("--remove-label"),
            "no claim label should have been removed: {}",
            gh.calls()
        );
    }
}
