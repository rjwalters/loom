//! Daemon-side periodic worktree reaper — restores CLAUDE.md's "auto-removed
//! when their PR merges" contract for merges the merge script never observed
//! (issue #4876).
//!
//! # Why this exists
//!
//! Before this module, worktree auto-removal had exactly **one** trigger:
//! `defaults/scripts/merge-pr.sh`'s `_remove_loom_worktree()`, a *synchronous
//! side effect* of that script running. That trigger only fires when
//!
//! 1. the merge goes through `merge-pr.sh` (not the forge UI, `gh api`, or an
//!    auto-merge queue), **and**
//! 2. it runs on the host that holds the worktree, **and**
//! 3. it runs in the checkout that holds the worktree (`find_main_repo_root()`
//!    walks up from the invoking shell's cwd — there is no host-wide scan).
//!
//! A three-host fleet violates (2) routinely: worker-1 builds issue #N, studio
//! merges the PR, and worker-1's worktree has nobody to observe the merge.
//! Nothing in `daemon_service.rs` covered the gap — none of the registered
//! interval tasks (claim reconciliation, sweep reaper/watchdog, epic
//! supervisor, work finder, main-health gate, token-ranking refresh,
//! heartbeat, role runner) called into [`crate::worktree_ops::clean`]. The
//! observed result: 44 stale worktrees on worker-1 (81% disk, 54G under
//! `.loom/`) and 35 on studio, 16 of which were merged-PR worktrees eligible
//! for removal on *each* host.
//!
//! This loop closes it: every host independently reaps **its own** worktrees
//! from *forge state* (is the issue closed? did its PR merge? has the grace
//! period elapsed?), so it does not matter which host performed the merge, or
//! whether a merge script ran at all.
//!
//! # Safety: strictly a subset of what `clean --safe` would remove
//!
//! The decision is [`crate::worktree_ops::clean::classify_worktree`] — the
//! *same* function the interactive `loom-daemon clean` CLI uses, so there is
//! no second, drifting copy of the gates. The reaper runs it with
//! `safe: true, force: false`, which means every one of these preserves a
//! worktree: a live spawn-loop task or claim-lock, a `.loom-in-use` marker, a
//! process whose cwd is inside it, an editable pip install pointing into it,
//! an open issue, an open/unmerged/absent PR, a merge inside the grace period,
//! and any uncommitted change.
//!
//! It adds **one gate the CLI does not have**:
//! [`CleanOptions::require_managed_sentinel`]. An unattended remover must honor
//! "user-provisioned worktrees are never removed" — nobody is at the keyboard
//! to say no — so a worktree without the `.loom-managed` sentinel is skipped
//! even when every other gate passes.
//!
//! Net: the reaper can only ever remove a strict subset of what an operator
//! running `loom-daemon clean --safe` would remove, which is also what makes
//! missed cleanups idempotently recoverable — a manual `clean --safe` after
//! the loop already ran simply finds nothing to do.
//!
//! # REST, not GraphQL
//!
//! The probes use [`clean::check_pr_merged_rest`] /
//! `gh::issue_state_rest` rather than the GraphQL-backed
//! `gh issue view` / `gh pr list`. GraphQL quota exhaustion under concurrent
//! agents is a live failure mode; an unattended cadence prober must not compete
//! with interactive agents for the scarcer pool. A REST probe failure resolves
//! to `UNKNOWN`, which is a *skip*, never a removal.
//!
//! # Disk headroom
//!
//! Each pass also probes [`crate::disk_headroom::worktree_root_free_gb`] and
//! logs a `warn!` when free space on the worktree-root volume is below the
//! configured floor — the metric the dashboard already renders but nothing
//! acted on. That is the signal that turns "the disk filled and sweeps started
//! failing with unrelated build errors" into "this host is low on disk".
//!
//! # Default-on
//!
//! Like [`crate::token_ranking_refresh`] and unlike the work finder /
//! main-health gate, this loop is **default-on**: it restores a behavior
//! CLAUDE.md already documents as the contract, and its absence is a
//! slow-motion outage. Opt out with `LOOM_WORKTREE_REAPER=0` or
//! `autonomous.worktreeReaper.enabled=false`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::Utc;

use crate::workspace_registry::WorkspaceRegistry;
use crate::worktree_ops::clean::{
    self, CleanOptions, WorktreeDecision, WorktreeProbes, DEFAULT_GRACE_PERIOD_SECS,
};

// ============================================================================
// Constants
// ============================================================================

/// Master on/off env override. Default-on (see module docs): set to
/// `0`/`false`/`no`/`off` to disable, `1`/`true`/`yes`/`on` to force-enable
/// even when config disables it.
pub const WORKTREE_REAPER_ENABLE_ENV: &str = "LOOM_WORKTREE_REAPER";

/// Env override for the reap cadence (seconds).
pub const WORKTREE_REAPER_INTERVAL_ENV: &str = "LOOM_WORKTREE_REAPER_INTERVAL_SECS";

/// Env override for the low-disk warning floor (GB of free space on the
/// worktree-root volume).
pub const WORKTREE_REAPER_DISK_WARN_ENV: &str = "LOOM_WORKTREE_REAPER_DISK_WARN_GB";

/// Default reap cadence (15 minutes). Slow enough that the forge probes are
/// negligible next to normal sweep traffic, fast enough that a merged
/// worktree's ~1G of build artifacts is reclaimed the same hour.
pub const DEFAULT_WORKTREE_REAPER_INTERVAL_SECS: u64 = 900;

/// Default low-disk warning floor (GB). Below this, a host is close enough to
/// full that a sweep's build can fail with a confusing unrelated error.
pub const DEFAULT_DISK_WARN_FREE_GB: u64 = 20;

// ============================================================================
// Config (.loom/config.json → autonomous.worktreeReaper)
// ============================================================================

/// The subset of `.loom/config.json → autonomous.worktreeReaper` this module
/// consumes. Every field is `Option` so an absent key falls through to the
/// env-var / built-in-default resolution — precedence **env > config >
/// default**, matching every other `autonomous.*` surface.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorktreeReaperConfig {
    /// `autonomous.worktreeReaper.enabled` (default **true**).
    pub enabled: Option<bool>,
    /// `autonomous.worktreeReaper.intervalSecs` (a zero/invalid value drops to
    /// `None`).
    pub interval_secs: Option<u64>,
    /// `autonomous.worktreeReaper.gracePeriodSecs` — how long after a PR merge
    /// a worktree becomes eligible. Same knob as `clean --safe`'s
    /// `--grace-period`.
    pub grace_period_secs: Option<i64>,
    /// `autonomous.worktreeReaper.diskWarnFreeGb` — free-GB floor below which
    /// the pass logs a low-disk warning.
    pub disk_warn_free_gb: Option<u64>,
}

/// Read `.loom/config.json → autonomous.worktreeReaper`, soft-failing every
/// field to `None` (env/default resolution) on a missing file, malformed JSON,
/// or a missing `autonomous` / `worktreeReaper` block.
#[must_use]
pub fn read_worktree_reaper_config(repo_root: &Path) -> WorktreeReaperConfig {
    let effective = crate::config_resolver::resolve_effective_config(repo_root);
    let Some(block) = crate::config_resolver::get_path(&effective, "autonomous.worktreeReaper")
    else {
        return WorktreeReaperConfig::default();
    };

    WorktreeReaperConfig {
        enabled: block.get("enabled").and_then(serde_json::Value::as_bool),
        interval_secs: block
            .get("intervalSecs")
            .and_then(serde_json::Value::as_u64)
            .filter(|&s| s > 0),
        grace_period_secs: block
            .get("gracePeriodSecs")
            .and_then(serde_json::Value::as_i64)
            .filter(|&s| s >= 0),
        disk_warn_free_gb: block
            .get("diskWarnFreeGb")
            .and_then(serde_json::Value::as_u64),
    }
}

/// Resolve whether the loop runs — precedence **env > config > default(true)**.
#[must_use]
pub fn resolve_enabled(config: &WorktreeReaperConfig) -> bool {
    if let Ok(v) = std::env::var(WORKTREE_REAPER_ENABLE_ENV) {
        return matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on");
    }
    config.enabled.unwrap_or(true)
}

/// Resolve the reap cadence — precedence **env > config > default**. A zero or
/// unparseable env value falls through rather than producing a busy loop.
#[must_use]
pub fn resolve_interval(config: &WorktreeReaperConfig) -> Duration {
    std::env::var(WORKTREE_REAPER_INTERVAL_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&s| s > 0)
        .or(config.interval_secs)
        .map_or_else(
            || Duration::from_secs(DEFAULT_WORKTREE_REAPER_INTERVAL_SECS),
            Duration::from_secs,
        )
}

/// Resolve the post-merge grace period — precedence **config > default**
/// (there is no dedicated env var; `clean --safe --grace-period` covers the
/// manual path).
#[must_use]
pub fn resolve_grace_period(config: &WorktreeReaperConfig) -> i64 {
    config
        .grace_period_secs
        .unwrap_or(DEFAULT_GRACE_PERIOD_SECS)
}

/// Resolve the low-disk warning floor — precedence **env > config > default**.
#[must_use]
pub fn resolve_disk_warn_free_gb(config: &WorktreeReaperConfig) -> u64 {
    std::env::var(WORKTREE_REAPER_DISK_WARN_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .or(config.disk_warn_free_gb)
        .unwrap_or(DEFAULT_DISK_WARN_FREE_GB)
}

/// The [`CleanOptions`] the reaper always uses: `--safe` semantics, never
/// `--force`, never a dry run, and the sentinel gate the CLI does not apply.
#[must_use]
pub fn reaper_clean_options(grace_period_secs: i64) -> CleanOptions {
    CleanOptions {
        dry_run: false,
        deep: false,
        force: false,
        safe: true,
        grace_period_secs,
        worktrees_only: true,
        branches_only: false,
        tmux_only: false,
        require_managed_sentinel: true,
    }
}

// ============================================================================
// One reap pass
// ============================================================================

/// What one reap pass over one repo did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReapReport {
    /// `issue-<N>` worktree directories examined.
    pub scanned: usize,
    /// Issue numbers whose worktrees were removed.
    pub removed: Vec<u32>,
    /// Issue numbers whose removal was attempted but failed.
    pub failed: Vec<u32>,
    /// Worktrees a safety gate preserved, keyed by the gate's own wording.
    pub skipped: Vec<(u32, String)>,
    /// Free GB on the worktree-root volume, or `None` when unmeasurable
    /// (#4164 — unknown is never treated as zero).
    pub free_gb: Option<u64>,
}

impl ReapReport {
    /// A compact one-line summary for the daemon log.
    #[must_use]
    pub fn summary(&self) -> String {
        format!(
            "scanned={} removed={} failed={} skipped={}",
            self.scanned,
            self.removed.len(),
            self.failed.len(),
            self.skipped.len()
        )
    }
}

/// Human-readable reason a decision preserved a worktree. `None` for
/// [`WorktreeDecision::Remove`].
#[must_use]
fn skip_reason(decision: &WorktreeDecision) -> Option<String> {
    match decision {
        WorktreeDecision::Remove => None,
        WorktreeDecision::SkipInUse(reason) => Some(reason.clone()),
        WorktreeDecision::SkipEditable(pkgs) => Some(format!("editable pip install(s): {pkgs}")),
        WorktreeDecision::SkipUnmanaged => {
            Some("no .loom-managed sentinel (user-provisioned)".to_string())
        }
        WorktreeDecision::SkipIssueNotClosed(state) => Some(format!("issue is {state}")),
        WorktreeDecision::SkipGrace(remaining) => {
            Some(format!("grace period not passed ({remaining}s remaining)"))
        }
        WorktreeDecision::SkipUncommitted => Some("uncommitted changes".to_string()),
        WorktreeDecision::SkipNotMerged(reason) => Some(reason.clone()),
        WorktreeDecision::SkipPrOpen => Some("PR still open".to_string()),
        WorktreeDecision::SkipUnknownPrStatus => Some("PR status unknown".to_string()),
        // Unreachable with the reaper's `safe: true` options, but a
        // non-`--safe` caller must never be interpreted as "remove it".
        WorktreeDecision::ConfirmClosedIssue => {
            Some("needs confirmation (non-safe mode)".to_string())
        }
    }
}

/// Enumerate `issue-<N>` worktrees under `repo_root`'s worktree root, classify
/// each with [`clean::classify_worktree`], and remove the eligible ones via
/// `remove`.
///
/// The probes and the remover are injected so the whole pass is unit-testable
/// without a forge, a process table, or real `git worktree` state — production
/// wiring lives in [`reap_repo`].
pub fn reap_worktrees(
    repo_root: &Path,
    opts: &CleanOptions,
    probes: &WorktreeProbes<'_>,
    remove: &dyn Fn(&Path, u32) -> bool,
) -> ReapReport {
    let mut report = ReapReport::default();

    let worktrees_dir = crate::worktree_root::worktree_root(repo_root);
    let Ok(entries) = std::fs::read_dir(&worktrees_dir) else {
        // No worktree root yet (or unreadable) — nothing to reap, not an error.
        return report;
    };

    let mut worktree_dirs: Vec<_> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .collect();
    worktree_dirs.sort_by_key(std::fs::DirEntry::path);

    for entry in worktree_dirs {
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(issue_num) = crate::worktree_ops::naming::issue_from_worktree(&name) else {
            continue;
        };
        report.scanned += 1;

        let worktree_path = entry.path().canonicalize().unwrap_or_else(|_| entry.path());
        let decision = clean::classify_worktree(&worktree_path, issue_num, opts, probes);

        if let Some(reason) = skip_reason(&decision) {
            report.skipped.push((issue_num, reason));
            continue;
        }

        if remove(&worktree_path, issue_num) {
            report.removed.push(issue_num);
        } else {
            report.failed.push(issue_num);
        }
    }

    report
}

// ============================================================================
// Artifact reclaim pass (AC3 of #5177 — #5187)
// ============================================================================

/// What one artifact-reclaim pass over one repo did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReclaimReport {
    /// `issue-<N>` worktree directories examined.
    pub scanned: usize,
    /// Issue number -> reclaimed top-level directory names (e.g. `"target"`,
    /// `"node_modules"`), for worktrees that had at least one.
    pub reclaimed: Vec<(u32, Vec<String>)>,
    /// Worktrees this pass left untouched, keyed by why.
    pub skipped: Vec<(u32, String)>,
}

impl ReclaimReport {
    /// A compact one-line summary for the daemon log.
    #[must_use]
    pub fn summary(&self) -> String {
        format!(
            "scanned={} reclaimed={} skipped={}",
            self.scanned,
            self.reclaimed.len(),
            self.skipped.len()
        )
    }
}

/// Reclaim `target/`/`node_modules/`/... from every **kept** worktree under
/// `repo_root`'s worktree root — a worktree the removal pass ([`reap_worktrees`])
/// is *not* deleting, and that nothing is actively using.
///
/// Shares [`clean::classify_worktree`] with [`reap_worktrees`] (same `opts`,
/// same `probes`) rather than inventing a parallel eligibility check, so this
/// pass can never disagree with the removal gates about what "in use" means:
///
/// - [`WorktreeDecision::Remove`]: skipped here. [`reap_worktrees`] deletes
///   the whole worktree (artifacts included), so reclaiming from it
///   separately would either race a directory about to vanish or, if the
///   removal already ran this tick, simply find nothing (the directory is
///   already gone).
/// - [`WorktreeDecision::SkipInUse`] (live spawn-loop claim-lock, a
///   `.loom-in-use` marker, or an active process): skipped — this is the one
///   outcome the acceptance criteria say must never be touched.
/// - Every other skip reason (open issue, open/unmerged/absent PR, grace
///   period, uncommitted changes, unmanaged, editable install, unknown PR
///   status): a **kept, idle** worktree — reclaim its build artifacts via
///   [`clean::reclaim_worktree_artifacts`].
pub fn reclaim_kept_worktree_artifacts(
    repo_root: &Path,
    opts: &CleanOptions,
    probes: &WorktreeProbes<'_>,
    dry_run: bool,
) -> ReclaimReport {
    let mut report = ReclaimReport::default();

    let worktrees_dir = crate::worktree_root::worktree_root(repo_root);
    let Ok(entries) = std::fs::read_dir(&worktrees_dir) else {
        // No worktree root yet (or unreadable) — nothing to reclaim, not an error.
        return report;
    };

    let mut worktree_dirs: Vec<_> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .collect();
    worktree_dirs.sort_by_key(std::fs::DirEntry::path);

    for entry in worktree_dirs {
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(issue_num) = crate::worktree_ops::naming::issue_from_worktree(&name) else {
            continue;
        };
        report.scanned += 1;

        let worktree_path = entry.path().canonicalize().unwrap_or_else(|_| entry.path());
        let decision = clean::classify_worktree(&worktree_path, issue_num, opts, probes);

        match &decision {
            WorktreeDecision::Remove => {
                report.skipped.push((
                    issue_num,
                    "eligible for full removal (handled by the directory reap pass)".to_string(),
                ));
                continue;
            }
            WorktreeDecision::SkipInUse(reason) => {
                report.skipped.push((issue_num, reason.clone()));
                continue;
            }
            _ => {}
        }

        let reclaimed = clean::reclaim_worktree_artifacts(&worktree_path, dry_run);
        if reclaimed.is_empty() {
            continue;
        }
        report
            .reclaimed
            .push((issue_num, reclaimed.into_iter().map(|a| a.name).collect()));
    }

    report
}

/// Log an artifact-reclaim pass's outcome, mirroring [`log_report`]'s shape.
pub fn log_reclaim_report(repo_root: &Path, report: &ReclaimReport) {
    if report.reclaimed.is_empty() {
        log::debug!(
            "worktree_reaper: {} artifact reclaim: nothing to reclaim ({})",
            repo_root.display(),
            report.summary()
        );
    } else {
        log::info!(
            "worktree_reaper: {} artifact reclaim: {} reclaimed={:?}",
            repo_root.display(),
            report.summary(),
            report.reclaimed
        );
    }
    for (issue, reason) in &report.skipped {
        log::debug!(
            "worktree_reaper: {} artifact reclaim skipping issue-{issue}: {reason}",
            repo_root.display()
        );
    }
}

/// Run one production reap pass over `repo_root`.
///
/// Wires the real probes (REST-first forge lookups — see the module docs) and
/// the real remover ([`clean::cleanup_worktree`]), then layers the disk-headroom
/// probe on top.
pub fn reap_repo(repo_root: &Path, config: &WorktreeReaperConfig) -> ReapReport {
    let opts = reaper_clean_options(resolve_grace_period(config));
    let active_issues = crate::worktree_ops::liveness::active_spawn_loop_issues(repo_root);

    // Resolved once per pass (one REST call), not once per worktree.
    let owner = clean::repo_owner_rest(repo_root);

    let issue_state_fn = |n: u32| crate::worktree_ops::gh::issue_state_rest(repo_root, n);
    let pr_status_fn = |n: u32| match owner.as_deref() {
        Some(owner) => clean::check_pr_merged_rest(repo_root, owner, n),
        // No owner ⇒ no REST head filter is constructible; fall back to the
        // GraphQL-backed probe rather than silently reporting Unknown forever.
        None => clean::check_pr_merged(repo_root, n),
    };

    let probes =
        clean::production_probes(&active_issues, &issue_state_fn, &pr_status_fn, Utc::now());
    // `cleanup_worktree` reports the underlying cause of a failed removal
    // (#4877). `reap_worktrees` only needs the removed/failed bit, so name the
    // cause in the daemon log here rather than discarding it — the reaper is
    // unattended and its log is the only place an operator can see why.
    let remover =
        |path: &Path, issue: u32| match clean::cleanup_worktree(repo_root, path, issue, false) {
            Ok(()) => true,
            Err(cause) => {
                log::warn!(
                    "worktree_reaper: {} could not remove issue-{issue} ({}): {cause}",
                    repo_root.display(),
                    path.display()
                );
                false
            }
        };

    let mut report = reap_worktrees(repo_root, &opts, &probes, &remover);
    report.free_gb = crate::disk_headroom::worktree_root_free_gb(repo_root);

    // AC3 of #5177 (#5187): worktrees this pass just decided to *keep*
    // (everything except `Remove`) may still be sitting on GBs of
    // regenerable `target/`/`node_modules/` — reclaim those without
    // removing the worktree. Reuses the exact same probes/decision so
    // eligibility never drifts from the removal gates above.
    let reclaim_report = reclaim_kept_worktree_artifacts(repo_root, &opts, &probes, false);
    log_reclaim_report(repo_root, &reclaim_report);

    report
}

/// Log a pass's outcome, including the low-disk warning (the acceptance
/// criterion that a host approaching a disk threshold surfaces it).
pub fn log_report(repo_root: &Path, report: &ReapReport, warn_below_gb: u64) {
    if report.removed.is_empty() && report.failed.is_empty() {
        log::debug!(
            "worktree_reaper: {} nothing to reap ({})",
            repo_root.display(),
            report.summary()
        );
    } else {
        log::info!(
            "worktree_reaper: {} {} removed={:?}",
            repo_root.display(),
            report.summary(),
            report.removed
        );
    }
    for (issue, reason) in &report.skipped {
        log::debug!("worktree_reaper: {} preserving issue-{issue}: {reason}", repo_root.display());
    }
    if !report.failed.is_empty() {
        log::warn!(
            "worktree_reaper: {} could not remove worktrees for {:?} — a manual \
             `loom-daemon clean --safe` will retry idempotently",
            repo_root.display(),
            report.failed
        );
    }
    match report.free_gb {
        Some(free) if free < warn_below_gb => log::warn!(
            "worktree_reaper: {} LOW DISK — {free}G free on the worktree-root volume \
             (floor {warn_below_gb}G, {} worktree(s) still present). Sweeps on this host \
             will start failing with unrelated build errors before they report \
             'out of space'; run `loom-daemon clean --safe --deep` or raise \
             autonomous.worktreeReaper.diskWarnFreeGb if this is expected.",
            repo_root.display(),
            report.scanned - report.removed.len()
        ),
        Some(free) => log::debug!(
            "worktree_reaper: {} {free}G free on the worktree-root volume",
            repo_root.display()
        ),
        None => log::debug!(
            "worktree_reaper: {} worktree-root free space unmeasurable — skipping the \
             low-disk check (unknown != zero)",
            repo_root.display()
        ),
    }
}

// ============================================================================
// Runtime wiring
// ============================================================================

/// Spawn the **multi-workspace** worktree reaper loop on the shared daemon
/// runtime (mirrors [`crate::token_ranking_refresh::spawn_multi_token_ranking_refresh_task`]).
///
/// Every `interval` it re-reads [`WorkspaceRegistry::effective_roots`] against
/// `fallback_root` (an empty registry ⇒ the single `fallback_root`) and reaps
/// **every registered repo**, each gated by that repo's own
/// `autonomous.worktreeReaper.enabled`. This is what makes cleanup independent
/// of "the daemon's attached workspace": a host whose daemon is attached to
/// `~/GitHub/anvil` still reaps `~/GitHub/loom` as long as that repo is a
/// registered workspace.
///
/// Passes run **sequentially** per tick (like the health gate and the ranking
/// refresh): each one shells out to `gh` and `git`, and bursting several repos'
/// forge probes at once buys nothing.
///
/// The first tick is **skipped** (unlike the ranking refresh): a reap has a
/// destructive side effect, and deferring it one cadence lets a just-restarted
/// daemon's own in-flight sweeps re-establish their `.loom-in-use` markers and
/// claim-locks before anything is evaluated for removal.
pub fn spawn_multi_worktree_reaper_task(
    fallback_root: PathBuf,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    log::info!(
        "worktree_reaper: starting multi-workspace loop (interval={}s)",
        interval.as_secs()
    );
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // Skip the immediate first tick — see the doc comment.
        ticker.tick().await;

        loop {
            ticker.tick().await;

            let roots = WorkspaceRegistry::load_default()
                .unwrap_or_else(|e| {
                    log::warn!(
                        "worktree_reaper: could not load workspace registry ({e}); using fallback"
                    );
                    WorkspaceRegistry::default()
                })
                .effective_roots(&fallback_root);

            for root in roots {
                // Orphaned *process* trees first (#5110): a runaway driver
                // inside an issue worktree pins the worktree's "active
                // process(es) using worktree" gate, so reaping the processes
                // before the directory pass is also what lets the directory
                // pass make progress on the same tick.
                reap_orphan_processes_for(&root).await;

                // Primary-checkout reap (#5268): answers a different question
                // about the SAME registered root ("is the primary checkout's
                // own HEAD parked on a dead branch?") rather than about its
                // `.loom/worktrees/<n>` entries, but rides the same tick for
                // the same reason the two passes above already share one —
                // both `git`/forge probes are cheap next to normal sweep
                // traffic, and one loop per registered root is simpler to
                // reason about than three.
                crate::primary_checkout_reaper::reap_primary_checkout_for(&root).await;

                let config = read_worktree_reaper_config(&root);
                if !resolve_enabled(&config) {
                    log::debug!(
                        "worktree_reaper: {} disabled (autonomous.worktreeReaper.enabled=false \
                         or LOOM_WORKTREE_REAPER unset-falsy) — skipping",
                        root.display()
                    );
                    continue;
                }
                let warn_below_gb = resolve_disk_warn_free_gb(&config);
                let root_for_task = root.clone();
                let joined =
                    tokio::task::spawn_blocking(move || reap_repo(&root_for_task, &config)).await;
                match joined {
                    Ok(report) => log_report(&root, &report, warn_below_gb),
                    Err(e) => log::error!(
                        "worktree_reaper: pass for {} panicked ({e}); continuing to the next repo",
                        root.display()
                    ),
                }
            }
        }
    })
}

/// Run one [`crate::orphan_process_reaper`] pass for `root` on the blocking
/// pool, gated by that repo's own `autonomous.processReaper.enabled`.
///
/// Kept beside the directory reaper (rather than as its own daemon loop)
/// because the two answer the same question about the same worktrees — "does
/// anything still own issue-N's worktree?" — and sharing a tick keeps them from
/// racing each other's `/proc` and forge probes.
async fn reap_orphan_processes_for(root: &Path) {
    let config = crate::orphan_process_reaper::read_config(root);
    if !crate::orphan_process_reaper::resolve_enabled(&config) {
        log::debug!(
            "orphan_process_reaper: {} disabled (autonomous.processReaper.enabled=false \
             or LOOM_ORPHAN_PROCESS_REAPER unset-falsy) — skipping",
            root.display()
        );
        return;
    }
    let root_for_task = root.to_path_buf();
    let joined = tokio::task::spawn_blocking(move || {
        crate::orphan_process_reaper::reap_repo_processes(&root_for_task, &config)
    })
    .await;
    match joined {
        Ok(report) => crate::orphan_process_reaper::log_report(root, &report),
        Err(e) => log::error!(
            "orphan_process_reaper: pass for {} panicked ({e}); continuing to the next repo",
            root.display()
        ),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::worktree_ops::clean::PrStatus;
    use crate::worktree_ops::InUseMarker;
    use serial_test::serial;
    use std::collections::HashSet;
    use std::fs;
    use std::sync::{Arc, Mutex};

    // ===================================================================
    // Test fixtures
    // ===================================================================

    /// Build a repo root with `.loom/worktrees/issue-<N>` directories, each
    /// carrying a `.loom-managed` sentinel unless `managed` says otherwise.
    fn make_repo(issues: &[(u32, bool)]) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        for (issue, managed) in issues {
            let wt = tmp
                .path()
                .join(".loom/worktrees")
                .join(format!("issue-{issue}"));
            fs::create_dir_all(&wt).unwrap();
            if *managed {
                fs::write(wt.join(".loom-managed"), "").unwrap();
            }
        }
        tmp
    }

    struct ProbeSpec {
        active: HashSet<u32>,
        issue_state: String,
        pr_status: PrStatus,
        uncommitted: bool,
        in_use: bool,
        editable: Vec<String>,
    }

    impl Default for ProbeSpec {
        fn default() -> Self {
            Self {
                active: HashSet::new(),
                issue_state: "CLOSED".to_string(),
                pr_status: PrStatus::Merged {
                    // Well outside any sane grace period.
                    merged_at: "2020-01-01T00:00:00Z".to_string(),
                },
                uncommitted: false,
                in_use: false,
                editable: Vec::new(),
            }
        }
    }

    /// Run a full reap pass against `repo` with scripted probes, recording
    /// which worktrees the (fake) remover was asked to delete.
    ///
    /// **Every test that calls this must be `#[serial]`** (the crate-default
    /// unkeyed group). `reap_worktrees` -> `reap_repo` reads the process-wide
    /// `LOOM_WORKTREE_ROOT` via `worktree_root::worktree_root()`, and
    /// `worktree_root`'s own tests *set* that var for the duration of their
    /// body. `#[serial]` only serializes against other `#[serial]` tests, so a
    /// non-serial reader here can otherwise observe a sibling module's
    /// override, resolve the wrong root, scan zero worktrees and flake
    /// nondeterministically (#5164, same class as #5133).
    fn run_pass(repo: &Path, spec: &ProbeSpec, opts: &CleanOptions) -> (ReapReport, Vec<u32>) {
        let removed = Arc::new(Mutex::new(Vec::new()));
        let in_use_marker = |_: &Path| {
            spec.in_use.then(|| InUseMarker {
                task_id: "t".to_string(),
                pid: "1".to_string(),
            })
        };
        let processes_using = |_: &Path| Vec::new();
        let editable_installs = |_: &Path| spec.editable.clone();
        let is_managed = |p: &Path| clean::is_loom_managed(p);
        let issue_state = |_: u32| spec.issue_state.clone();
        let pr_status = |_: u32| spec.pr_status.clone();
        let uncommitted = |_: &Path| spec.uncommitted;

        let probes = WorktreeProbes {
            active_issues: &spec.active,
            in_use_marker: &in_use_marker,
            processes_using: &processes_using,
            editable_installs: &editable_installs,
            is_managed: &is_managed,
            issue_state: &issue_state,
            pr_status: &pr_status,
            uncommitted: &uncommitted,
            now: Utc::now(),
        };

        let recorder = removed.clone();
        let remover = move |_: &Path, issue: u32| {
            recorder.lock().unwrap().push(issue);
            true
        };

        let report = reap_worktrees(repo, opts, &probes, &remover);
        let removed = removed.lock().unwrap().clone();
        (report, removed)
    }

    fn default_opts() -> CleanOptions {
        reaper_clean_options(DEFAULT_GRACE_PERIOD_SECS)
    }

    /// Same scripted-probe shape as [`run_pass`], but drives
    /// [`reclaim_kept_worktree_artifacts`] instead of [`reap_worktrees`].
    fn run_reclaim_pass(repo: &Path, spec: &ProbeSpec, opts: &CleanOptions) -> ReclaimReport {
        let in_use_marker = |_: &Path| {
            spec.in_use.then(|| InUseMarker {
                task_id: "t".to_string(),
                pid: "1".to_string(),
            })
        };
        let processes_using = |_: &Path| Vec::new();
        let editable_installs = |_: &Path| spec.editable.clone();
        let is_managed = |p: &Path| clean::is_loom_managed(p);
        let issue_state = |_: u32| spec.issue_state.clone();
        let pr_status = |_: u32| spec.pr_status.clone();
        let uncommitted = |_: &Path| spec.uncommitted;

        let probes = WorktreeProbes {
            active_issues: &spec.active,
            in_use_marker: &in_use_marker,
            processes_using: &processes_using,
            editable_installs: &editable_installs,
            is_managed: &is_managed,
            issue_state: &issue_state,
            pr_status: &pr_status,
            uncommitted: &uncommitted,
            now: Utc::now(),
        };

        reclaim_kept_worktree_artifacts(repo, opts, &probes, false)
    }

    /// Populate `issue-<N>`'s worktree with a `target/` and `node_modules/`
    /// directory plus an untouchable `Cargo.lock` file, so tests can assert
    /// on what a reclaim pass did and did not remove.
    fn populate_build_artifacts(repo: &Path, issue: u32) {
        let wt = repo.join(".loom/worktrees").join(format!("issue-{issue}"));
        fs::create_dir_all(wt.join("target/debug")).unwrap();
        fs::create_dir_all(wt.join("node_modules/.bin")).unwrap();
        fs::write(wt.join("Cargo.lock"), b"lockfile").unwrap();
    }

    // ===================================================================
    // The core defect: a merged PR's worktree is reclaimed with no manual
    // `clean` and no merge-pr.sh side effect on this host.
    // ===================================================================

    #[test]
    #[serial]
    fn test_merged_pr_worktree_is_reaped_without_a_manual_clean() {
        let repo = make_repo(&[(100, true), (101, true)]);
        let (report, removed) = run_pass(repo.path(), &ProbeSpec::default(), &default_opts());
        assert_eq!(report.scanned, 2);
        assert_eq!(removed, vec![100, 101]);
        assert_eq!(report.removed, vec![100, 101]);
        assert!(report.failed.is_empty());
        assert!(report.skipped.is_empty());
    }

    #[test]
    #[serial]
    fn test_reap_is_idempotent_second_pass_finds_nothing() {
        // The remover is a fake, so simulate the real effect by deleting the
        // directory ourselves and re-running: a second pass must be a no-op
        // and report zero failures (`clean --safe` idempotency, preserved).
        let repo = make_repo(&[(100, true)]);
        let (first, _) = run_pass(repo.path(), &ProbeSpec::default(), &default_opts());
        assert_eq!(first.removed, vec![100]);

        fs::remove_dir_all(repo.path().join(".loom/worktrees/issue-100")).unwrap();
        let (second, removed) = run_pass(repo.path(), &ProbeSpec::default(), &default_opts());
        assert_eq!(second.scanned, 0);
        assert!(removed.is_empty());
        assert!(second.failed.is_empty());
    }

    #[test]
    #[serial]
    fn test_missing_worktree_root_is_a_clean_no_op() {
        let tmp = tempfile::tempdir().unwrap();
        let (report, removed) = run_pass(tmp.path(), &ProbeSpec::default(), &default_opts());
        assert_eq!(report, ReapReport::default());
        assert!(removed.is_empty());
    }

    // ===================================================================
    // Safety gates — the reaper must never out-delete `clean --safe`
    // ===================================================================

    #[test]
    #[serial]
    fn test_unmanaged_worktree_is_never_reaped() {
        // No `.loom-managed` sentinel ⇒ user-provisioned ⇒ preserved even
        // though every other gate says "removable".
        let repo = make_repo(&[(200, false)]);
        let (report, removed) = run_pass(repo.path(), &ProbeSpec::default(), &default_opts());
        assert!(removed.is_empty());
        assert_eq!(report.skipped.len(), 1);
        assert!(report.skipped[0].1.contains(".loom-managed"), "{:?}", report.skipped);
    }

    #[test]
    #[serial]
    fn test_open_pr_worktree_is_never_reaped() {
        let repo = make_repo(&[(300, true)]);
        let spec = ProbeSpec {
            pr_status: PrStatus::Open,
            ..ProbeSpec::default()
        };
        let (report, removed) = run_pass(repo.path(), &spec, &default_opts());
        assert!(removed.is_empty());
        assert_eq!(report.skipped[0].1, "PR still open");
    }

    #[test]
    #[serial]
    fn test_unmerged_and_absent_pr_worktrees_are_never_reaped() {
        for (status, expect) in [
            (PrStatus::ClosedNoMerge, "PR closed without merge"),
            (PrStatus::NoPr, "no PR found for closed issue"),
            (PrStatus::Unknown, "PR status unknown"),
        ] {
            let repo = make_repo(&[(301, true)]);
            let spec = ProbeSpec {
                pr_status: status,
                ..ProbeSpec::default()
            };
            let (report, removed) = run_pass(repo.path(), &spec, &default_opts());
            assert!(removed.is_empty(), "{expect}");
            assert_eq!(report.skipped[0].1, expect);
        }
    }

    #[test]
    #[serial]
    fn test_open_issue_worktree_is_never_reaped() {
        let repo = make_repo(&[(302, true)]);
        let spec = ProbeSpec {
            issue_state: "OPEN".to_string(),
            ..ProbeSpec::default()
        };
        let (_, removed) = run_pass(repo.path(), &spec, &default_opts());
        assert!(removed.is_empty());
    }

    #[test]
    #[serial]
    fn test_uncommitted_changes_block_the_reap() {
        let repo = make_repo(&[(303, true)]);
        let spec = ProbeSpec {
            uncommitted: true,
            ..ProbeSpec::default()
        };
        let (report, removed) = run_pass(repo.path(), &spec, &default_opts());
        assert!(removed.is_empty());
        assert_eq!(report.skipped[0].1, "uncommitted changes");
    }

    #[test]
    #[serial]
    fn test_in_use_marker_blocks_the_reap() {
        let repo = make_repo(&[(304, true)]);
        let spec = ProbeSpec {
            in_use: true,
            ..ProbeSpec::default()
        };
        let (report, removed) = run_pass(repo.path(), &spec, &default_opts());
        assert!(removed.is_empty());
        assert!(report.skipped[0].1.contains("in use by shepherd"));
    }

    #[test]
    #[serial]
    fn test_live_claim_blocks_the_reap() {
        let repo = make_repo(&[(305, true)]);
        let spec = ProbeSpec {
            active: HashSet::from([305]),
            ..ProbeSpec::default()
        };
        let (report, removed) = run_pass(repo.path(), &spec, &default_opts());
        assert!(removed.is_empty());
        assert!(report.skipped[0]
            .1
            .contains("spawn-loop task or claim-lock"));
    }

    #[test]
    #[serial]
    fn test_editable_install_blocks_the_reap() {
        let repo = make_repo(&[(306, true)]);
        let spec = ProbeSpec {
            editable: vec!["loom-tools".to_string()],
            ..ProbeSpec::default()
        };
        let (report, removed) = run_pass(repo.path(), &spec, &default_opts());
        assert!(removed.is_empty());
        assert!(report.skipped[0].1.contains("loom-tools"));
    }

    #[test]
    #[serial]
    fn test_grace_period_defers_a_just_merged_pr() {
        let repo = make_repo(&[(307, true)]);
        let spec = ProbeSpec {
            pr_status: PrStatus::Merged {
                merged_at: Utc::now().to_rfc3339(),
            },
            ..ProbeSpec::default()
        };
        let (report, removed) = run_pass(repo.path(), &spec, &default_opts());
        assert!(removed.is_empty());
        assert!(report.skipped[0].1.contains("grace period not passed"));
    }

    #[test]
    #[serial]
    fn test_non_issue_directories_are_ignored() {
        let repo = make_repo(&[(400, true)]);
        fs::create_dir_all(repo.path().join(".loom/worktrees/scratch")).unwrap();
        fs::create_dir_all(repo.path().join(".loom/worktrees/issue-abc")).unwrap();
        let (report, removed) = run_pass(repo.path(), &ProbeSpec::default(), &default_opts());
        assert_eq!(report.scanned, 1);
        assert_eq!(removed, vec![400]);
    }

    #[test]
    fn test_failed_removal_is_reported_not_silently_counted_as_removed() {
        let repo = make_repo(&[(500, true)]);
        let spec = ProbeSpec::default();
        let opts = default_opts();
        let active = HashSet::new();
        let in_use_marker = |_: &Path| None;
        let processes_using = |_: &Path| Vec::new();
        let editable_installs = |_: &Path| Vec::new();
        let is_managed = |p: &Path| clean::is_loom_managed(p);
        let issue_state = |_: u32| spec.issue_state.clone();
        let pr_status = |_: u32| spec.pr_status.clone();
        let uncommitted = |_: &Path| false;
        let probes = WorktreeProbes {
            active_issues: &active,
            in_use_marker: &in_use_marker,
            processes_using: &processes_using,
            editable_installs: &editable_installs,
            is_managed: &is_managed,
            issue_state: &issue_state,
            pr_status: &pr_status,
            uncommitted: &uncommitted,
            now: Utc::now(),
        };
        let report = reap_worktrees(repo.path(), &opts, &probes, &|_, _| false);
        assert!(report.removed.is_empty());
        assert_eq!(report.failed, vec![500]);
    }

    // ===================================================================
    // Artifact reclaim pass (#5187, AC3 of #5177) — reclaim target/,
    // node_modules/, ... from a KEPT worktree without removing it.
    // ===================================================================

    #[test]
    #[serial]
    fn test_reclaim_never_touches_an_in_use_marked_worktree() {
        let repo = make_repo(&[(600, true)]);
        populate_build_artifacts(repo.path(), 600);
        let spec = ProbeSpec {
            in_use: true,
            ..ProbeSpec::default()
        };
        let report = run_reclaim_pass(repo.path(), &spec, &default_opts());
        assert!(report.reclaimed.is_empty());
        assert!(report.skipped[0].1.contains("in use by shepherd"));
        // Nothing was removed — the marker alone must veto the reclaim.
        let wt = repo.path().join(".loom/worktrees/issue-600");
        assert!(wt.join("target").is_dir());
        assert!(wt.join("node_modules").is_dir());
    }

    #[test]
    #[serial]
    fn test_reclaim_never_touches_an_actively_building_worktree() {
        // A live spawn-loop claim/lock is the "actively building" signal.
        let repo = make_repo(&[(601, true)]);
        populate_build_artifacts(repo.path(), 601);
        let spec = ProbeSpec {
            active: HashSet::from([601]),
            ..ProbeSpec::default()
        };
        let report = run_reclaim_pass(repo.path(), &spec, &default_opts());
        assert!(report.reclaimed.is_empty());
        assert!(report.skipped[0]
            .1
            .contains("spawn-loop task or claim-lock"));
        let wt = repo.path().join(".loom/worktrees/issue-601");
        assert!(wt.join("target").is_dir());
    }

    #[test]
    #[serial]
    fn test_reclaim_removes_artifacts_from_a_finished_but_kept_worktree() {
        // Open issue ⇒ `classify_worktree` never returns `Remove` (the
        // worktree is kept), and nothing marks it in-use ⇒ the sweep is
        // finished. This is the exact "kept worktree" case AC3 targets.
        let repo = make_repo(&[(602, true)]);
        populate_build_artifacts(repo.path(), 602);
        let spec = ProbeSpec {
            issue_state: "OPEN".to_string(),
            ..ProbeSpec::default()
        };
        let report = run_reclaim_pass(repo.path(), &spec, &default_opts());
        assert_eq!(report.scanned, 1);
        assert!(report.skipped.is_empty(), "{:?}", report.skipped);
        assert_eq!(report.reclaimed.len(), 1);
        let (issue, dirs) = &report.reclaimed[0];
        assert_eq!(*issue, 602);
        assert_eq!(
            dirs.iter()
                .map(String::as_str)
                .collect::<std::collections::HashSet<_>>(),
            std::collections::HashSet::from(["node_modules", "target"])
        );
        let wt = repo.path().join(".loom/worktrees/issue-602");
        assert!(!wt.join("target").exists());
        assert!(!wt.join("node_modules").exists());
        // The worktree itself — and non-artifact files inside it — survive.
        assert!(wt.is_dir());
        assert!(wt.join("Cargo.lock").is_file());
    }

    #[test]
    #[serial]
    fn test_reclaim_still_runs_when_the_worktree_has_uncommitted_changes() {
        // `SkipUncommitted` is a removal gate, not an in-use gate — the test
        // plan explicitly calls out that a kept worktree with uncommitted
        // work elsewhere still has its build-artifact dirs reclaimed.
        let repo = make_repo(&[(603, true)]);
        populate_build_artifacts(repo.path(), 603);
        let spec = ProbeSpec {
            uncommitted: true,
            ..ProbeSpec::default()
        };
        let report = run_reclaim_pass(repo.path(), &spec, &default_opts());
        assert_eq!(report.reclaimed.len(), 1);
        let wt = repo.path().join(".loom/worktrees/issue-603");
        assert!(!wt.join("target").exists());
    }

    #[test]
    #[serial]
    fn test_reclaim_skips_a_worktree_eligible_for_full_removal() {
        // `ProbeSpec::default()` classifies as `Remove` (closed issue, merged
        // PR, grace period passed, no uncommitted changes) — the directory
        // reap pass takes the whole worktree, so the reclaim pass must not
        // separately touch it.
        let repo = make_repo(&[(604, true)]);
        populate_build_artifacts(repo.path(), 604);
        let report = run_reclaim_pass(repo.path(), &ProbeSpec::default(), &default_opts());
        assert!(report.reclaimed.is_empty());
        assert!(report.skipped[0].1.contains("eligible for full removal"));
        let wt = repo.path().join(".loom/worktrees/issue-604");
        assert!(wt.join("target").is_dir(), "reclaim pass must not touch it");
    }

    #[test]
    #[serial]
    fn test_reclaim_with_no_build_artifact_dirs_is_a_clean_no_op() {
        let repo = make_repo(&[(605, true)]);
        // No `populate_build_artifacts` call — nothing to reclaim.
        let spec = ProbeSpec {
            issue_state: "OPEN".to_string(),
            ..ProbeSpec::default()
        };
        let report = run_reclaim_pass(repo.path(), &spec, &default_opts());
        assert_eq!(report.scanned, 1);
        assert!(report.reclaimed.is_empty());
        assert!(report.skipped.is_empty());
    }

    #[test]
    #[serial]
    fn test_reclaim_missing_worktree_root_is_a_clean_no_op() {
        let tmp = tempfile::tempdir().unwrap();
        let report = run_reclaim_pass(tmp.path(), &ProbeSpec::default(), &default_opts());
        assert_eq!(report, ReclaimReport::default());
    }

    #[test]
    fn test_reclaim_summary_is_compact_and_complete() {
        let report = ReclaimReport {
            scanned: 3,
            reclaimed: vec![(1, vec!["target".to_string()])],
            skipped: vec![(2, "in use".to_string())],
        };
        assert_eq!(report.summary(), "scanned=3 reclaimed=1 skipped=1");
    }

    #[test]
    fn test_log_reclaim_report_handles_every_shape() {
        // Smoke: no panics on the empty / non-empty branches.
        let tmp = tempfile::tempdir().unwrap();
        log_reclaim_report(tmp.path(), &ReclaimReport::default());
        log_reclaim_report(
            tmp.path(),
            &ReclaimReport {
                scanned: 1,
                reclaimed: vec![(1, vec!["target".to_string(), "node_modules".to_string()])],
                skipped: vec![(2, "PR still open".to_string())],
            },
        );
    }

    // ===================================================================
    // Reaper options — the invariant the safety argument rests on
    // ===================================================================

    #[test]
    fn test_reaper_options_are_safe_never_forced_and_sentinel_gated() {
        let opts = reaper_clean_options(DEFAULT_GRACE_PERIOD_SECS);
        assert!(opts.safe, "the reaper must use --safe semantics");
        assert!(!opts.force, "the reaper must never bypass the safety gates");
        assert!(!opts.dry_run);
        assert!(
            opts.require_managed_sentinel,
            "an unattended remover must honor the .loom-managed sentinel"
        );
        assert_eq!(opts.grace_period_secs, DEFAULT_GRACE_PERIOD_SECS);
    }

    // ===================================================================
    // Config surface — autonomous.worktreeReaper
    // ===================================================================

    fn write_config(root: &Path, contents: &str) {
        fs::create_dir_all(root.join(".loom")).unwrap();
        fs::write(root.join(".loom").join("config.json"), contents).unwrap();
    }

    #[test]
    #[serial(loom_config_env)]
    fn test_config_missing_file_is_default() {
        std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
        let tmp = tempfile::tempdir().unwrap();
        let cfg = read_worktree_reaper_config(tmp.path());
        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
        assert_eq!(cfg, WorktreeReaperConfig::default());
    }

    #[test]
    #[serial(loom_config_env)]
    fn test_config_malformed_json_is_default() {
        std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), "{not valid json");
        let cfg = read_worktree_reaper_config(tmp.path());
        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
        assert_eq!(cfg, WorktreeReaperConfig::default());
    }

    #[test]
    #[serial(loom_config_env)]
    fn test_config_missing_block_is_default() {
        std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), r#"{"autonomous": {"workFinder": {"enabled": true}}}"#);
        let cfg = read_worktree_reaper_config(tmp.path());
        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
        assert_eq!(cfg, WorktreeReaperConfig::default());
    }

    #[test]
    fn test_config_reads_every_knob() {
        let tmp = tempfile::tempdir().unwrap();
        write_config(
            tmp.path(),
            r#"{"autonomous": {"worktreeReaper": {"enabled": false, "intervalSecs": 120,
               "gracePeriodSecs": 30, "diskWarnFreeGb": 5}}}"#,
        );
        assert_eq!(
            read_worktree_reaper_config(tmp.path()),
            WorktreeReaperConfig {
                enabled: Some(false),
                interval_secs: Some(120),
                grace_period_secs: Some(30),
                disk_warn_free_gb: Some(5),
            }
        );
    }

    #[test]
    fn test_config_zero_interval_is_dropped_to_none() {
        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), r#"{"autonomous": {"worktreeReaper": {"intervalSecs": 0}}}"#);
        assert_eq!(read_worktree_reaper_config(tmp.path()).interval_secs, None);
    }

    // ===================================================================
    // Precedence — env > config > default
    // ===================================================================

    #[test]
    #[serial]
    fn test_resolve_enabled_default_is_true() {
        std::env::remove_var(WORKTREE_REAPER_ENABLE_ENV);
        assert!(
            resolve_enabled(&WorktreeReaperConfig::default()),
            "the reaper restores a documented contract — absent config leaves it ON"
        );
    }

    #[test]
    #[serial]
    fn test_resolve_enabled_env_overrides_config() {
        std::env::set_var(WORKTREE_REAPER_ENABLE_ENV, "0");
        assert!(!resolve_enabled(&WorktreeReaperConfig {
            enabled: Some(true),
            ..WorktreeReaperConfig::default()
        }));
        std::env::set_var(WORKTREE_REAPER_ENABLE_ENV, "1");
        assert!(resolve_enabled(&WorktreeReaperConfig {
            enabled: Some(false),
            ..WorktreeReaperConfig::default()
        }));
        std::env::remove_var(WORKTREE_REAPER_ENABLE_ENV);
    }

    #[test]
    #[serial]
    fn test_resolve_enabled_config_can_disable() {
        std::env::remove_var(WORKTREE_REAPER_ENABLE_ENV);
        assert!(!resolve_enabled(&WorktreeReaperConfig {
            enabled: Some(false),
            ..WorktreeReaperConfig::default()
        }));
    }

    #[test]
    #[serial]
    fn test_resolve_interval_precedence() {
        std::env::remove_var(WORKTREE_REAPER_INTERVAL_ENV);
        assert_eq!(
            resolve_interval(&WorktreeReaperConfig::default()),
            Duration::from_secs(DEFAULT_WORKTREE_REAPER_INTERVAL_SECS)
        );
        let cfg = WorktreeReaperConfig {
            interval_secs: Some(300),
            ..WorktreeReaperConfig::default()
        };
        assert_eq!(resolve_interval(&cfg), Duration::from_secs(300));
        std::env::set_var(WORKTREE_REAPER_INTERVAL_ENV, "45");
        assert_eq!(resolve_interval(&cfg), Duration::from_secs(45));
        // Zero/garbage env falls through to config, not to the default.
        std::env::set_var(WORKTREE_REAPER_INTERVAL_ENV, "0");
        assert_eq!(resolve_interval(&cfg), Duration::from_secs(300));
        std::env::set_var(WORKTREE_REAPER_INTERVAL_ENV, "garbage");
        assert_eq!(resolve_interval(&cfg), Duration::from_secs(300));
        std::env::remove_var(WORKTREE_REAPER_INTERVAL_ENV);
    }

    #[test]
    #[serial]
    fn test_resolve_disk_warn_free_gb_precedence() {
        std::env::remove_var(WORKTREE_REAPER_DISK_WARN_ENV);
        assert_eq!(
            resolve_disk_warn_free_gb(&WorktreeReaperConfig::default()),
            DEFAULT_DISK_WARN_FREE_GB
        );
        let cfg = WorktreeReaperConfig {
            disk_warn_free_gb: Some(7),
            ..WorktreeReaperConfig::default()
        };
        assert_eq!(resolve_disk_warn_free_gb(&cfg), 7);
        std::env::set_var(WORKTREE_REAPER_DISK_WARN_ENV, "3");
        assert_eq!(resolve_disk_warn_free_gb(&cfg), 3);
        std::env::remove_var(WORKTREE_REAPER_DISK_WARN_ENV);
    }

    #[test]
    fn test_resolve_grace_period_default_and_config() {
        assert_eq!(
            resolve_grace_period(&WorktreeReaperConfig::default()),
            DEFAULT_GRACE_PERIOD_SECS
        );
        assert_eq!(
            resolve_grace_period(&WorktreeReaperConfig {
                grace_period_secs: Some(42),
                ..WorktreeReaperConfig::default()
            }),
            42
        );
    }

    // ===================================================================
    // Reporting
    // ===================================================================

    #[test]
    fn test_summary_is_compact_and_complete() {
        let report = ReapReport {
            scanned: 5,
            removed: vec![1, 2],
            failed: vec![3],
            skipped: vec![(4, "PR still open".to_string())],
            free_gb: Some(10),
        };
        assert_eq!(report.summary(), "scanned=5 removed=2 failed=1 skipped=1");
    }

    #[test]
    fn test_log_report_handles_every_disk_probe_shape() {
        // Smoke: no panics on the warn / ok / unmeasurable branches (the
        // low-disk warning is the only consumer of `free_gb`).
        let tmp = tempfile::tempdir().unwrap();
        for free_gb in [Some(1), Some(1000), None] {
            log_report(
                tmp.path(),
                &ReapReport {
                    scanned: 1,
                    free_gb,
                    ..ReapReport::default()
                },
                DEFAULT_DISK_WARN_FREE_GB,
            );
        }
    }
}
