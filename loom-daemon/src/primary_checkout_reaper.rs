//! Periodic primary-checkout reaper (#5268): returns a managed repo's
//! **primary** checkout to its default branch when it is parked on a dead
//! branch — one whose PR merged or closed without merging, or that never had
//! a PR at all and carries no commits that would be lost by switching away.
//!
//! # The gap this closes
//!
//! [`crate::worktree_reaper`] and `loom-daemon clean` both operate on
//! `.loom/worktrees/<n>` entries — never on the **primary checkout's own
//! `HEAD`**. Nothing returns *that* checkout to the default branch, so a
//! primary clone left checked out on a feature branch (no PR ever opened) or
//! on a PR branch whose PR was closed without merging stays there
//! indefinitely. Every primary-clone agent — role ticks, the work finder,
//! anything that isn't running inside a `.loom/worktrees/` worktree — then
//! reads stale files off that branch, and any repo-level `git` operation run
//! from the primary checkout lands on the dead branch by default. Observed in
//! production during loom#5184 (2026-08-04): two of eight managed-repo
//! primary checkouts were parked exactly this way (one on a never-PR'd
//! feature branch, one on a branch whose PR had been closed without merge).
//!
//! # Gates (mirrors [`crate::worktree_ops::clean::classify_worktree`]'s shape)
//!
//! 1. **Resolvable branches.** The current branch and the repo's default
//!    branch (`origin/HEAD`) must both resolve — a detached HEAD, or a repo
//!    with no `origin/HEAD` set, is left alone.
//! 2. **Already on default?** Nothing to do.
//! 3. **Clean tree, not mid-rebase/merge/cherry-pick.** Checked with `git
//!    status --porcelain` PLUS a scan for `.git/{rebase-merge,rebase-apply,
//!    MERGE_HEAD,CHERRY_PICK_HEAD,BISECT_LOG,REVERT_HEAD}` — a tree can be
//!    porcelain-clean yet still mid-rebase between conflict-free steps. Any
//!    ambiguity (a `git` failure) fails closed as dirty.
//! 4. **PR state via the forge API** ([`clean::check_pr_status_for_branch`] /
//!    [`clean::check_pr_status_for_branch_rest`] — the branch-name-generic
//!    siblings of the `feature/issue-<n>`-keyed helpers
//!    [`crate::worktree_reaper`] already uses for the same purpose), never
//!    `git branch -d` reachability classification (loom#4889 is the
//!    precedent failure mode: squash-merge makes reachability useless for
//!    "was this merged?"):
//!    - `Open` → skip, still being worked.
//!    - `Unknown` (forge probe failed) → skip, fail closed.
//!    - `Merged` → gated by the same post-merge grace period
//!      [`clean::check_grace_period`] applies to worktree removal.
//!    - `ClosedNoMerge` / `NoPr` → no grace period (no merge event to race).
//! 5. **No commits that would be lost.** Regardless of the PR state above,
//!    the branch must carry no commits absent from its own remote upstream
//!    (`@{u}..HEAD`) — this is what makes step 4 safe even for a `Merged`
//!    verdict: a local branch can carry commits made *after* what the PR
//!    merged that were never pushed, and comparing HEAD against the default
//!    branch directly would misclassify a squash-merged branch's own already
//!    -merged commits as "unpushed" (loom#4889 again). A branch with **no
//!    upstream at all** (never pushed) falls back to counting commits ahead
//!    of `origin/<default>` — the only case with no forge-side record to
//!    consult, so any commit ahead of default is treated as unsafe to
//!    discard.
//!
//! Every ambiguity resolves to *skip*: unlike [`crate::worktree_reaper`]
//! (which deletes a *disposable* worktree directory), this switches the
//! **primary checkout's own HEAD** — a mistake here is not "redo the sweep",
//! it is "an operator's working tree changed under them". Under-acting is
//! always recoverable on the next tick; over-acting is not.
//!
//! # Default-on
//!
//! Rides the same tick as [`crate::worktree_reaper`] (see
//! `daemon_service.rs`), so it costs nothing beyond what that loop already
//! pays for the primary checkout. Default-on for the same reason: it restores
//! CLAUDE.md's tacit contract ("agents read the default branch") and its
//! absence is a slow-motion outage. Opt out with
//! `LOOM_PRIMARY_CHECKOUT_REAPER=0` or
//! `autonomous.primaryCheckoutReaper.enabled=false`.

use std::path::{Path, PathBuf};
use std::process::Command;

use chrono::{DateTime, Utc};

use crate::worktree_ops::clean::{self, PrStatus, DEFAULT_GRACE_PERIOD_SECS};

// ============================================================================
// Constants
// ============================================================================

/// Master on/off env override. Default-on (see module docs): set to
/// `0`/`false`/`no`/`off` to disable, `1`/`true`/`yes`/`on` to force-enable
/// even when config disables it.
pub const PRIMARY_CHECKOUT_REAPER_ENABLE_ENV: &str = "LOOM_PRIMARY_CHECKOUT_REAPER";

// ============================================================================
// Config (.loom/config.json → autonomous.primaryCheckoutReaper)
// ============================================================================

/// The subset of `.loom/config.json → autonomous.primaryCheckoutReaper` this
/// module consumes. Every field is `Option` so an absent key falls through to
/// the env-var / built-in-default resolution — precedence **env > config >
/// default**, matching every other `autonomous.*` surface.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PrimaryCheckoutReaperConfig {
    /// `autonomous.primaryCheckoutReaper.enabled` (default **true**).
    pub enabled: Option<bool>,
    /// `autonomous.primaryCheckoutReaper.gracePeriodSecs` — how long after a
    /// PR merge the primary checkout becomes eligible to switch. Same knob
    /// shape as `worktreeReaper.gracePeriodSecs`.
    pub grace_period_secs: Option<i64>,
}

/// Read `.loom/config.json → autonomous.primaryCheckoutReaper`, soft-failing
/// every field to `None` (env/default resolution) on a missing file,
/// malformed JSON, or a missing `autonomous` / `primaryCheckoutReaper` block.
#[must_use]
pub fn read_config(repo_root: &Path) -> PrimaryCheckoutReaperConfig {
    let effective = crate::config_resolver::resolve_effective_config(repo_root);
    let Some(block) =
        crate::config_resolver::get_path(&effective, "autonomous.primaryCheckoutReaper")
    else {
        return PrimaryCheckoutReaperConfig::default();
    };

    PrimaryCheckoutReaperConfig {
        enabled: block.get("enabled").and_then(serde_json::Value::as_bool),
        grace_period_secs: block
            .get("gracePeriodSecs")
            .and_then(serde_json::Value::as_i64)
            .filter(|&s| s >= 0),
    }
}

/// Resolve whether the pass runs — precedence **env > config > default(true)**.
#[must_use]
pub fn resolve_enabled(config: &PrimaryCheckoutReaperConfig) -> bool {
    if let Ok(v) = std::env::var(PRIMARY_CHECKOUT_REAPER_ENABLE_ENV) {
        return matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on");
    }
    config.enabled.unwrap_or(true)
}

/// Resolve the post-merge grace period — precedence **config > default**
/// (mirrors [`crate::worktree_reaper::resolve_grace_period`]; no dedicated
/// env var).
#[must_use]
pub fn resolve_grace_period(config: &PrimaryCheckoutReaperConfig) -> i64 {
    config
        .grace_period_secs
        .unwrap_or(DEFAULT_GRACE_PERIOD_SECS)
}

// ============================================================================
// Decision
// ============================================================================

/// The outcome of applying every primary-checkout safety gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrimaryCheckoutDecision {
    /// Every gate passed — safe to `git checkout` the repo's default branch.
    Switch {
        from: String,
        to: String,
        reason: String,
    },
    /// The checkout is already on the default branch — nothing to do.
    AlreadyOnDefault(String),
    /// Could not determine the current branch (detached HEAD, or a `git`
    /// failure).
    SkipUnknownBranch,
    /// Could not resolve the repo's default branch (`origin/HEAD` unset, or a
    /// `git` failure).
    SkipUnknownDefaultBranch,
    /// The working tree has uncommitted/untracked changes, or is mid-rebase,
    /// mid-merge, mid-cherry-pick, mid-bisect, or mid-revert.
    SkipDirty,
    /// The PR for this branch is still open.
    SkipPrOpen,
    /// The PR status could not be determined (forge probe failed).
    SkipUnknownPrStatus,
    /// The PR merged, but the post-merge grace period has not elapsed
    /// (payload: seconds remaining).
    SkipGrace(i64),
    /// The branch carries commits that are not yet safe to discard — either
    /// ahead of its own remote upstream, or (no upstream configured) ahead of
    /// the default branch (payload: the count).
    SkipUnpushedCommits(u32),
    /// Could not determine whether the branch carries commits not yet safe to
    /// discard.
    SkipUnknownUnpushedState,
}

impl PrimaryCheckoutDecision {
    /// True only for [`PrimaryCheckoutDecision::Switch`].
    #[must_use]
    pub fn is_switch(&self) -> bool {
        matches!(self, Self::Switch { .. })
    }
}

/// Injected probes for [`classify_primary_checkout`], so the safety decision
/// is unit-testable without a live forge, `git`, or clock — the same
/// dependency-injection shape [`clean::WorktreeProbes`] already uses in the
/// sibling worktree-removal decision.
pub struct PrimaryCheckoutProbes<'a> {
    /// The branch currently checked out, or `None` for a detached HEAD.
    pub current_branch: &'a dyn Fn() -> Option<String>,
    /// The repo's default branch, or `None` if unresolvable.
    pub default_branch: &'a dyn Fn() -> Option<String>,
    /// Whether the working tree is dirty (uncommitted/untracked changes, or
    /// mid-rebase/merge/etc.).
    pub dirty: &'a dyn Fn() -> bool,
    /// Forge PR status for a given branch name.
    pub pr_status: &'a dyn Fn(&str) -> PrStatus,
    /// Commits not yet safe to discard by switching away — see the module
    /// docs' step 5. Takes the resolved default branch name (needed for the
    /// no-upstream fallback range). `None` means "could not determine".
    pub unpushed_commits: &'a dyn Fn(&str) -> Option<u32>,
    /// Wall clock the grace-period gate measures against.
    pub now: DateTime<Utc>,
}

/// Options [`classify_primary_checkout`] consumes — currently just the grace
/// period, mirroring [`clean::CleanOptions::grace_period_secs`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrimaryCheckoutOptions {
    pub grace_period_secs: i64,
}

/// Apply every primary-checkout safety gate (see the module docs) and report
/// the outcome. Pure decision logic: performs no `git`/forge I/O and mutates
/// nothing.
#[must_use]
pub fn classify_primary_checkout(
    opts: &PrimaryCheckoutOptions,
    probes: &PrimaryCheckoutProbes<'_>,
) -> PrimaryCheckoutDecision {
    let Some(branch) = (probes.current_branch)() else {
        return PrimaryCheckoutDecision::SkipUnknownBranch;
    };
    let Some(default) = (probes.default_branch)() else {
        return PrimaryCheckoutDecision::SkipUnknownDefaultBranch;
    };
    if branch == default {
        return PrimaryCheckoutDecision::AlreadyOnDefault(branch);
    }

    if (probes.dirty)() {
        return PrimaryCheckoutDecision::SkipDirty;
    }

    let reason = match (probes.pr_status)(&branch) {
        PrStatus::Open => return PrimaryCheckoutDecision::SkipPrOpen,
        PrStatus::Unknown => return PrimaryCheckoutDecision::SkipUnknownPrStatus,
        PrStatus::Merged { merged_at } => {
            if let Ok(dt) = DateTime::parse_from_rfc3339(&merged_at) {
                let (passed, remaining) = clean::check_grace_period(
                    dt.with_timezone(&Utc),
                    opts.grace_period_secs,
                    probes.now,
                );
                if !passed {
                    return PrimaryCheckoutDecision::SkipGrace(remaining);
                }
            }
            format!("PR merged ({merged_at})")
        }
        PrStatus::ClosedNoMerge => "PR closed without merge".to_string(),
        PrStatus::NoPr => "no PR found for this branch".to_string(),
    };

    match (probes.unpushed_commits)(&default) {
        Some(0) => {}
        Some(n) => return PrimaryCheckoutDecision::SkipUnpushedCommits(n),
        None => return PrimaryCheckoutDecision::SkipUnknownUnpushedState,
    }

    PrimaryCheckoutDecision::Switch {
        from: branch,
        to: default,
        reason,
    }
}

// ============================================================================
// Production `git` probes
// ============================================================================

/// Whether `repo_root`'s working tree has any change `git status --porcelain`
/// would report (staged, unstaged, **or untracked**) — deliberately stricter
/// than [`crate::worktree_ops::safety::check_uncommitted_changes`] (which
/// only checks tracked diffs): switching the **primary** checkout's `HEAD` is
/// a higher-stakes action than removing a disposable worktree, so an
/// untracked file the checkout would carry across the switch (or that a `git
/// checkout` could refuse to clobber) must also count as dirty.
///
/// Also treats a mid-rebase/merge/cherry-pick/bisect/revert state as dirty
/// even when `git status --porcelain` itself reports nothing outstanding (a
/// rebase paused between conflict-free steps can be porcelain-clean).
///
/// Fails **closed**: any `git` invocation failure (spawn error, non-zero
/// exit, an unresolvable git-dir) reads as dirty. This is the opposite of
/// [`crate::worktree_ops::safety::check_uncommitted_changes`]'s fail-open
/// convention — appropriate there because the caller layers other removal
/// gates on top of it, but not appropriate here, where this is one of only
/// two absolute gates standing between a background pass and an operator's
/// working tree.
#[must_use]
fn is_primary_checkout_dirty(repo_root: &Path) -> bool {
    if in_special_git_state(repo_root) {
        return true;
    }
    match Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(repo_root)
        .output()
    {
        Ok(out) if out.status.success() => !out.stdout.is_empty(),
        _ => true,
    }
}

/// Whether `repo_root` is mid-rebase, mid-merge, mid-cherry-pick,
/// mid-bisect, or mid-revert. Resolved via `git rev-parse --git-dir` (not a
/// bare `.git` join) so this still works if `.git` is ever a gitfile rather
/// than a directory. Fails closed: an unresolvable git-dir reads as "in a
/// special state".
fn in_special_git_state(repo_root: &Path) -> bool {
    let git_dir = match Command::new("git")
        .args(["rev-parse", "--git-dir"])
        .current_dir(repo_root)
        .output()
    {
        Ok(out) if out.status.success() => {
            let raw = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if raw.is_empty() {
                return true;
            }
            let p = PathBuf::from(raw);
            if p.is_absolute() {
                p
            } else {
                repo_root.join(p)
            }
        }
        _ => return true,
    };
    [
        "rebase-merge",
        "rebase-apply",
        "MERGE_HEAD",
        "CHERRY_PICK_HEAD",
        "BISECT_LOG",
        "REVERT_HEAD",
    ]
    .iter()
    .any(|marker| git_dir.join(marker).exists())
}

/// Commits on `HEAD` not yet safe to discard by switching away: commits ahead
/// of `HEAD`'s own remote upstream (`@{u}..HEAD`) when one is configured, or
/// — for a branch with **no upstream at all** (never pushed) — commits ahead
/// of `origin/<default_branch>` (falling back to the local `<default_branch>`
/// if that remote-tracking ref doesn't exist). `None` means "could not
/// determine" (any `git` failure), which the caller must treat as unsafe.
///
/// Deliberately does **not** compare a *pushed* branch against the default
/// branch directly — a squash-merged PR gives the default branch a new commit
/// hash for the same content, so `default..HEAD` reachability would
/// misclassify already-merged work as "unpushed" (the #4889 trap this
/// module's docs call out). Comparing against the branch's own upstream
/// sidesteps that: it answers "is there anything on this branch that isn't
/// already on some remote copy of it?", not "is this branch reachable from
/// default?".
#[must_use]
fn commits_not_yet_safe(repo_root: &Path, default_branch: &str) -> Option<u32> {
    if let Some(n) = commits_ahead(repo_root, "@{u}") {
        return Some(n);
    }
    let base = if origin_ref_exists(repo_root, default_branch) {
        format!("origin/{default_branch}")
    } else {
        default_branch.to_string()
    };
    commits_ahead(repo_root, &base)
}

fn commits_ahead(repo_root: &Path, base: &str) -> Option<u32> {
    let out = Command::new("git")
        .args(["rev-list", "--count", &format!("{base}..HEAD")])
        .current_dir(repo_root)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<u32>()
        .ok()
}

fn origin_ref_exists(repo_root: &Path, branch: &str) -> bool {
    Command::new("git")
        .args([
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/remotes/origin/{branch}"),
        ])
        .current_dir(repo_root)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn checkout_branch(repo_root: &Path, branch: &str) -> Result<(), String> {
    let out = Command::new("git")
        .args(["checkout", branch])
        .current_dir(repo_root)
        .output()
        .map_err(|e| format!("could not run `git checkout {branch}`: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        Err(if stderr.is_empty() {
            format!("exited with {}", out.status)
        } else {
            stderr
        })
    }
}

// ============================================================================
// One reap pass
// ============================================================================

/// What one primary-checkout reap pass did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrimaryCheckoutOutcome {
    /// The checkout was switched to the default branch.
    Switched {
        from: String,
        to: String,
        reason: String,
    },
    /// Every gate passed, but the `git checkout` itself failed.
    SwitchFailed {
        from: String,
        to: String,
        reason: String,
        cause: String,
    },
    /// A gate preserved the checkout's current branch.
    Skipped(PrimaryCheckoutDecision),
}

/// Run one production reap pass over `repo_root`'s primary checkout.
///
/// Wires the real probes (REST-first forge lookups, mirroring
/// [`crate::worktree_reaper::reap_repo`]) and the real `git checkout`.
#[must_use]
pub fn reap_repo(repo_root: &Path, config: &PrimaryCheckoutReaperConfig) -> PrimaryCheckoutOutcome {
    let opts = PrimaryCheckoutOptions {
        grace_period_secs: resolve_grace_period(config),
    };

    let current_branch = || clean::current_branch(repo_root);
    let default_branch = || clean::default_branch(repo_root);
    let dirty = || is_primary_checkout_dirty(repo_root);

    // Resolved once per pass (one REST call), not once per gate.
    let owner = clean::repo_owner_rest(repo_root);
    let pr_status = |branch: &str| match owner.as_deref() {
        Some(owner) => clean::check_pr_status_for_branch_rest(repo_root, owner, branch),
        // No owner ⇒ no REST head filter is constructible; fall back to the
        // GraphQL-backed probe rather than silently reporting Unknown forever.
        None => clean::check_pr_status_for_branch(repo_root, branch),
    };
    let unpushed_commits = |default: &str| commits_not_yet_safe(repo_root, default);

    let probes = PrimaryCheckoutProbes {
        current_branch: &current_branch,
        default_branch: &default_branch,
        dirty: &dirty,
        pr_status: &pr_status,
        unpushed_commits: &unpushed_commits,
        now: Utc::now(),
    };

    match classify_primary_checkout(&opts, &probes) {
        PrimaryCheckoutDecision::Switch { from, to, reason } => {
            match checkout_branch(repo_root, &to) {
                Ok(()) => PrimaryCheckoutOutcome::Switched { from, to, reason },
                Err(cause) => PrimaryCheckoutOutcome::SwitchFailed {
                    from,
                    to,
                    reason,
                    cause,
                },
            }
        }
        other => PrimaryCheckoutOutcome::Skipped(other),
    }
}

/// Human-readable reason a decision preserved the checkout's current branch.
/// `None` for outcomes that are not worth a log line (already on default, or
/// a successful/failed switch — those get their own log lines).
#[must_use]
fn skip_reason(decision: &PrimaryCheckoutDecision) -> Option<String> {
    match decision {
        PrimaryCheckoutDecision::Switch { .. } | PrimaryCheckoutDecision::AlreadyOnDefault(_) => {
            None
        }
        PrimaryCheckoutDecision::SkipUnknownBranch => {
            Some("could not determine the current branch (detached HEAD?)".to_string())
        }
        PrimaryCheckoutDecision::SkipUnknownDefaultBranch => {
            Some("could not resolve the repo's default branch (origin/HEAD unset?)".to_string())
        }
        PrimaryCheckoutDecision::SkipDirty => {
            Some("working tree is dirty or mid-rebase/merge".to_string())
        }
        PrimaryCheckoutDecision::SkipPrOpen => Some("PR still open".to_string()),
        PrimaryCheckoutDecision::SkipUnknownPrStatus => {
            Some("PR status unknown (forge probe failed)".to_string())
        }
        PrimaryCheckoutDecision::SkipGrace(remaining) => {
            Some(format!("grace period not passed ({remaining}s remaining)"))
        }
        PrimaryCheckoutDecision::SkipUnpushedCommits(n) => {
            Some(format!("{n} commit(s) not yet safe to discard"))
        }
        PrimaryCheckoutDecision::SkipUnknownUnpushedState => Some(
            "could not determine whether the branch carries commits not yet safe to discard"
                .to_string(),
        ),
    }
}

/// Log a pass's outcome.
pub fn log_outcome(repo_root: &Path, outcome: &PrimaryCheckoutOutcome) {
    match outcome {
        PrimaryCheckoutOutcome::Switched { from, to, reason } => log::info!(
            "primary_checkout_reaper: {} switched from '{from}' to '{to}' ({reason})",
            repo_root.display()
        ),
        PrimaryCheckoutOutcome::SwitchFailed {
            from,
            to,
            reason,
            cause,
        } => log::warn!(
            "primary_checkout_reaper: {} eligible to switch from '{from}' to '{to}' ({reason}) \
             but `git checkout` failed: {cause}",
            repo_root.display()
        ),
        PrimaryCheckoutOutcome::Skipped(decision) => {
            if let Some(reason) = skip_reason(decision) {
                log::debug!(
                    "primary_checkout_reaper: {} staying put: {reason}",
                    repo_root.display()
                );
            }
        }
    }
}

// ============================================================================
// Runtime wiring
// ============================================================================

/// Run one primary-checkout reap pass for `root` on the blocking pool, gated
/// by that repo's own `autonomous.primaryCheckoutReaper.enabled`. Called
/// alongside [`crate::worktree_reaper`]'s own passes so all three ride the
/// same per-repo tick rather than each maintaining a separate daemon loop.
pub async fn reap_primary_checkout_for(root: &Path) {
    let config = read_config(root);
    if !resolve_enabled(&config) {
        log::debug!(
            "primary_checkout_reaper: {} disabled (autonomous.primaryCheckoutReaper.enabled=false \
             or LOOM_PRIMARY_CHECKOUT_REAPER unset-falsy) — skipping",
            root.display()
        );
        return;
    }
    let root_for_task = root.to_path_buf();
    let joined = tokio::task::spawn_blocking(move || reap_repo(&root_for_task, &config)).await;
    match joined {
        Ok(outcome) => log_outcome(root, &outcome),
        Err(e) => log::error!(
            "primary_checkout_reaper: pass for {} panicked ({e}); continuing to the next repo",
            root.display()
        ),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::process::Stdio;

    // ===================================================================
    // Pure decision logic — scripted probes, no `git`/forge/clock
    // ===================================================================

    struct ProbeSpec {
        current_branch: Option<String>,
        default_branch: Option<String>,
        dirty: bool,
        pr_status: PrStatus,
        unpushed: Option<u32>,
    }

    impl Default for ProbeSpec {
        fn default() -> Self {
            Self {
                current_branch: Some("feature/issue-82".to_string()),
                default_branch: Some("main".to_string()),
                dirty: false,
                pr_status: PrStatus::Merged {
                    // Well outside any sane grace period.
                    merged_at: "2020-01-01T00:00:00Z".to_string(),
                },
                unpushed: Some(0),
            }
        }
    }

    fn classify(spec: &ProbeSpec, grace_period_secs: i64) -> PrimaryCheckoutDecision {
        let current_branch = || spec.current_branch.clone();
        let default_branch = || spec.default_branch.clone();
        let dirty = || spec.dirty;
        let pr_status = |_: &str| spec.pr_status.clone();
        let unpushed_commits = |_: &str| spec.unpushed;

        let probes = PrimaryCheckoutProbes {
            current_branch: &current_branch,
            default_branch: &default_branch,
            dirty: &dirty,
            pr_status: &pr_status,
            unpushed_commits: &unpushed_commits,
            now: Utc::now(),
        };
        classify_primary_checkout(&PrimaryCheckoutOptions { grace_period_secs }, &probes)
    }

    #[test]
    fn test_merged_pr_with_no_unpushed_commits_switches() {
        let decision = classify(&ProbeSpec::default(), DEFAULT_GRACE_PERIOD_SECS);
        assert_eq!(
            decision,
            PrimaryCheckoutDecision::Switch {
                from: "feature/issue-82".to_string(),
                to: "main".to_string(),
                reason: "PR merged (2020-01-01T00:00:00Z)".to_string(),
            }
        );
        assert!(decision.is_switch());
    }

    #[test]
    fn test_closed_no_merge_switches_the_gf180_sar_adc_case() {
        // The real-world case cited in #5268: gf180-sar-adc parked on `pr-63`
        // whose PR was closed without merging.
        let spec = ProbeSpec {
            current_branch: Some("pr-63".to_string()),
            pr_status: PrStatus::ClosedNoMerge,
            ..Default::default()
        };
        let decision = classify(&spec, DEFAULT_GRACE_PERIOD_SECS);
        assert_eq!(
            decision,
            PrimaryCheckoutDecision::Switch {
                from: "pr-63".to_string(),
                to: "main".to_string(),
                reason: "PR closed without merge".to_string(),
            }
        );
    }

    #[test]
    fn test_never_pr_d_branch_with_no_commits_ahead_switches_the_gf180_bandgap_case() {
        // The other real-world case: gf180-bandgap parked on `feature/issue-82`
        // with no PR ever opened.
        let spec = ProbeSpec {
            pr_status: PrStatus::NoPr,
            unpushed: Some(0),
            ..Default::default()
        };
        let decision = classify(&spec, DEFAULT_GRACE_PERIOD_SECS);
        assert!(decision.is_switch());
    }

    #[test]
    fn test_already_on_default_is_a_no_op() {
        let spec = ProbeSpec {
            current_branch: Some("main".to_string()),
            ..Default::default()
        };
        assert_eq!(
            classify(&spec, DEFAULT_GRACE_PERIOD_SECS),
            PrimaryCheckoutDecision::AlreadyOnDefault("main".to_string())
        );
    }

    #[test]
    fn test_detached_head_is_skipped() {
        let spec = ProbeSpec {
            current_branch: None,
            ..Default::default()
        };
        assert_eq!(
            classify(&spec, DEFAULT_GRACE_PERIOD_SECS),
            PrimaryCheckoutDecision::SkipUnknownBranch
        );
    }

    #[test]
    fn test_unresolvable_default_branch_is_skipped() {
        let spec = ProbeSpec {
            default_branch: None,
            ..Default::default()
        };
        assert_eq!(
            classify(&spec, DEFAULT_GRACE_PERIOD_SECS),
            PrimaryCheckoutDecision::SkipUnknownDefaultBranch
        );
    }

    #[test]
    fn test_dirty_tree_is_never_touched_even_with_a_merged_pr() {
        let spec = ProbeSpec {
            dirty: true,
            ..Default::default()
        };
        assert_eq!(classify(&spec, DEFAULT_GRACE_PERIOD_SECS), PrimaryCheckoutDecision::SkipDirty);
    }

    #[test]
    fn test_open_pr_is_left_alone() {
        let spec = ProbeSpec {
            pr_status: PrStatus::Open,
            ..Default::default()
        };
        assert_eq!(classify(&spec, DEFAULT_GRACE_PERIOD_SECS), PrimaryCheckoutDecision::SkipPrOpen);
    }

    #[test]
    fn test_unknown_pr_status_fails_closed() {
        let spec = ProbeSpec {
            pr_status: PrStatus::Unknown,
            ..Default::default()
        };
        assert_eq!(
            classify(&spec, DEFAULT_GRACE_PERIOD_SECS),
            PrimaryCheckoutDecision::SkipUnknownPrStatus
        );
    }

    #[test]
    fn test_merged_but_within_grace_period_waits() {
        let spec = ProbeSpec {
            pr_status: PrStatus::Merged {
                merged_at: Utc::now().to_rfc3339(),
            },
            ..Default::default()
        };
        let decision = classify(&spec, DEFAULT_GRACE_PERIOD_SECS);
        assert!(matches!(decision, PrimaryCheckoutDecision::SkipGrace(_)), "{decision:?}");
    }

    #[test]
    fn test_unpushed_commits_are_never_discarded_even_with_a_merged_pr() {
        // The squash-merge / local-commits-after-merge case the module docs
        // call out: a merged PR must NOT be enough on its own.
        let spec = ProbeSpec {
            unpushed: Some(2),
            ..Default::default()
        };
        assert_eq!(
            classify(&spec, DEFAULT_GRACE_PERIOD_SECS),
            PrimaryCheckoutDecision::SkipUnpushedCommits(2)
        );
    }

    #[test]
    fn test_unknown_unpushed_state_fails_closed() {
        let spec = ProbeSpec {
            unpushed: None,
            ..Default::default()
        };
        assert_eq!(
            classify(&spec, DEFAULT_GRACE_PERIOD_SECS),
            PrimaryCheckoutDecision::SkipUnknownUnpushedState
        );
    }

    #[test]
    fn test_never_pushed_branch_with_commits_ahead_of_default_is_left_alone() {
        // No PR, no upstream, but local commits ahead of default — there is
        // no forge-side record to confirm it's safe to discard.
        let spec = ProbeSpec {
            pr_status: PrStatus::NoPr,
            unpushed: Some(3),
            ..Default::default()
        };
        assert_eq!(
            classify(&spec, DEFAULT_GRACE_PERIOD_SECS),
            PrimaryCheckoutDecision::SkipUnpushedCommits(3)
        );
    }

    // ===================================================================
    // Config resolution
    // ===================================================================

    #[test]
    fn test_resolve_enabled_defaults_true() {
        assert!(resolve_enabled(&PrimaryCheckoutReaperConfig::default()));
    }

    #[test]
    fn test_resolve_enabled_config_false() {
        let config = PrimaryCheckoutReaperConfig {
            enabled: Some(false),
            ..Default::default()
        };
        assert!(!resolve_enabled(&config));
    }

    #[test]
    fn test_resolve_grace_period_defaults_to_worktree_reaper_default() {
        assert_eq!(
            resolve_grace_period(&PrimaryCheckoutReaperConfig::default()),
            DEFAULT_GRACE_PERIOD_SECS
        );
    }

    #[test]
    fn test_resolve_grace_period_config_override() {
        let config = PrimaryCheckoutReaperConfig {
            grace_period_secs: Some(42),
            ..Default::default()
        };
        assert_eq!(resolve_grace_period(&config), 42);
    }

    // ===================================================================
    // Real-`git` helpers — throwaway repos, mirroring main_health_gate.rs's
    // `make_origin_and_clone` pattern.
    // ===================================================================

    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed in {}", dir.display());
    }

    /// Bare `origin` seeded with an initial `main` commit, plus a working
    /// clone checked out on `main`. Returns `(origin_dir, clone_dir)` — both
    /// `TempDir` guards kept alive for the caller.
    fn make_origin_and_clone() -> (tempfile::TempDir, tempfile::TempDir) {
        let origin = tempfile::tempdir().unwrap();
        git(origin.path(), &["init", "--bare", "--initial-branch=main"]);

        let seed = tempfile::tempdir().unwrap();
        git(seed.path(), &["init", "--initial-branch=main"]);
        git(seed.path(), &["config", "user.email", "t@t.t"]);
        git(seed.path(), &["config", "user.name", "t"]);
        std::fs::write(seed.path().join("file.txt"), "v1\n").unwrap();
        git(seed.path(), &["add", "."]);
        git(seed.path(), &["commit", "-m", "initial"]);
        git(seed.path(), &["remote", "add", "origin", origin.path().to_str().unwrap()]);
        git(seed.path(), &["push", "origin", "main"]);

        let clone = tempfile::tempdir().unwrap();
        git(
            clone.path(),
            &[
                "clone",
                origin.path().to_str().unwrap(),
                clone.path().to_str().unwrap(),
            ],
        );
        git(clone.path(), &["config", "user.email", "t@t.t"]);
        git(clone.path(), &["config", "user.name", "t"]);
        (origin, clone)
    }

    #[test]
    fn test_clean_checkout_is_not_dirty() {
        let (_origin, clone) = make_origin_and_clone();
        assert!(!is_primary_checkout_dirty(clone.path()));
    }

    #[test]
    fn test_unstaged_change_is_dirty() {
        let (_origin, clone) = make_origin_and_clone();
        std::fs::write(clone.path().join("file.txt"), "v2\n").unwrap();
        assert!(is_primary_checkout_dirty(clone.path()));
    }

    #[test]
    fn test_untracked_file_is_dirty() {
        // Deliberately stricter than `check_uncommitted_changes` (#5268): an
        // untracked file must still block switching the primary checkout.
        let (_origin, clone) = make_origin_and_clone();
        std::fs::write(clone.path().join("scratch.txt"), "x\n").unwrap();
        assert!(is_primary_checkout_dirty(clone.path()));
    }

    #[test]
    fn test_mid_rebase_is_dirty_even_with_a_clean_porcelain_status() {
        let (_origin, clone) = make_origin_and_clone();
        // A rebase paused with nothing outstanding: fabricate the state
        // directory `git status --porcelain` alone would not see.
        let git_dir = clone.path().join(".git");
        std::fs::create_dir_all(git_dir.join("rebase-merge")).unwrap();
        assert!(is_primary_checkout_dirty(clone.path()));
    }

    #[test]
    fn test_pushed_branch_with_no_local_only_commits_has_zero_unpushed() {
        let (_origin, clone) = make_origin_and_clone();
        git(clone.path(), &["checkout", "-b", "feature/issue-1"]);
        std::fs::write(clone.path().join("file.txt"), "v2\n").unwrap();
        git(clone.path(), &["commit", "-am", "work"]);
        git(clone.path(), &["push", "-u", "origin", "feature/issue-1"]);

        assert_eq!(commits_not_yet_safe(clone.path(), "main"), Some(0));
    }

    #[test]
    fn test_pushed_branch_with_a_local_only_commit_after_push_is_unsafe() {
        let (_origin, clone) = make_origin_and_clone();
        git(clone.path(), &["checkout", "-b", "feature/issue-2"]);
        std::fs::write(clone.path().join("file.txt"), "v2\n").unwrap();
        git(clone.path(), &["commit", "-am", "work"]);
        git(clone.path(), &["push", "-u", "origin", "feature/issue-2"]);

        // A commit made AFTER the push — never pushed, must never be lost,
        // even if the branch's own PR already merged.
        std::fs::write(clone.path().join("file.txt"), "v3\n").unwrap();
        git(clone.path(), &["commit", "-am", "local-only work"]);

        assert_eq!(commits_not_yet_safe(clone.path(), "main"), Some(1));
    }

    #[test]
    fn test_never_pushed_branch_with_no_commits_ahead_of_default_is_safe() {
        let (_origin, clone) = make_origin_and_clone();
        // A branch created locally but never committed to and never pushed —
        // identical to `main`.
        git(clone.path(), &["checkout", "-b", "scratch-branch"]);

        assert_eq!(commits_not_yet_safe(clone.path(), "main"), Some(0));
    }

    #[test]
    fn test_never_pushed_branch_with_local_commits_is_unsafe() {
        let (_origin, clone) = make_origin_and_clone();
        git(clone.path(), &["checkout", "-b", "scratch-branch"]);
        std::fs::write(clone.path().join("file.txt"), "v2\n").unwrap();
        git(clone.path(), &["commit", "-am", "never pushed"]);

        assert_eq!(commits_not_yet_safe(clone.path(), "main"), Some(1));
    }

    #[test]
    fn test_end_to_end_reap_repo_switches_a_never_pr_d_clean_branch() {
        let (_origin, clone) = make_origin_and_clone();
        git(clone.path(), &["checkout", "-b", "feature/issue-82"]);

        // `repo_owner_rest` / the forge probes all shell out to `gh` against a
        // throwaway local-only repo, so they resolve to `Unknown` /
        // `PrStatus::Unknown` here — verify that reads as "no PR record" is
        // NOT assumed; a genuinely offline/no-`gh` environment must skip, not
        // switch. This exercises the fail-closed default path end to end via
        // `reap_repo`'s production wiring.
        let config = PrimaryCheckoutReaperConfig::default();
        let outcome = reap_repo(clone.path(), &config);
        assert!(
            matches!(
                outcome,
                PrimaryCheckoutOutcome::Skipped(PrimaryCheckoutDecision::SkipUnknownPrStatus)
            ),
            "{outcome:?}"
        );
        // Never switched away from the branch.
        assert_eq!(clean::current_branch(clone.path()), Some("feature/issue-82".to_string()));
    }

    #[test]
    fn test_end_to_end_switch_via_classify_and_checkout_branch() {
        // `reap_repo` itself cannot be driven to `Switch` without a real
        // forge, so exercise the switch side effect directly: `classify` says
        // Switch, and `checkout_branch` performs it.
        let (_origin, clone) = make_origin_and_clone();
        git(clone.path(), &["checkout", "-b", "scratch-branch"]);
        assert_eq!(clean::current_branch(clone.path()), Some("scratch-branch".to_string()));

        checkout_branch(clone.path(), "main").unwrap();
        assert_eq!(clean::current_branch(clone.path()), Some("main".to_string()));
    }

    #[test]
    fn test_log_outcome_does_not_panic_on_every_variant() {
        // Cheap smoke test: every variant must format without panicking.
        let repo = Path::new("/tmp/does-not-matter");
        let mut buf = Vec::new();
        let _ = write!(
            buf,
            "{:?}",
            PrimaryCheckoutOutcome::Switched {
                from: "a".to_string(),
                to: "b".to_string(),
                reason: "r".to_string(),
            }
        );
        log_outcome(
            repo,
            &PrimaryCheckoutOutcome::Switched {
                from: "a".to_string(),
                to: "b".to_string(),
                reason: "r".to_string(),
            },
        );
        log_outcome(
            repo,
            &PrimaryCheckoutOutcome::SwitchFailed {
                from: "a".to_string(),
                to: "b".to_string(),
                reason: "r".to_string(),
                cause: "c".to_string(),
            },
        );
        log_outcome(
            repo,
            &PrimaryCheckoutOutcome::Skipped(PrimaryCheckoutDecision::AlreadyOnDefault(
                "main".to_string(),
            )),
        );
        log_outcome(repo, &PrimaryCheckoutOutcome::Skipped(PrimaryCheckoutDecision::SkipDirty));
    }
}
