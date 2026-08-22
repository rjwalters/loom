//! `loom-daemon clean`: the native port of `loom-clean` (`clean.py`).
//!
//! Covers standard + `--safe` worktree cleanup, local-branch cleanup
//! (two-pass: remote-ref-gone, then issue-state), tmux session cleanup,
//! per-agent Claude config-dir cleanup, `--deep` build-artifact cleanup,
//! and `--daemon` crash recovery (stale `loom:building` label revert +
//! stale spawn-loop claim-lock cleanup). `--aggressive` lives in
//! `aggressive.rs`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Utc};

use super::gh;
use super::liveness::active_spawn_loop_issues;
use super::naming::{self, BRANCH_PREFIX};
use super::safety::{
    check_uncommitted_changes, check_uncommitted_or_untracked_changes,
    find_processes_using_directory, read_in_use_marker, InUseMarker,
};
use crate::quarantine_stash_status::QUARANTINE_STASH_LABEL;

/// Default grace period after PR merge before a worktree is eligible for
/// `--safe` removal (10 minutes).
pub const DEFAULT_GRACE_PERIOD_SECS: i64 = 600;

/// Grace period after a PR **closes without merging** before its worktree's
/// directory becomes eligible for `--safe` removal (issue #6418): 30 days,
/// deliberately much longer than [`DEFAULT_GRACE_PERIOD_SECS`].
///
/// The merged case can afford a short grace period because `main` already
/// holds every commit the moment the PR merges — the worktree is pure
/// redundancy from that instant on. A closed-without-merge worktree has no
/// such guarantee: its commits may exist nowhere but that local branch. This
/// constant alone is not the safety gate — [`classify_worktree`] /
/// [`classify_pr_worktree`] additionally require the branch to be provably
/// present on a remote (see the `PrStatus::ClosedNoMerge` arm) before ever
/// returning [`WorktreeDecision::Remove`] for this case. The long grace
/// period is what leaves a human enough time to notice and push a branch
/// they closed the PR on by mistake, or to reopen it, before either gate
/// would otherwise let the reaper remove it.
pub const CLOSED_NO_MERGE_GRACE_PERIOD_SECS: i64 = 30 * 24 * 60 * 60;

/// Grace period after an issue **closes with no PR ever opened for it**
/// before its worktree's directory becomes eligible for `--safe` removal
/// (issue #6653) — e.g. a Builder claimed the issue, then it was closed as a
/// duplicate/not-planned before `gh pr create` ever ran. Same duration as
/// [`CLOSED_NO_MERGE_GRACE_PERIOD_SECS`] and for the same reason: `main`
/// never received these commits either, so a human needs enough time to
/// notice and push/reopen before the reaper would otherwise remove the only
/// copy. Like that constant, this alone is not the safety gate —
/// [`classify_worktree`]'s `PrStatus::NoPr` arm additionally requires the
/// branch to be provably present on a remote before ever returning
/// [`WorktreeDecision::Remove`] (or [`WorktreeDecision::RemoveWithQuarantine`]).
pub const NO_PR_GRACE_PERIOD_SECS: i64 = 30 * 24 * 60 * 60;

/// Minimum age before a `.loom/sweep-checkpoint/` transient is eligible for
/// bulk pruning (48 hours). Belt-and-suspenders on top of the liveness checks
/// in [`clean_sweep_transients`]: a sweep that has only just started (its
/// registry write racing this scan) is never touched, and it bounds the number
/// of forge probes a single clean pass can issue.
pub const SWEEP_TRANSIENT_MIN_AGE_SECS: u64 = 48 * 60 * 60;

/// Prompt on stdout/stdin for a `[y/N]` confirmation. EOF (no TTY attached,
/// e.g. under cron) is treated as "no" — matches
/// `clean.py`'s `except (EOFError, KeyboardInterrupt): response = ""` fallback.
fn confirm(prompt: &str) -> bool {
    use std::io::Write;
    print!("{prompt}");
    let _ = std::io::stdout().flush();
    let mut input = String::new();
    if std::io::stdin().read_line(&mut input).is_err() {
        return false;
    }
    matches!(input.trim().to_lowercase().as_str(), "y" | "yes")
}

/// Shared confirmation gate for every destructive `clean` mode (the general
/// pass, `--worktrees-only`, `--branches-only`, `--tmux-only`, and
/// `--aggressive`). Issue #5736: `--aggressive` used to bypass this gate
/// entirely by short-circuiting before [`run_clean`] was ever called, so a
/// closed-stdin/non-interactive invocation destroyed worktrees and branches
/// with zero prompt — the most destructive combination was also the one with
/// no gate. Every caller must route through this function instead of
/// re-implementing the dry-run/force/prompt tri-state.
///
/// - `dry_run` short-circuits to `true` with no prompt (a dry run never
///   mutates anything, so gating it adds friction with no safety benefit).
/// - `force` short-circuits to `true` with no prompt (existing
///   non-interactive-affirmative path; unchanged from today's behavior for
///   the non-aggressive modes). Note `force` is deliberately still a single
///   flag: whether it *also* widens the removal set (e.g. overriding
///   uncommitted-changes safety checks) is decided independently by each
///   caller's own logic (e.g. [`clean_aggressive`](super::aggressive::clean_aggressive)'s
///   `force` parameter) — this function only answers "may the run proceed at
///   all", not "how much may it remove".
/// - Otherwise, prompts on stdin. No TTY / closed stdin (e.g. `< /dev/null`)
///   reads as EOF, which [`confirm`] treats as "no" — the run aborts.
#[must_use]
pub fn confirm_destructive_action(dry_run: bool, force: bool) -> bool {
    if dry_run {
        println!("DRY RUN - No changes will be made");
        true
    } else if force {
        println!("FORCE MODE - Auto-confirming all prompts");
        true
    } else {
        confirm("Proceed with cleanup? [y/N] ")
    }
}

/// Compose one operator-actionable error diagnostic: *what* failed, *to what*,
/// and *why* (#4877). A bare `Errors: N` tally is not actionable, so every
/// recorded error carries these three parts.
#[must_use]
pub fn error_line(target: &str, operation: &str, cause: &str) -> String {
    let cause = cause.trim();
    if cause.is_empty() {
        format!("{operation} failed for {target}")
    } else {
        format!("{operation} failed for {target}: {cause}")
    }
}

/// Compose the closing line of a cleanup pass. A run that recorded errors must
/// never read identically to a clean one (#4877), so the count is folded into
/// the line itself rather than being buried in the summary block above it.
#[must_use]
pub fn completion_line(label: &str, dry_run: bool, errors: usize) -> String {
    let plural = if errors == 1 { "" } else { "s" };
    match (dry_run, errors) {
        (true, 0) => "Dry run complete - no changes made".to_string(),
        (true, n) => format!(
            "Dry run complete - no changes made, but {n} error{plural} occurred (see diagnostics above)"
        ),
        (false, 0) => format!("{label} complete!"),
        (false, n) => format!("{label} completed with {n} error{plural} (see diagnostics above)"),
    }
}

/// Process exit code for a finished pass: non-zero exactly when at least one
/// error was recorded, so a scripted caller can distinguish "completed with
/// errors" from "completed cleanly".
#[must_use]
pub fn exit_code(errors: usize) -> i32 {
    i32::from(errors > 0)
}

/// Run `cmd`, mapping failure to the underlying cause (git's stderr when it
/// wrote one, otherwise the exit status or the spawn error) so callers can name
/// it in their diagnostic instead of discarding it.
pub(super) fn run_checked(mut cmd: Command) -> Result<(), String> {
    match cmd.output() {
        Ok(o) if o.status.success() => Ok(()),
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr).trim().to_string();
            Err(if stderr.is_empty() {
                format!("exited with {}", o.status)
            } else {
                stderr
            })
        }
        Err(e) => Err(format!("could not run command: {e}")),
    }
}

#[derive(Debug, Default)]
pub struct CleanupStats {
    pub cleaned_worktrees: usize,
    /// Worktrees whose `git worktree remove` failed with "is not a working
    /// tree" and had no directory left on disk either (#5895) — git already
    /// had no record of the worktree and there was nothing unsafe left to
    /// remove, so these are reported as already-removed rather than folded
    /// into [`Self::errors`]. Tracked separately from
    /// [`Self::cleaned_worktrees`] so a summary can distinguish "we did the
    /// removal work" from "there was nothing left to do".
    pub stale_worktree_registrations: usize,
    pub skipped_open: usize,
    pub skipped_in_use: usize,
    pub skipped_not_merged: usize,
    pub skipped_grace: usize,
    pub skipped_uncommitted: usize,
    pub skipped_editable: usize,
    pub cleaned_branches: usize,
    pub kept_branches: usize,
    pub errored_branches: usize,
    pub killed_tmux: usize,
    /// Tmux sessions preserved because they have an attached client (a live
    /// operator terminal) or because `--safe` mode does not touch tmux at all
    /// (issue #4890).
    pub skipped_tmux: usize,
    pub cleaned_config_dirs: usize,
    pub cleaned_sweep_baselines: usize,
    pub cleaned_sweep_checkpoints: usize,
    pub kept_sweep_transients: usize,
    pub errors: usize,
    /// One diagnostic per recorded error, in the order they occurred. Printed
    /// inline as each error happens and re-listed under the summary tally.
    pub error_details: Vec<String>,
}

impl CleanupStats {
    /// Report a failure where it happens *and* tally it: prints an actionable
    /// diagnostic naming the target, the operation, and the underlying cause,
    /// then increments the error counter (#4877).
    pub fn record_error(&mut self, target: &str, operation: &str, cause: &str) {
        let line = error_line(target, operation, cause);
        eprintln!("  ERROR: {line}");
        self.errors += 1;
        self.error_details.push(line);
    }
}

/// Options mirroring `clean.py`'s argparse surface (minus subcommand-style
/// flags handled by the CLI layer directly: `--aggressive` routes to
/// `aggressive::clean_aggressive`, `--daemon` to [`clean_daemon_crash_state`]).
#[derive(Debug, Clone)]
pub struct CleanOptions {
    pub dry_run: bool,
    pub deep: bool,
    pub force: bool,
    pub safe: bool,
    pub grace_period_secs: i64,
    pub worktrees_only: bool,
    pub branches_only: bool,
    pub tmux_only: bool,
    /// Require the `.loom-managed` sentinel before a worktree is eligible for
    /// removal (issue #4876).
    ///
    /// The interactive `loom-daemon clean` CLI leaves this **false** to preserve
    /// its historical behavior (an operator who typed the command and answered
    /// the prompt is the authority). The daemon-side periodic reaper
    /// ([`crate::worktree_reaper`]) sets it **true**: an unattended background
    /// remover must honor CLAUDE.md's "user-provisioned worktrees are never
    /// removed" contract, because nobody is there to say no.
    pub require_managed_sentinel: bool,
}

impl Default for CleanOptions {
    fn default() -> Self {
        Self {
            dry_run: false,
            deep: false,
            force: false,
            safe: false,
            grace_period_secs: DEFAULT_GRACE_PERIOD_SECS,
            worktrees_only: false,
            branches_only: false,
            tmux_only: false,
            require_managed_sentinel: false,
        }
    }
}

/// PR status for a closed issue's worktree, used by `--safe` mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrStatus {
    Merged {
        merged_at: String,
    },
    /// The PR closed without merging. `closed_at` is the forge-reported
    /// close timestamp when the probe could resolve one (issue #6418) — used
    /// to gate [`CLOSED_NO_MERGE_GRACE_PERIOD_SECS`]. `None` when the probe
    /// path doesn't carry it (should not happen for a live `gh` response, but
    /// a probe must never treat "couldn't parse a timestamp" as "grace period
    /// already elapsed").
    ClosedNoMerge {
        closed_at: Option<String>,
    },
    Open,
    NoPr,
    Unknown,
}

#[derive(serde::Deserialize)]
struct PrRow {
    state: String,
    #[serde(default, rename = "mergedAt")]
    merged_at: Option<String>,
    #[serde(default, rename = "closedAt")]
    closed_at: Option<String>,
}

fn gh_pr_list(repo_root: &Path, args: &[&str]) -> Option<Vec<PrRow>> {
    let mut cmd = Command::new("gh");
    cmd.args(args).current_dir(repo_root);
    // #5401/#5431: cross-owner managed repo -> its own owner's installation-token
    // GH_CONFIG_DIR (no-op for single-owner fleets / the root owner).
    crate::credential_preflight::apply_gh_config_for_root(&mut cmd, repo_root);
    let out = cmd.output().ok()?;
    if !out.status.success() {
        return None;
    }
    serde_json::from_slice(&out.stdout).ok()
}

fn gh_pr_list_by_head(repo_root: &Path, branch: &str) -> Option<Vec<PrRow>> {
    gh_pr_list(
        repo_root,
        &[
            "pr",
            "list",
            "--head",
            branch,
            "--state",
            "all",
            "--json",
            "number,state,mergedAt,closedAt",
        ],
    )
}

fn gh_pr_list_by_issue_search(repo_root: &Path, issue_num: u32) -> Option<Vec<PrRow>> {
    gh_pr_list(
        repo_root,
        &[
            "pr",
            "list",
            "--search",
            &format!("Closes #{issue_num}"),
            "--state",
            "all",
            "--json",
            "number,state,mergedAt,closedAt",
        ],
    )
}

/// Resolve a list of same-branch PR rows (in whatever order the forge
/// returned them) to a single [`PrStatus`] (issue #6746): a branch can have
/// had more than one PR opened against it over its lifetime — e.g. a merged
/// PR, then later a second, unrelated PR opened from the same branch name
/// that was closed without merging (observed live for `feature/issue-5179`
/// during #6653's curation). Both `gh pr list --head` (GraphQL) and the REST
/// `pulls?head=` list endpoint default to newest-created-first, so naively
/// taking "the first row" picks whichever PR was opened *last* against the
/// branch, not whichever one is actually relevant.
///
/// Preference order:
/// 1. `Merged` always wins, over any number of other rows — a branch that was
///    ever merged must never be reported as merely closed-without-merge or
///    open just because a later PR against the same branch name says
///    otherwise.
/// 2. Otherwise `Open` wins — an actively-reviewed PR should not be shadowed
///    by an older closed one.
/// 3. Otherwise the first row in forge-returned order wins (i.e., given the
///    newest-first default ordering both probes rely on, the most recently
///    closed PR) — covers `ClosedNoMerge`/`Unknown` rows.
///
/// An empty row list resolves to [`PrStatus::NoPr`].
fn select_pr_status<I: IntoIterator<Item = PrStatus>>(rows: I) -> PrStatus {
    let mut best: Option<PrStatus> = None;
    for status in rows {
        if matches!(status, PrStatus::Merged { .. }) {
            return status;
        }
        match &best {
            None => best = Some(status),
            Some(PrStatus::Open) => {} // already the best short of Merged
            Some(_) => {
                if matches!(status, PrStatus::Open) {
                    best = Some(status);
                }
            }
        }
    }
    best.unwrap_or(PrStatus::NoPr)
}

/// Map an optional `gh pr list --json number,state,mergedAt,closedAt` result
/// onto a [`PrStatus`]: `None` (the `gh` call itself failed or returned
/// unparseable JSON) is `Unknown`; an empty (but successful) result is `NoPr`.
/// When multiple rows are present, see [`select_pr_status`] for the
/// preference order (issue #6746).
fn rows_to_status(rows: Option<Vec<PrRow>>) -> PrStatus {
    let Some(rows) = rows else {
        return PrStatus::Unknown;
    };
    select_pr_status(rows.into_iter().map(|row| {
        if let Some(merged_at) = row.merged_at {
            PrStatus::Merged { merged_at }
        } else if row.state == "CLOSED" {
            PrStatus::ClosedNoMerge {
                closed_at: row.closed_at,
            }
        } else if row.state == "OPEN" {
            PrStatus::Open
        } else {
            PrStatus::Unknown
        }
    }))
}

/// Check the PR status for `issue_num`'s branch. Thin `gh` wrapper mirroring
/// `clean.py::check_pr_merged`.
#[must_use]
pub fn check_pr_merged(repo_root: &Path, issue_num: u32) -> PrStatus {
    let branch = naming::branch_name(issue_num);
    let rows = gh_pr_list_by_head(repo_root, &branch)
        .or_else(|| gh_pr_list_by_issue_search(repo_root, issue_num));
    rows_to_status(rows)
}

/// GraphQL-backed PR-status lookup for an arbitrary branch name — the same
/// `--head` query [`check_pr_merged`] uses, minus the issue-number-keyed
/// `"Closes #N"` search fallback (there is no issue number to search for when
/// the branch does not follow the `feature/issue-<n>` convention — e.g. a
/// primary checkout parked on a hand-created `pr-63` branch, see #5268).
#[must_use]
pub fn check_pr_status_for_branch(repo_root: &Path, branch: &str) -> PrStatus {
    rows_to_status(gh_pr_list_by_head(repo_root, branch))
}

#[derive(serde::Deserialize)]
struct PrRowRest {
    state: String,
    #[serde(default)]
    merged_at: Option<String>,
    #[serde(default)]
    closed_at: Option<String>,
    #[serde(default)]
    head: Option<PrHeadRest>,
}

/// The `head` object of a REST pull-request payload. Only `sha` is read: it is
/// the safety criterion for force-deleting a `pr-<N>` worktree's local branch
/// (issue #5939, mirroring `merge-pr.sh`'s #4100 rule).
#[derive(Debug, serde::Deserialize)]
struct PrHeadRest {
    #[serde(default)]
    sha: Option<String>,
}

/// Resolve the repository owner via the **REST** API
/// (`gh api repos/{owner}/{repo} --jq .owner.login`).
///
/// Used to build the `head=<owner>:<branch>` filter [`check_pr_merged_rest`]
/// needs. Returns `None` on any failure so callers can fall back to the
/// GraphQL-backed [`check_pr_merged`].
#[must_use]
pub fn repo_owner_rest(repo_root: &Path) -> Option<String> {
    let mut cmd = Command::new("gh");
    cmd.args(["api", "repos/{owner}/{repo}", "--jq", ".owner.login"])
        .current_dir(repo_root);
    // #5401/#5431: cross-owner managed repo -> its own owner's installation-token
    // GH_CONFIG_DIR (no-op for single-owner fleets / the root owner).
    crate::credential_preflight::apply_gh_config_for_root(&mut cmd, repo_root);
    let out = cmd.output().ok()?;
    if !out.status.success() {
        return None;
    }
    let owner = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if owner.is_empty() {
        None
    } else {
        Some(owner)
    }
}

/// REST variant of [`check_pr_merged`] (issue #4876).
///
/// `gh pr list` goes through GraphQL, whose quota is routinely exhausted on a
/// host running several agents concurrently; the REST quota is separate and
/// much less contended. The daemon-side reaper probes unattended on a cadence,
/// so it must not compete with interactive agents for the scarcer pool.
///
/// Queries `repos/{owner}/{repo}/pulls?state=all&head=<owner>:<branch>`. Falls
/// back to nothing on its own — the caller decides whether an `Unknown` should
/// be retried through [`check_pr_merged`].
#[must_use]
pub fn check_pr_merged_rest(repo_root: &Path, owner: &str, issue_num: u32) -> PrStatus {
    check_pr_status_for_branch_rest(repo_root, owner, &naming::branch_name(issue_num))
}

/// REST variant of [`check_pr_status_for_branch`] — an arbitrary-branch
/// counterpart to [`check_pr_merged_rest`] for callers that do not have (or
/// cannot assume) a `feature/issue-<n>` branch name, e.g. a primary checkout
/// parked on a hand-created branch (#5268).
///
/// Queries `repos/{owner}/{repo}/pulls?state=all&head=<owner>:<branch>`. Not
/// capped to a single row (issue #6746): the REST list endpoint defaults to
/// newest-created-first, same as the GraphQL `gh pr list` probe, so a
/// `per_page=1` cap here has the identical misclassification exposure that
/// motivated dropping `--limit 1` from [`gh_pr_list_by_head`] — a branch with
/// more than one PR against it over its lifetime (e.g. a merged PR, then a
/// later unrelated PR closed without merging) would otherwise resolve to
/// whichever PR was opened last, not whichever one merged. See
/// [`select_pr_status`] for the preference order applied across rows.
#[must_use]
pub fn check_pr_status_for_branch_rest(repo_root: &Path, owner: &str, branch: &str) -> PrStatus {
    let path =
        format!("repos/{{owner}}/{{repo}}/pulls?state=all&head={owner}:{branch}&per_page=30");
    let mut cmd = Command::new("gh");
    cmd.args(["api", &path]).current_dir(repo_root);
    // #5401/#5431: cross-owner managed repo -> its own owner's installation-token
    // GH_CONFIG_DIR (no-op for single-owner fleets / the root owner).
    crate::credential_preflight::apply_gh_config_for_root(&mut cmd, repo_root);
    let Ok(out) = cmd.output() else {
        return PrStatus::Unknown;
    };
    if !out.status.success() {
        return PrStatus::Unknown;
    }
    let Ok(rows) = serde_json::from_slice::<Vec<PrRowRest>>(&out.stdout) else {
        return PrStatus::Unknown;
    };
    select_pr_status(rows.into_iter().map(|row| {
        classify_pr_row(row.state.as_str(), row.merged_at.as_deref(), row.closed_at.as_deref())
    }))
}

/// Map a REST pull-request `(state, merged_at, closed_at)` triple onto a
/// [`PrStatus`]. Pure, so the REST probe's classification is unit-testable
/// without `gh`.
#[must_use]
pub fn classify_pr_row(state: &str, merged_at: Option<&str>, closed_at: Option<&str>) -> PrStatus {
    if let Some(merged_at) = merged_at.filter(|s| !s.is_empty()) {
        return PrStatus::Merged {
            merged_at: merged_at.to_string(),
        };
    }
    match state.to_ascii_uppercase().as_str() {
        "CLOSED" => PrStatus::ClosedNoMerge {
            closed_at: closed_at.filter(|s| !s.is_empty()).map(str::to_string),
        },
        "OPEN" => PrStatus::Open,
        _ => PrStatus::Unknown,
    }
}

/// REST PR-status lookup keyed **directly on a PR number**, for `pr-<N>`
/// worktrees (issue #5939) — the counterpart of [`check_pr_merged_rest`] for
/// worktrees that were never created from an issue number to begin with, so
/// there is no `feature/issue-<N>` branch name to search by. Unlike
/// [`check_pr_status_for_branch_rest`], this needs no `owner` parameter: the
/// single-PR endpoint resolves `{owner}/{repo}` from the invoking repo just
/// like [`super::gh::issue_state_rest`] does, with no `head=<owner>:<branch>`
/// filter to construct.
///
/// Queries `repos/{owner}/{repo}/pulls/<n>` (a single object, not the search
/// list [`check_pr_merged_rest`] and friends page through). A 404 (PR number
/// does not exist against this repo) or any other failure resolves to
/// [`PrStatus::Unknown`] — a probe failure is always a skip, never a removal.
#[must_use]
pub fn check_pr_status_by_number_rest(repo_root: &Path, pr_num: u32) -> PrStatus {
    check_pr_by_number_rest(repo_root, pr_num).status
}

/// One `pr-<N>` worktree's PR, as a single REST probe: its
/// [`PrStatus`] **and** the head commit the forge recorded for it.
///
/// The head SHA is what makes a force branch-delete provably safe (#5939,
/// mirroring `merge-pr.sh`'s #4100 rule): after a squash merge the branch is
/// never an ancestor of `main`, so `git branch -d` / `--merged` say nothing
/// useful, and the only sound question is "is this local branch tip exactly
/// what the forge merged?". Bundling it with the status keeps that answer to
/// the *same* API call the eligibility gate already makes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrProbe {
    /// Open / merged / closed-without-merge / not-found / unresolvable.
    pub status: PrStatus,
    /// `head.sha` as the forge reported it, when it was resolvable.
    pub head_sha: Option<String>,
}

impl PrProbe {
    /// The all-unknown result every failure path resolves to — always a skip.
    #[must_use]
    pub fn unknown() -> Self {
        Self {
            status: PrStatus::Unknown,
            head_sha: None,
        }
    }
}

/// The full single-PR REST probe behind [`check_pr_status_by_number_rest`]
/// (issue #5939): one `gh api repos/{owner}/{repo}/pulls/<n>` call yielding
/// both the eligibility status and the head SHA.
#[must_use]
pub fn check_pr_by_number_rest(repo_root: &Path, pr_num: u32) -> PrProbe {
    let mut cmd = Command::new("gh");
    cmd.args(["api", &format!("repos/{{owner}}/{{repo}}/pulls/{pr_num}")])
        .current_dir(repo_root);
    // #5401/#5431: cross-owner managed repo -> its own owner's installation-token
    // GH_CONFIG_DIR (no-op for single-owner fleets / the root owner).
    crate::credential_preflight::apply_gh_config_for_root(&mut cmd, repo_root);
    let Ok(out) = cmd.output() else {
        return PrProbe::unknown();
    };
    if !out.status.success() {
        return PrProbe::unknown();
    }
    let Ok(row) = serde_json::from_slice::<PrRowRest>(&out.stdout) else {
        return PrProbe::unknown();
    };
    PrProbe {
        status: classify_pr_row(
            row.state.as_str(),
            row.merged_at.as_deref(),
            row.closed_at.as_deref(),
        ),
        head_sha: row
            .head
            .and_then(|h| h.sha)
            .filter(|s| !s.trim().is_empty()),
    }
}

/// Whether the grace period since `merged_at` has passed. Pure and
/// unit-testable — mirrors `clean.py::check_grace_period`.
#[must_use]
pub fn check_grace_period(
    merged_at: DateTime<Utc>,
    grace_period_secs: i64,
    now: DateTime<Utc>,
) -> (bool, i64) {
    let elapsed = now.signed_duration_since(merged_at).num_seconds();
    if elapsed > grace_period_secs {
        (true, 0)
    } else {
        (false, grace_period_secs - elapsed)
    }
}

/// Find pip packages with editable installs pointing into `worktree_path`.
/// Best-effort; mirrors `clean.py::find_editable_pip_installs` closely enough
/// to preserve the safety gate (skip removal when present, unless `--force`).
#[must_use]
pub fn find_editable_pip_installs(worktree_path: &Path) -> Vec<String> {
    let worktree_str = worktree_path
        .canonicalize()
        .unwrap_or_else(|_| worktree_path.to_path_buf());
    let worktree_str = worktree_str.to_string_lossy().to_string();

    let mut interpreters: Vec<String> = Vec::new();
    for name in ["python3", "python"] {
        if let Ok(out) = Command::new("which").arg(name).output() {
            if out.status.success() {
                let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if !path.is_empty() && !interpreters.contains(&path) {
                    interpreters.push(path);
                }
            }
        }
    }
    for candidate in [worktree_path.join(".venv").join("bin").join("python")] {
        if candidate.is_file() {
            let s = candidate.to_string_lossy().to_string();
            if !interpreters.contains(&s) {
                interpreters.push(s);
            }
        }
    }

    let mut packages: Vec<String> = Vec::new();
    for interpreter in &interpreters {
        let Ok(out) = Command::new(interpreter)
            .args(["-m", "pip", "list", "--editable", "--format=json"])
            .output()
        else {
            continue;
        };
        if !out.status.success() {
            continue;
        }
        let Ok(list) = serde_json::from_slice::<Vec<serde_json::Value>>(&out.stdout) else {
            continue;
        };
        for pkg in list {
            let Some(name) = pkg.get("name").and_then(|v| v.as_str()) else {
                continue;
            };
            let Ok(show) = Command::new(interpreter)
                .args(["-m", "pip", "show", name])
                .output()
            else {
                continue;
            };
            if !show.status.success() {
                continue;
            }
            let show_text = String::from_utf8_lossy(&show.stdout);
            for line in show_text.lines() {
                if let Some(loc) = line
                    .strip_prefix("Editable project location:")
                    .or_else(|| line.strip_prefix("Location:"))
                {
                    let loc = loc.trim();
                    if loc.starts_with(&worktree_str) && !packages.iter().any(|p| p == name) {
                        packages.push(name.to_string());
                    }
                    break;
                }
            }
        }
    }
    packages
}

// ============================================================================
// Removal decision (shared by the `clean` CLI and the daemon-side reaper)
// ============================================================================

/// Whether `worktree_path` carries the `.loom-managed` sentinel that
/// `worktree.sh` drops into every Loom-provisioned worktree. A worktree
/// without it is user-provisioned and must never be auto-removed (CLAUDE.md:
/// "user-provisioned worktrees are never removed").
#[must_use]
pub fn is_loom_managed(worktree_path: &Path) -> bool {
    worktree_path.join(".loom-managed").is_file()
}

/// The outcome of applying every worktree-removal safety gate to one worktree.
///
/// Extracted from [`clean_worktrees`] (issue #4876) so the **decision** has a
/// single implementation shared by the interactive `loom-daemon clean` CLI and
/// the daemon-side periodic reaper ([`crate::worktree_reaper`]). A second
/// hand-rolled copy of these gates in the reaper is exactly the divergence that
/// would let an unattended remover delete something the manual path preserves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorktreeDecision {
    /// Every gate passed — the worktree is eligible for removal.
    Remove,
    /// Held by a live spawn-loop task/claim-lock, a `.loom-in-use` marker, or a
    /// process whose cwd is inside the worktree.
    SkipInUse(String),
    /// Editable pip install(s) point into the worktree (payload: the package list).
    SkipEditable(String),
    /// No `.loom-managed` sentinel and [`CleanOptions::require_managed_sentinel`]
    /// is set — user-provisioned, never auto-removed.
    SkipUnmanaged,
    /// The issue is not `CLOSED` (payload: the observed state, e.g. `OPEN` /
    /// `UNKNOWN`).
    SkipIssueNotClosed(String),
    /// The PR merged, but the post-merge grace period has not elapsed
    /// (payload: seconds remaining).
    SkipGrace(i64),
    /// The PR closed without merging, but the (longer)
    /// [`CLOSED_NO_MERGE_GRACE_PERIOD_SECS`] since it closed has not elapsed
    /// yet (payload: seconds remaining) — issue #6418.
    SkipClosedNoMergeGrace(i64),
    /// No PR was ever opened for this closed issue, but the (longer)
    /// [`NO_PR_GRACE_PERIOD_SECS`] since the issue closed has not elapsed yet
    /// (payload: seconds remaining) — issue #6653.
    SkipNoPrGrace(i64),
    /// Uncommitted work in the worktree would be lost.
    SkipUncommitted,
    /// Every removal gate passed, including the grace period, but the
    /// worktree has uncommitted/untracked changes (issue #6653). Distinct
    /// from [`Self::SkipUncommitted`] (which preserves the worktree
    /// untouched): here, the grace period elapsing means the caller MUST
    /// quarantine-stash the changes first (never silently discard them),
    /// log the resulting stash ref, and only then remove the worktree —
    /// see `worktree_ops::clean::quarantine_dirty_worktree` and
    /// `worktree_reaper`'s reap loop.
    RemoveWithQuarantine,
    /// Closed issue whose PR did not merge, or that has no PR at all
    /// (payload: which). Also covers a closed-without-merge PR whose branch
    /// could not be proven safe to delete — no `closed_at` timestamp, or its
    /// commits are not fully reachable from any remote ref (issue #6418).
    SkipNotMerged(String),
    /// The PR is still open.
    SkipPrOpen,
    /// The PR status could not be determined (forge probe failed).
    SkipUnknownPrStatus,
    /// Non-`--safe` mode: the issue is closed and the caller decides
    /// (interactive prompt, or `--force`).
    ConfirmClosedIssue,
}

impl WorktreeDecision {
    /// True only for [`WorktreeDecision::Remove`].
    #[must_use]
    pub fn is_remove(&self) -> bool {
        matches!(self, Self::Remove)
    }
}

/// Injected probes for [`classify_worktree`], so the safety decision is
/// unit-testable without a live forge, process table, pip, or clock — the same
/// dependency-injection shape [`SweepTransientEnv`] already uses in this module.
pub struct WorktreeProbes<'a> {
    /// Issues with a live spawn-loop task or claim-lock.
    pub active_issues: &'a std::collections::HashSet<u32>,
    /// Reads a worktree's `.loom-in-use` marker.
    pub in_use_marker: &'a dyn Fn(&Path) -> Option<InUseMarker>,
    /// PIDs whose cwd is inside the worktree.
    pub processes_using: &'a dyn Fn(&Path) -> Vec<u32>,
    /// Editable pip installs pointing into the worktree.
    pub editable_installs: &'a dyn Fn(&Path) -> Vec<String>,
    /// Whether the worktree carries the `.loom-managed` sentinel.
    pub is_managed: &'a dyn Fn(&Path) -> bool,
    /// Whether `worktree_path` still appears in `git worktree list` — i.e.
    /// git has an administrative record of it. `false` means the directory
    /// is either user-created (never a git worktree at all) or an orphan
    /// whose git-side registration was pruned elsewhere while the directory
    /// itself was left behind (issue #6652 — the `.loom/worktrees/issue-4343`
    /// residue). Production wiring must fail closed: "cannot determine"
    /// means "assume registered", never "assume orphaned".
    pub is_registered_worktree: &'a dyn Fn(&Path) -> bool,
    /// Forge issue state (`"OPEN"` / `"CLOSED"` / `"UNKNOWN"`).
    pub issue_state: &'a dyn Fn(u32) -> String,
    /// The issue's own `closed_at` timestamp (issue #6653) — the safety
    /// criterion gating [`PrStatus::NoPr`]'s grace period, since there is no
    /// PR `closedAt`/`mergedAt` to read when no PR was ever opened. `None`
    /// when unresolvable (probe failure, or the issue isn't actually
    /// closed). Never consulted for any other [`PrStatus`] arm.
    pub issue_closed_at: &'a dyn Fn(u32) -> Option<String>,
    /// Forge PR status for the issue's branch.
    pub pr_status: &'a dyn Fn(u32) -> PrStatus,
    /// Whether every commit on the issue's branch (`naming::branch_name`) is
    /// reachable from some remote ref (issue #6418) — the safety criterion
    /// gating removal of a `ClosedNoMerge` worktree's directory. Never
    /// consulted for any other [`PrStatus`].
    pub branch_reachable_from_remotes: &'a dyn Fn(u32) -> bool,
    /// Whether the worktree has uncommitted changes.
    pub uncommitted: &'a dyn Fn(&Path) -> bool,
    /// Wall clock the grace-period gate measures against.
    pub now: DateTime<Utc>,
}

/// Apply every worktree-removal safety gate, in the order `clean.py` /
/// [`clean_worktrees`] has always applied them, and report the outcome.
///
/// Pure decision logic: performs no removal, prints nothing, and mutates
/// nothing. Callers map the returned [`WorktreeDecision`] onto their own
/// reporting (stdout for the CLI, `log::` for the daemon reaper) and act only
/// on [`WorktreeDecision::Remove`].
#[must_use]
pub fn classify_worktree(
    worktree_path: &Path,
    issue_num: u32,
    opts: &CleanOptions,
    probes: &WorktreeProbes<'_>,
) -> WorktreeDecision {
    if !opts.force && probes.active_issues.contains(&issue_num) {
        return WorktreeDecision::SkipInUse(format!(
            "issue #{issue_num} has a live spawn-loop task or claim-lock"
        ));
    }

    if let Some(marker) = (probes.in_use_marker)(worktree_path) {
        return WorktreeDecision::SkipInUse(format!(
            "in use by shepherd (task: {}, pid: {})",
            marker.task_id, marker.pid
        ));
    }

    if !opts.force {
        let active_pids = (probes.processes_using)(worktree_path);
        if !active_pids.is_empty() {
            return WorktreeDecision::SkipInUse(format!(
                "active process(es) using worktree: {active_pids:?}"
            ));
        }
    }

    if !opts.force {
        let editable_pkgs = (probes.editable_installs)(worktree_path);
        if !editable_pkgs.is_empty() {
            return WorktreeDecision::SkipEditable(editable_pkgs.join(", "));
        }
    }

    // Orphaned-directory gate (#6652): a directory `git worktree list` no
    // longer knows about was never a live worktree to begin with (or stopped
    // being one when its registration was pruned independently, e.g. a crash
    // mid-`git worktree prune`). The forge-state gates below (issue closed?
    // PR merged? grace period?) can only ever produce a skip for it — there
    // is no PR/issue state that makes an *unregistered* directory eligible
    // through that path, so it would be stranded forever (exactly the
    // `.loom/worktrees/issue-4343` residue that motivated this check: 12 MB,
    // unregistered, issue long closed, never reclaimed). Route straight to
    // the existing untracked-orphan removal fallback in `cleanup_worktree`
    // instead, bypassing the issue/PR checks entirely.
    //
    // The `.loom-managed` sentinel gate here is UNCONDITIONAL — never gated
    // by `opts.require_managed_sentinel` the way the registered-worktree
    // sentinel check below is — because a user-provisioned directory must
    // never be auto-removed regardless of which caller (CLI or reaper) is
    // asking.
    if !(probes.is_registered_worktree)(worktree_path) {
        return if (probes.is_managed)(worktree_path) {
            WorktreeDecision::Remove
        } else {
            WorktreeDecision::SkipUnmanaged
        };
    }

    // Sentinel gate (#4876): only the unattended reaper opts into this. It sits
    // AFTER the in-use gates so a user-provisioned worktree that is also busy
    // still reports the more specific "in use" reason.
    if opts.require_managed_sentinel && !(probes.is_managed)(worktree_path) {
        return WorktreeDecision::SkipUnmanaged;
    }

    let issue_state = (probes.issue_state)(issue_num);
    if issue_state != "CLOSED" {
        return WorktreeDecision::SkipIssueNotClosed(issue_state);
    }

    if !opts.safe {
        return WorktreeDecision::ConfirmClosedIssue;
    }

    match (probes.pr_status)(issue_num) {
        PrStatus::Merged { merged_at } => {
            if !opts.force {
                if let Ok(dt) = DateTime::parse_from_rfc3339(&merged_at) {
                    let (passed, remaining) = check_grace_period(
                        dt.with_timezone(&Utc),
                        opts.grace_period_secs,
                        probes.now,
                    );
                    if !passed {
                        return WorktreeDecision::SkipGrace(remaining);
                    }
                }
                if (probes.uncommitted)(worktree_path) {
                    // #6653: the grace period already elapsed — `main` has
                    // every commit this worktree could ever contribute, so
                    // the only thing left to protect is uncommitted local
                    // edits. Quarantine them rather than holding the
                    // worktree forever.
                    return WorktreeDecision::RemoveWithQuarantine;
                }
            }
            WorktreeDecision::Remove
        }
        PrStatus::ClosedNoMerge { closed_at } => {
            // #6418: unlike a merged PR, `main` never holds these commits, so
            // removal requires proof the branch is fully pushed to a remote
            // — checked unconditionally, never bypassed by `--force` — before
            // even considering the (longer) grace period below.
            if !(probes.branch_reachable_from_remotes)(issue_num) {
                return WorktreeDecision::SkipNotMerged(
                    "PR closed without merge, branch not fully pushed to a remote".to_string(),
                );
            }
            if !opts.force {
                let Some(dt) = closed_at
                    .as_deref()
                    .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                else {
                    // No resolvable close time — fail closed rather than
                    // treat an unknown elapsed time as "grace period passed".
                    return WorktreeDecision::SkipNotMerged(
                        "PR closed without merge (close time unknown)".to_string(),
                    );
                };
                let (passed, remaining) = check_grace_period(
                    dt.with_timezone(&Utc),
                    CLOSED_NO_MERGE_GRACE_PERIOD_SECS,
                    probes.now,
                );
                if !passed {
                    return WorktreeDecision::SkipClosedNoMergeGrace(remaining);
                }
                if (probes.uncommitted)(worktree_path) {
                    // #6653: same rationale as the `Merged` arm above — the
                    // branch is already provably on a remote (checked
                    // above) and its own grace period elapsed, so
                    // uncommitted edits are quarantined rather than
                    // pinning the worktree forever.
                    return WorktreeDecision::RemoveWithQuarantine;
                }
            }
            WorktreeDecision::Remove
        }
        PrStatus::Open => WorktreeDecision::SkipPrOpen,
        PrStatus::NoPr => {
            // #6653: a closed issue whose Builder claim never got as far as
            // `gh pr create` (e.g. closed as a duplicate/not-planned mid-
            // session) used to be skipped unconditionally, with no grace
            // period and no reclaim path at all. Same safety posture as
            // `ClosedNoMerge` above: `main` never received these commits
            // either, so proof the branch is fully pushed to a remote is
            // required, unconditionally, before a (long) grace period is
            // even considered.
            if !(probes.branch_reachable_from_remotes)(issue_num) {
                return WorktreeDecision::SkipNotMerged(
                    "no PR found for closed issue, branch not fully pushed to a remote".to_string(),
                );
            }
            if !opts.force {
                let Some(dt) = (probes.issue_closed_at)(issue_num)
                    .as_deref()
                    .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                else {
                    // No resolvable issue-close time — fail closed rather
                    // than treat an unknown elapsed time as "grace period
                    // passed".
                    return WorktreeDecision::SkipNotMerged(
                        "no PR found for closed issue (close time unknown)".to_string(),
                    );
                };
                let (passed, remaining) =
                    check_grace_period(dt.with_timezone(&Utc), NO_PR_GRACE_PERIOD_SECS, probes.now);
                if !passed {
                    return WorktreeDecision::SkipNoPrGrace(remaining);
                }
                if (probes.uncommitted)(worktree_path) {
                    return WorktreeDecision::RemoveWithQuarantine;
                }
            }
            WorktreeDecision::Remove
        }
        PrStatus::Unknown => WorktreeDecision::SkipUnknownPrStatus,
    }
}

/// The canonicalized path of every currently-registered git worktree for
/// `repo_root` (including the primary checkout itself), parsed from `git
/// worktree list --porcelain`.
///
/// `None` on any `git` failure (not installed, not a repo, I/O error) —
/// callers MUST treat "cannot determine" as "assume registered" (fail
/// closed), never as grounds to route a directory into the orphan-removal
/// path. See [`WorktreeProbes::is_registered_worktree`] (#6652).
#[must_use]
pub fn registered_worktree_paths(repo_root: &Path) -> Option<std::collections::HashSet<PathBuf>> {
    let out = Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(repo_root)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut set = std::collections::HashSet::new();
    for line in stdout.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            let p = PathBuf::from(path.trim());
            set.insert(p.canonicalize().unwrap_or(p));
        }
    }
    Some(set)
}

/// Build the production [`WorktreeProbes`] for `repo_root`, wiring each gate to
/// its real implementation. Split from [`classify_worktree`] so tests can
/// substitute scripted probes without touching the classifier.
///
/// The returned struct borrows `active_issues` and the caller-provided closures
/// (`issue_state_fn` / `issue_closed_at_fn` / `pr_status_fn` /
/// `branch_reachable_fn` / `is_registered_fn` capture `repo_root`), which is
/// why those are parameters rather than being constructed here.
#[must_use]
pub fn production_probes<'a>(
    active_issues: &'a std::collections::HashSet<u32>,
    issue_state_fn: &'a dyn Fn(u32) -> String,
    issue_closed_at_fn: &'a dyn Fn(u32) -> Option<String>,
    pr_status_fn: &'a dyn Fn(u32) -> PrStatus,
    branch_reachable_fn: &'a dyn Fn(u32) -> bool,
    is_registered_fn: &'a dyn Fn(&Path) -> bool,
    now: DateTime<Utc>,
) -> WorktreeProbes<'a> {
    WorktreeProbes {
        active_issues,
        in_use_marker: &read_in_use_marker,
        processes_using: &find_processes_using_directory,
        editable_installs: &find_editable_pip_installs,
        is_managed: &is_loom_managed,
        is_registered_worktree: is_registered_fn,
        issue_state: issue_state_fn,
        issue_closed_at: issue_closed_at_fn,
        pr_status: pr_status_fn,
        branch_reachable_from_remotes: branch_reachable_fn,
        uncommitted: &check_uncommitted_changes,
        now,
    }
}

/// Build the `is_registered_worktree` probe closure for [`production_probes`]
/// from a pre-fetched [`registered_worktree_paths`] snapshot — fetched once
/// per pass (not once per worktree) by the caller, the same memoization
/// shape `reap_repo`'s `owner`/`pr_cache` already use. Fails closed: `None`
/// (the snapshot could not be taken) reports every path as registered.
pub fn is_registered_worktree_probe(
    registered: &Option<std::collections::HashSet<PathBuf>>,
) -> impl Fn(&Path) -> bool + '_ {
    move |p: &Path| match registered {
        Some(set) => {
            let canon = p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
            set.contains(&canon)
        }
        None => true,
    }
}

// ============================================================================
// pr-<N> worktrees (issue #5939)
// ============================================================================
//
// A `pr-<N>` worktree (`.loom/scripts/pr-worktree.sh`) has no Loom issue
// backing it — its own PR is the sole unit of eligibility, checked directly by
// PR number rather than through the `feature/issue-<N>` branch-name heuristic
// [`WorktreeProbes::pr_status`] relies on. That is the reason this is a
// dedicated probe struct + classifier rather than a reuse of
// [`WorktreeProbes`]/[`classify_worktree`] with dummy issue fields: there is no
// issue number to gate on, spawn-loop claim-locks are keyed by issue number too
// (so "active issue" liveness does not apply here), and the branch checked out
// in a `pr-<N>` worktree is whatever `gh pr checkout` produced, not a name Loom
// controls.

/// Injected probes for [`classify_pr_worktree`] — the `pr-<N>` counterpart of
/// [`WorktreeProbes`].
pub struct PrWorktreeProbes<'a> {
    /// Reads a worktree's `.loom-in-use` marker.
    pub in_use_marker: &'a dyn Fn(&Path) -> Option<InUseMarker>,
    /// PIDs whose cwd is inside the worktree.
    pub processes_using: &'a dyn Fn(&Path) -> Vec<u32>,
    /// Editable pip installs pointing into the worktree.
    pub editable_installs: &'a dyn Fn(&Path) -> Vec<String>,
    /// Whether the worktree carries the `.loom-managed` sentinel.
    pub is_managed: &'a dyn Fn(&Path) -> bool,
    /// Forge PR status, keyed directly on the PR number (not a branch-name
    /// search — see [`check_pr_status_by_number_rest`]).
    pub pr_status: &'a dyn Fn(u32) -> PrStatus,
    /// Whether every commit on the worktree's checked-out branch is reachable
    /// from some remote ref (issue #6418) — the safety criterion gating
    /// removal of a `ClosedNoMerge` worktree's directory. Keyed by worktree
    /// path rather than a branch name: unlike `issue-<N>`, the branch a
    /// `pr-<N>` worktree has checked out is whatever `gh pr checkout`
    /// produced, not one Loom constructed. Never consulted for any other
    /// [`PrStatus`].
    pub branch_reachable_from_remotes: &'a dyn Fn(&Path) -> bool,
    /// Whether the worktree has uncommitted changes.
    pub uncommitted: &'a dyn Fn(&Path) -> bool,
    /// Wall clock the grace-period gate measures against.
    pub now: DateTime<Utc>,
}

/// Apply the worktree-removal safety gates to a `pr-<N>` worktree and report
/// the outcome. The `pr-<N>` counterpart of [`classify_worktree`] — same
/// in-use / editable-install / sentinel / grace-period / uncommitted-changes
/// gates, minus the issue-closed check (there is no issue), and the PR status
/// is resolved directly from `pr_num` rather than a `feature/issue-<N>`
/// branch-name search.
///
/// Pure decision logic: performs no removal, prints nothing, and mutates
/// nothing.
#[must_use]
pub fn classify_pr_worktree(
    worktree_path: &Path,
    pr_num: u32,
    opts: &CleanOptions,
    probes: &PrWorktreeProbes<'_>,
) -> WorktreeDecision {
    if let Some(marker) = (probes.in_use_marker)(worktree_path) {
        return WorktreeDecision::SkipInUse(format!(
            "in use by shepherd (task: {}, pid: {})",
            marker.task_id, marker.pid
        ));
    }

    if !opts.force {
        let active_pids = (probes.processes_using)(worktree_path);
        if !active_pids.is_empty() {
            return WorktreeDecision::SkipInUse(format!(
                "active process(es) using worktree: {active_pids:?}"
            ));
        }
    }

    if !opts.force {
        let editable_pkgs = (probes.editable_installs)(worktree_path);
        if !editable_pkgs.is_empty() {
            return WorktreeDecision::SkipEditable(editable_pkgs.join(", "));
        }
    }

    // Sentinel gate (#4876): same rationale as `classify_worktree` — an
    // unattended remover must honor "user-provisioned worktrees are never
    // removed".
    if opts.require_managed_sentinel && !(probes.is_managed)(worktree_path) {
        return WorktreeDecision::SkipUnmanaged;
    }

    if !opts.safe {
        // Reused from the issue-keyed enum rather than adding a
        // `pr-<N>`-specific variant: unreachable with the reaper's `safe: true`
        // options (its only caller today), and a future non-`--safe` caller of
        // this function must never be interpreted as "remove it" either way.
        return WorktreeDecision::ConfirmClosedIssue;
    }

    match (probes.pr_status)(pr_num) {
        PrStatus::Merged { merged_at } => {
            if !opts.force {
                if let Ok(dt) = DateTime::parse_from_rfc3339(&merged_at) {
                    let (passed, remaining) = check_grace_period(
                        dt.with_timezone(&Utc),
                        opts.grace_period_secs,
                        probes.now,
                    );
                    if !passed {
                        return WorktreeDecision::SkipGrace(remaining);
                    }
                }
                if (probes.uncommitted)(worktree_path) {
                    return WorktreeDecision::SkipUncommitted;
                }
            }
            WorktreeDecision::Remove
        }
        PrStatus::ClosedNoMerge { closed_at } => {
            // #6418: same rationale as `classify_worktree`'s arm — proof the
            // branch is fully pushed to a remote is a hard invariant, never
            // bypassed by `--force`, checked before the (longer) grace period.
            if !(probes.branch_reachable_from_remotes)(worktree_path) {
                return WorktreeDecision::SkipNotMerged(
                    "PR closed without merge, branch not fully pushed to a remote".to_string(),
                );
            }
            if !opts.force {
                let Some(dt) = closed_at
                    .as_deref()
                    .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                else {
                    return WorktreeDecision::SkipNotMerged(
                        "PR closed without merge (close time unknown)".to_string(),
                    );
                };
                let (passed, remaining) = check_grace_period(
                    dt.with_timezone(&Utc),
                    CLOSED_NO_MERGE_GRACE_PERIOD_SECS,
                    probes.now,
                );
                if !passed {
                    return WorktreeDecision::SkipClosedNoMergeGrace(remaining);
                }
                if (probes.uncommitted)(worktree_path) {
                    return WorktreeDecision::SkipUncommitted;
                }
            }
            WorktreeDecision::Remove
        }
        PrStatus::Open => WorktreeDecision::SkipPrOpen,
        PrStatus::NoPr => WorktreeDecision::SkipNotMerged("PR not found".to_string()),
        PrStatus::Unknown => WorktreeDecision::SkipUnknownPrStatus,
    }
}

/// Build the production [`PrWorktreeProbes`] for `repo_root`. The `pr-<N>`
/// counterpart of [`production_probes`] — split out the same way, so tests can
/// substitute scripted probes without touching the classifier.
///
/// `branch_reachable_fn` is a caller-provided closure (captures `repo_root`)
/// for the same reason `pr_status_fn` is: keeping this function itself free
/// of any `repo_root` parameter or git/forge I/O of its own.
#[must_use]
pub fn production_pr_probes<'a>(
    pr_status_fn: &'a dyn Fn(u32) -> PrStatus,
    branch_reachable_fn: &'a dyn Fn(&Path) -> bool,
    now: DateTime<Utc>,
) -> PrWorktreeProbes<'a> {
    PrWorktreeProbes {
        in_use_marker: &read_in_use_marker,
        processes_using: &find_processes_using_directory,
        editable_installs: &find_editable_pip_installs,
        is_managed: &is_loom_managed,
        pr_status: pr_status_fn,
        branch_reachable_from_remotes: branch_reachable_fn,
        // Wider than the issue-keyed pass's probe (#5939 review): `git diff`
        // alone is blind to untracked files, and `git worktree remove --force`
        // deletes them. A `pr-<N>` worktree's contents come from outside Loom,
        // so it does not get the closed-issue gate that bounds that gap on the
        // `issue-<N>` side.
        uncommitted: &check_uncommitted_or_untracked_changes,
        now,
    }
}

/// Whether a `git worktree remove` failure means "this path is not a git
/// worktree" — the signature (#5177) of an orphaned `.loom/worktrees/*`
/// directory git no longer tracks (e.g. a `git worktree prune` ran while the
/// directory was left on disk). Any *other* removal failure (a lock, a busy
/// path, a permission error) is NOT this case and must not trigger the
/// direct-removal fallback.
#[must_use]
pub fn is_untracked_worktree_error(cause: &str) -> bool {
    cause.to_ascii_lowercase().contains("is not a working tree")
}

/// Whether `worktree_path` is safely inside `repo_root`'s managed worktree root
/// ([`crate::worktree_root::worktree_root`]). A precondition for the
/// direct-removal fallback (#5177): even after confirming the git error and the
/// `.loom-managed` sentinel, never `remove_dir_all` a path outside the tree
/// Loom provisions worktrees into.
#[must_use]
pub fn is_under_worktree_root(repo_root: &Path, worktree_path: &Path) -> bool {
    let root = crate::worktree_root::worktree_root(repo_root);
    let root = root.canonicalize().unwrap_or(root);
    let wt = worktree_path
        .canonicalize()
        .unwrap_or_else(|_| worktree_path.to_path_buf());
    wt != root && wt.starts_with(&root)
}

/// Decide whether a failed `git worktree remove` should fall back to a direct
/// directory removal (#5177). Pure, so the untracked-orphan path is testable
/// without a git repo. All three conditions must hold — the specific "not a
/// working tree" git error, the `.loom-managed` sentinel, and containment under
/// the managed worktree root — so this never degrades into a blanket `rm -rf`
/// on any removal failure.
#[must_use]
pub fn should_force_remove_orphan_dir(cause: &str, is_managed: bool, under_root: bool) -> bool {
    is_untracked_worktree_error(cause) && is_managed && under_root
}

/// What [`cleanup_worktree`] actually did on success.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CleanupOutcome {
    /// The worktree (or its untracked directory) was actually removed —
    /// `git worktree remove` succeeded, the #5177 direct-removal fallback
    /// ran, or this was a `--dry-run` "would remove".
    Removed,
    /// `git worktree remove` failed with "is not a working tree" and no
    /// directory existed on disk either (#5895) — there was nothing left to
    /// remove, so this is not a failure, just a no-op cleanup of a stale
    /// registration.
    AlreadyGone,
}

/// Remove a worktree and delete its feature branch.
///
/// Exposed (issue #4876) so the daemon-side reaper performs the *same*
/// removal the CLI does, including the `git branch -d` → `-D` fallback.
///
/// `Err` carries the underlying cause (git's stderr) so the caller can name it
/// in a diagnostic (issue #4877).
///
/// `mechanism` names the caller in the removal ledger (#5950) — `"clean"` for
/// the interactive pass, `"worktree_reaper"` for the unattended one. It is
/// recorded only on a removal that actually happened.
pub fn cleanup_worktree(
    repo_root: &Path,
    worktree_path: &Path,
    issue_num: u32,
    dry_run: bool,
    mechanism: &str,
) -> Result<CleanupOutcome, String> {
    let branch_name = naming::branch_name(issue_num);
    if dry_run {
        println!("Would remove: {}", worktree_path.display());
        println!("Would delete branch: {branch_name}");
        return Ok(CleanupOutcome::Removed);
    }
    let mut remove = Command::new("git");
    remove
        .args(["worktree", "remove"])
        .arg(worktree_path)
        .arg("--force")
        .current_dir(repo_root);
    let outcome = if let Err(cause) = run_checked(remove) {
        // #5895: git has no record of this path as a worktree at all. Two
        // physical states are possible: the directory still exists on disk
        // (the #5177 untracked-orphan shape, handled below) or it has
        // already been removed by something other than `git worktree
        // remove` — a manual `rm -rf`, an interrupted sweep, or a race with
        // the daemon's own periodic reaper. Either way there is nothing
        // unsafe left to remove, so this is "already gone", not an error —
        // it never contributes to the caller's error tally.
        if is_untracked_worktree_error(&cause) && !worktree_path.exists() {
            println!(
                "  Already removed (stale git registration, no directory on disk): {}",
                worktree_path.display()
            );
            let _ = Command::new("git")
                .args(["worktree", "prune"])
                .current_dir(repo_root)
                .status();
            CleanupOutcome::AlreadyGone
        } else if should_force_remove_orphan_dir(
            // #5177: an orphaned `.loom/worktrees/*` directory git no longer
            // tracks fails `git worktree remove` with "is not a working
            // tree" and could never be cleaned by the normal path. Fall back
            // to a direct removal — but ONLY when we can prove this is a
            // Loom-managed worktree path (the specific git error + the
            // `.loom-managed` sentinel + containment under the managed
            // worktree root), never a blanket rm on any failure.
            &cause,
            is_loom_managed(worktree_path),
            is_under_worktree_root(repo_root, worktree_path),
        ) {
            std::fs::remove_dir_all(worktree_path).map_err(|e| {
                format!(
                    "git worktree remove failed ({cause}); direct removal of the untracked \
                     worktree directory also failed: {e}"
                )
            })?;
            println!(
                "  Removed untracked worktree directory (no git worktree entry): {}",
                worktree_path.display()
            );
            let _ = Command::new("git")
                .args(["worktree", "prune"])
                .current_dir(repo_root)
                .status();
            CleanupOutcome::Removed
        } else {
            return Err(cause);
        }
    } else {
        println!("  Removed worktree: {}", worktree_path.display());
        CleanupOutcome::Removed
    };

    // #5950: name the responsible mechanism in the one place an operator can
    // correlate every worktree removal from, whatever path made the decision.
    super::removal_log::record(
        repo_root,
        mechanism,
        worktree_path,
        Some(&branch_name),
        "classify_worktree=Remove",
    );

    let deleted = Command::new("git")
        .args(["branch", "-d", &branch_name])
        .current_dir(repo_root)
        .status()
        .is_ok_and(|s| s.success());
    if !deleted {
        let _ = Command::new("git")
            .args(["branch", "-D", &branch_name])
            .current_dir(repo_root)
            .status();
    }
    Ok(outcome)
}

/// Push every uncommitted/untracked change in `worktree_path` into a
/// `loom-quarantine:`-labeled git stash (issue #6653), instead of silently
/// discarding it, when a worktree tied to a closed issue is reclaimed past
/// its grace period while still dirty (see
/// [`WorktreeDecision::RemoveWithQuarantine`]).
///
/// Mirrors `.loom/scripts/check-main-clean.sh --quarantine`'s rescue path
/// (`git stash push --include-untracked -m "loom-quarantine: $LABEL"`) —
/// reusing the same [`QUARANTINE_STASH_LABEL`] prefix means the resulting
/// stash is visible to the SAME `stash_retirement` / `quarantine_stash_status`
/// machinery that already lists (and, with explicit operator opt-in,
/// auto-retires) that script's rescue stashes; this never invents a second,
/// parallel bucket of stashes.
///
/// `refs/stash` is one ref shared by every linked worktree of a repo (not
/// per-worktree), so pushing from `worktree_path` still lands somewhere
/// `repo_root` (the primary checkout) can see and later retire.
///
/// Returns the pushed stash's commit sha — a durable `git stash apply <sha>`
/// recovery handle, the same identity `stash_retirement::QuarantineStashEntry::commit`
/// tracks — on a genuine push. Returns `None` when there was nothing to
/// stash (a race resolved the dirt between detection and this call — `git
/// stash push` exits 0 but creates no new entry) or the `git stash push`
/// itself failed. Both `None` cases are deliberately indistinguishable to
/// the caller: either way nothing was quarantined, so the caller must NOT
/// proceed to remove the worktree.
#[must_use]
pub fn quarantine_dirty_worktree(worktree_path: &Path, label: &str) -> Option<String> {
    let before = stash_ref_commit(worktree_path);
    let msg = format!("{QUARANTINE_STASH_LABEL} {label}");
    let status = Command::new("git")
        .args(["stash", "push", "--include-untracked", "-m", &msg])
        .current_dir(worktree_path)
        .status();
    if !status.is_ok_and(|s| s.success()) {
        return None;
    }
    let after = stash_ref_commit(worktree_path);
    if after.is_some() && after != before {
        after
    } else {
        None
    }
}

/// The current `refs/stash` tip's commit sha, or `None` if the ref does not
/// exist (no stash has ever been pushed in this repo). Used by
/// [`quarantine_dirty_worktree`] to detect a no-op `git stash push` the same
/// way `check-main-clean.sh --quarantine` does (its "remember the stack top
/// BEFORE pushing" comment).
fn stash_ref_commit(repo_or_worktree: &Path) -> Option<String> {
    let out = Command::new("git")
        .args(["rev-parse", "--verify", "--quiet", "refs/stash"])
        .current_dir(repo_or_worktree)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if sha.is_empty() {
        None
    } else {
        Some(sha)
    }
}

/// Record the outcome of a [`cleanup_worktree`] call against `stats`: an
/// actual removal, an already-gone stale registration (#5895 — never an
/// error), or a genuine failure.
fn record_cleanup_result(
    stats: &mut CleanupStats,
    worktree_path: &Path,
    result: Result<CleanupOutcome, String>,
) {
    match result {
        Ok(CleanupOutcome::Removed) => stats.cleaned_worktrees += 1,
        Ok(CleanupOutcome::AlreadyGone) => stats.stale_worktree_registrations += 1,
        Err(cause) => stats.record_error(
            &worktree_path.display().to_string(),
            "git worktree remove --force",
            &cause,
        ),
    }
}

/// Long-lived integration branches [`cleanup_pr_worktree`] must never delete,
/// however a `pr-<N>` worktree came to have one checked out (issue #5939).
///
/// [`cleanup_worktree`] needs no such list: the branch it deletes is always
/// `naming::branch_name(issue)`, a name Loom itself constructed. The `pr-<N>`
/// path is the one place the branch name comes from **outside** Loom — it is
/// whatever `gh pr checkout` produced — so it is the one place a wrong name
/// could reach `git branch -D`.
///
/// Mostly belt-and-braces: `git branch -d/-D` already refuses to delete a
/// branch that is checked out in *any* worktree, which protects the primary
/// checkout's own branch on its own. This covers the residual case where the
/// integration branch is not currently checked out anywhere.
///
/// This list is the **floor**, not the whole safety story — the load-bearing
/// gate is [`branch_delete_mode`]'s tip-matches-merged-head-SHA rule, which
/// bounds *every* branch name rather than an enumerated few. The list was
/// widened past `main`/`master`/`develop`/`trunk` in the #5939 review: nothing
/// in an allowlist of four names covered `staging`, `release/1.x`, or
/// `gh-pages`.
#[must_use]
pub fn is_protected_branch_name(branch: &str) -> bool {
    const EXACT: [&str; 10] = [
        "main",
        "master",
        "develop",
        "development",
        "trunk",
        "staging",
        "stage",
        "production",
        "prod",
        "gh-pages",
    ];
    // Release-train / long-lived line namespaces: `release/1.x`, `hotfix/2.3`,
    // `support/1.0`, `maint/4`, `stable/v2` and friends are integration
    // branches by convention wherever they appear, never a PR head Loom
    // provisioned a `pr-<N>` worktree for.
    const PREFIXES: [&str; 6] = [
        "release/",
        "releases/",
        "hotfix/",
        "support/",
        "maint/",
        "stable/",
    ];
    EXACT.contains(&branch) || PREFIXES.iter().any(|p| branch.starts_with(p))
}

/// How [`cleanup_pr_worktree`] may delete a `pr-<N>` worktree's local branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BranchDeleteMode {
    /// The local tip is exactly the head SHA the forge merged, so every commit
    /// on the branch shipped — `git branch -D` loses nothing.
    ForceSafe,
    /// The tip does not match (or could not be compared): try `git branch -d`
    /// only and let git's own "not fully merged" refusal keep the branch.
    /// **Never escalates to `-D`.**
    SafeOnly,
    /// The branch name is an integration branch — do not attempt a delete.
    Refuse,
}

/// Decide how a `pr-<N>` worktree's branch may be deleted (issue #5939).
///
/// This is `merge-pr.sh`'s `_maybe_delete_local_branch` rule (#4100) in Rust,
/// and it exists because the shape it replaces was unsound here. The old code
/// tried `git branch -d` and escalated to `git branch -D` on *any* failure —
/// but this repo squash-merges, so a merged PR's branch is never an ancestor
/// of `main`, `-d` therefore fails essentially always, and `-D` fired on every
/// single PR-worktree removal. A `pr-<N>` worktree carrying commits a Doctor
/// made locally and never pushed would have had them force-deleted with no
/// record.
///
/// The sound criterion is the one `merge-pr.sh` already uses: compare the
/// local branch tip against the head SHA the forge actually merged, and force
/// only on an exact match. Anything else — a mismatch, a missing local tip, a
/// forge probe that never returned a SHA — falls back to `-d`, which keeps and
/// reports the branch instead of destroying it.
///
/// Pure, so the policy is unit-testable without git.
#[must_use]
pub fn branch_delete_mode(
    branch: &str,
    local_tip: Option<&str>,
    merged_head_sha: Option<&str>,
) -> BranchDeleteMode {
    if is_protected_branch_name(branch) {
        return BranchDeleteMode::Refuse;
    }
    match (local_tip, merged_head_sha) {
        (Some(tip), Some(head)) if !tip.is_empty() && tip == head => BranchDeleteMode::ForceSafe,
        _ => BranchDeleteMode::SafeOnly,
    }
}

/// The commit `refs/heads/<branch>` points at in `repo_root`, or `None` if the
/// branch does not exist or `git` failed.
#[must_use]
pub fn local_branch_tip(repo_root: &Path, branch: &str) -> Option<String> {
    let out = Command::new("git")
        .args(["rev-parse", "--verify", "--quiet"])
        .arg(format!("refs/heads/{branch}"))
        .current_dir(repo_root)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if sha.is_empty() {
        None
    } else {
        Some(sha)
    }
}

/// Remove a `pr-<N>` worktree and delete its checked-out branch (issue #5939).
///
/// The `pr-<N>` counterpart of [`cleanup_worktree`]. Unlike an `issue-<N>`
/// worktree, a `pr-<N>` worktree's branch is not `naming::branch_name(N)` — it
/// is whatever `gh pr checkout` produced inside `.loom/scripts/pr-worktree.sh`
/// (the PR's own head ref for a same-repo PR, or a disambiguated local name for
/// a fork PR) — so the branch to delete is read from the worktree itself via
/// [`current_branch`] rather than constructed from `pr_num`. `pr_num` is kept
/// only to mirror [`cleanup_worktree`]'s signature (the shared `remove: &dyn
/// Fn(&Path, u32) -> bool` shape both reap passes use) and appears in the
/// dry-run message.
///
/// `Err` carries the underlying cause (git's stderr), same contract as
/// [`cleanup_worktree`].
///
/// `mechanism` names the caller in the removal ledger (#5950), exactly as
/// [`cleanup_worktree`]'s does — every worktree-removal path writes one line,
/// so both an entry and its absence are evidence. This function is the newest
/// and least-constrained of those paths, which makes it the one that most
/// needs to be in the ledger, not the one that may skip it.
///
/// `merged_head_sha` is the head commit the forge recorded for PR `pr_num`
/// (from [`check_pr_by_number_rest`]). It gates the branch delete via
/// [`branch_delete_mode`]: `git branch -D` only when the local tip matches it
/// exactly, otherwise a plain `git branch -d` that keeps and reports the
/// branch. Pass `None` when it could not be resolved — that is the safe side.
pub fn cleanup_pr_worktree(
    repo_root: &Path,
    worktree_path: &Path,
    pr_num: u32,
    dry_run: bool,
    mechanism: &str,
    merged_head_sha: Option<&str>,
) -> Result<(), String> {
    let branch_name = current_branch(worktree_path);
    // Read the branch tip BEFORE the worktree goes away: `git worktree remove`
    // does not touch `refs/heads/<branch>`, but resolving it first keeps the
    // safety comparison from depending on removal side effects.
    let local_tip = branch_name
        .as_deref()
        .and_then(|b| local_branch_tip(repo_root, b));
    if dry_run {
        println!("Would remove: {} (pr-{pr_num})", worktree_path.display());
        if let Some(branch) = &branch_name {
            match branch_delete_mode(branch, local_tip.as_deref(), merged_head_sha) {
                BranchDeleteMode::ForceSafe => println!(
                    "Would delete branch: {branch} (tip matches merged PR head SHA — safe \
                     force-delete)"
                ),
                BranchDeleteMode::SafeOnly => println!(
                    "Would try `git branch -d {branch}` only (tip does not match the merged PR \
                     head SHA — no force-delete)"
                ),
                BranchDeleteMode::Refuse => {
                    println!("Would preserve integration branch: {branch}");
                }
            }
        }
        return Ok(());
    }
    let mut remove = Command::new("git");
    remove
        .args(["worktree", "remove"])
        .arg(worktree_path)
        .arg("--force")
        .current_dir(repo_root);
    if let Err(cause) = run_checked(remove) {
        // #5177: same orphaned-directory fallback as `cleanup_worktree`.
        if should_force_remove_orphan_dir(
            &cause,
            is_loom_managed(worktree_path),
            is_under_worktree_root(repo_root, worktree_path),
        ) {
            std::fs::remove_dir_all(worktree_path).map_err(|e| {
                format!(
                    "git worktree remove failed ({cause}); direct removal of the untracked \
                     worktree directory also failed: {e}"
                )
            })?;
            println!(
                "  Removed untracked worktree directory (no git worktree entry): {}",
                worktree_path.display()
            );
            let _ = Command::new("git")
                .args(["worktree", "prune"])
                .current_dir(repo_root)
                .status();
        } else {
            return Err(cause);
        }
    } else {
        println!("  Removed worktree: {}", worktree_path.display());
    }

    // #5950: every worktree-removal path appends one ledger line, so both an
    // entry AND its absence are evidence. Written after the removal actually
    // happened and before the branch delete, mirroring `cleanup_worktree`.
    super::removal_log::record(
        repo_root,
        mechanism,
        worktree_path,
        branch_name.as_deref(),
        "classify_pr_worktree=Remove",
    );

    if let Some(branch_name) = branch_name {
        match branch_delete_mode(&branch_name, local_tip.as_deref(), merged_head_sha) {
            BranchDeleteMode::Refuse => {
                log::info!(
                    "worktree_ops: preserving integration branch '{branch_name}' after removing \
                     pr-{pr_num}'s worktree"
                );
            }
            BranchDeleteMode::ForceSafe => {
                // Every commit on the branch is part of what the forge merged
                // (tip == head SHA), so `-D` cannot lose anything — and it is
                // required, because a squash merge never satisfies `-d`.
                let _ = Command::new("git")
                    .args(["branch", "-D", &branch_name])
                    .current_dir(repo_root)
                    .status();
            }
            BranchDeleteMode::SafeOnly => {
                // `merge-pr.sh` #4100's fallback: try the safe delete and let
                // git's own "not fully merged" refusal keep the branch. NEVER
                // escalate to `-D` here — a tip that does not match the merged
                // head is exactly the local-work-that-was-never-pushed case.
                let deleted = Command::new("git")
                    .args(["branch", "-d", &branch_name])
                    .current_dir(repo_root)
                    .status()
                    .is_ok_and(|s| s.success());
                if !deleted {
                    log::warn!(
                        "worktree_ops: kept local branch '{branch_name}' after removing \
                         pr-{pr_num}'s worktree — its tip ({}) is not the merged PR head SHA \
                         ({}), so it may carry commits that were never pushed. Delete it by \
                         hand with `git branch -D {branch_name}` once you have checked.",
                        local_tip.as_deref().unwrap_or("unknown"),
                        merged_head_sha.unwrap_or("unknown"),
                    );
                }
            }
        }
    }
    Ok(())
}

/// Standard + `--safe` worktree cleanup pass. Mirrors `clean.py::clean_worktrees`.
pub fn clean_worktrees(repo_root: &Path, stats: &mut CleanupStats, opts: &CleanOptions) {
    let worktrees_dir = crate::worktree_root::worktree_root(repo_root);
    if !worktrees_dir.is_dir() {
        println!("No worktrees directory found");
        return;
    }

    // #5895 (AC1): deregister worktrees whose directory is already gone
    // *before* enumerating and attempting removal, so a registration git can
    // already see is stale never reaches the removal loop as a candidate
    // failure in the first place. Best-effort — a failure here does not
    // block the pass; the per-entry "is not a working tree" backstop in
    // `cleanup_worktree` still covers whatever this doesn't catch (a
    // registration git prunes here is, by construction, one whose directory
    // is already absent, so this alone would never have produced a `read_dir`
    // entry for `cleanup_worktree` to fail on).
    if !opts.dry_run {
        let _ = Command::new("git")
            .args(["worktree", "prune"])
            .current_dir(repo_root)
            .status();
    }

    let active_issues = active_spawn_loop_issues(repo_root);

    let Ok(entries) = std::fs::read_dir(&worktrees_dir) else {
        return;
    };
    let mut worktree_dirs: Vec<_> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with(naming::WORKTREE_PREFIX)
        })
        .collect();
    worktree_dirs.sort_by_key(std::fs::DirEntry::path);

    let issue_state_fn = |n: u32| gh::issue_state(repo_root, n);
    // #6653: REST is fine here even though `issue_state_fn` above uses the
    // GraphQL-backed `gh issue view` — this probe is only ever consulted for
    // a `PrStatus::NoPr` worktree (rare), so there is no meaningful GraphQL
    // quota pressure to avoid the way there is for the reaper's per-tick,
    // per-worktree probes.
    let issue_closed_at_fn = |n: u32| gh::issue_closed_at_rest(repo_root, n);
    let pr_status_fn = |n: u32| check_pr_merged(repo_root, n);
    let branch_reachable_fn =
        |n: u32| branch_reachable_from_remotes(repo_root, &naming::branch_name(n));
    // #6652: one `git worktree list` per pass, not once per worktree.
    let registered = registered_worktree_paths(repo_root);
    let is_registered_fn = is_registered_worktree_probe(&registered);
    let probes = production_probes(
        &active_issues,
        &issue_state_fn,
        &issue_closed_at_fn,
        &pr_status_fn,
        &branch_reachable_fn,
        &is_registered_fn,
        Utc::now(),
    );

    for entry in worktree_dirs {
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(issue_num) = naming::issue_from_worktree(&name) else {
            continue;
        };
        let worktree_path = entry.path().canonicalize().unwrap_or_else(|_| entry.path());

        println!("Checking worktree: issue-{issue_num}");

        match classify_worktree(&worktree_path, issue_num, opts, &probes) {
            WorktreeDecision::SkipInUse(reason) => {
                println!("  {reason} - preserving");
                stats.skipped_in_use += 1;
            }
            WorktreeDecision::SkipEditable(pkg_list) => {
                println!("  Editable pip install(s) found ({pkg_list}) - skipping");
                stats.skipped_editable += 1;
            }
            WorktreeDecision::SkipUnmanaged => {
                println!("  No .loom-managed sentinel (user-provisioned) - preserving");
                stats.skipped_in_use += 1;
            }
            WorktreeDecision::SkipIssueNotClosed(state) => {
                println!("  Issue #{issue_num} is {state} - preserving");
                stats.skipped_open += 1;
            }
            WorktreeDecision::SkipGrace(remaining) => {
                println!("  PR merged but grace period not passed ({remaining}s remaining)");
                stats.skipped_grace += 1;
            }
            WorktreeDecision::SkipClosedNoMergeGrace(remaining) => {
                println!(
                    "  PR closed without merge but grace period not passed ({remaining}s \
                     remaining)"
                );
                stats.skipped_grace += 1;
            }
            WorktreeDecision::SkipNoPrGrace(remaining) => {
                println!(
                    "  No PR found for closed issue but grace period not passed ({remaining}s \
                     remaining)"
                );
                stats.skipped_grace += 1;
            }
            WorktreeDecision::SkipUncommitted => {
                println!("  Uncommitted changes detected - skipping");
                stats.skipped_uncommitted += 1;
            }
            WorktreeDecision::SkipNotMerged(reason) => {
                println!("  {reason} - skipping (may need investigation)");
                stats.skipped_not_merged += 1;
            }
            WorktreeDecision::SkipPrOpen => {
                println!("  PR still open - skipping");
                stats.skipped_open += 1;
            }
            WorktreeDecision::SkipUnknownPrStatus => {
                stats.record_error(
                    &format!("issue #{issue_num} ({})", worktree_path.display()),
                    "PR status lookup (gh pr list)",
                    "unknown PR state - worktree left in place, re-run once `gh` can reach \
                     the forge",
                );
            }
            WorktreeDecision::Remove => {
                let result =
                    cleanup_worktree(repo_root, &worktree_path, issue_num, opts.dry_run, "clean");
                record_cleanup_result(stats, &worktree_path, result);
            }
            WorktreeDecision::RemoveWithQuarantine => {
                if opts.dry_run {
                    println!(
                        "  Would quarantine-stash uncommitted changes, then remove: {}",
                        entry.path().display()
                    );
                    stats.cleaned_worktrees += 1;
                } else {
                    let label = format!("issue={issue_num} reason=clean-dirty-closed-worktree");
                    match quarantine_dirty_worktree(&worktree_path, &label) {
                        Some(stash_sha) => {
                            println!(
                                "  Quarantined uncommitted changes to stash {stash_sha} \
                                 (recover with `git stash apply {stash_sha}`)"
                            );
                            let result = cleanup_worktree(
                                repo_root,
                                &worktree_path,
                                issue_num,
                                opts.dry_run,
                                "clean",
                            );
                            record_cleanup_result(stats, &worktree_path, result);
                        }
                        None => {
                            println!(
                                "  Could not quarantine uncommitted changes - preserving \
                                 worktree rather than risk losing them"
                            );
                            stats.skipped_uncommitted += 1;
                        }
                    }
                }
            }
            WorktreeDecision::ConfirmClosedIssue => {
                println!("  Issue #{issue_num} is CLOSED");
                if opts.dry_run {
                    println!("  Would remove: {}", entry.path().display());
                    stats.cleaned_worktrees += 1;
                } else if opts.force {
                    println!("  Auto-removing: {}", entry.path().display());
                    let result = cleanup_worktree(
                        repo_root,
                        &worktree_path,
                        issue_num,
                        opts.dry_run,
                        "clean",
                    );
                    record_cleanup_result(stats, &worktree_path, result);
                } else if confirm("  Force remove this worktree? [y/N] ") {
                    let result = cleanup_worktree(
                        repo_root,
                        &worktree_path,
                        issue_num,
                        opts.dry_run,
                        "clean",
                    );
                    record_cleanup_result(stats, &worktree_path, result);
                } else {
                    println!("  Skipping: {}", entry.path().display());
                    stats.skipped_open += 1;
                }
            }
        }
    }
}

pub fn prune_orphaned_worktrees(repo_root: &Path, dry_run: bool) {
    println!("\nPruning Orphaned References");
    let mut args = vec!["worktree", "prune"];
    if dry_run {
        args.push("--dry-run");
    }
    args.push("--verbose");
    let out = Command::new("git")
        .args(&args)
        .current_dir(repo_root)
        .output();
    match out {
        Ok(o) => {
            let stdout = String::from_utf8_lossy(&o.stdout);
            if !stdout.trim().is_empty() {
                println!("{}", stdout.trim());
            } else {
                println!("No orphaned worktrees to prune");
            }
        }
        Err(e) => eprintln!("Error pruning worktrees: {e}"),
    }
}

/// The branch currently checked out at `repo_root`, or `None` for a detached
/// HEAD or any `git` failure. Also used by [`crate::primary_checkout_reaper`]
/// (#5268) to identify a primary checkout parked on a non-default branch.
#[must_use]
pub fn current_branch(repo_root: &Path) -> Option<String> {
    let out = Command::new("git")
        .args(["symbolic-ref", "--short", "HEAD"])
        .current_dir(repo_root)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

fn checked_out_branches(repo_root: &Path) -> std::collections::HashSet<String> {
    let mut out_set = std::collections::HashSet::new();
    let Ok(out) = Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(repo_root)
        .output()
    else {
        return out_set;
    };
    if !out.status.success() {
        return out_set;
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    for line in stdout.lines() {
        if let Some(name) = line.strip_prefix("branch refs/heads/") {
            out_set.insert(name.trim().to_string());
        }
    }
    out_set
}

/// The repo's default branch, resolved from `origin/HEAD` — `None` if that
/// symbolic ref is unset (e.g. `git remote set-head origin` was never run) or
/// any `git` failure. Also used by [`crate::primary_checkout_reaper`] (#5268)
/// to know which branch a stale primary checkout should be returned to.
#[must_use]
pub fn default_branch(repo_root: &Path) -> Option<String> {
    let out = Command::new("git")
        .args(["symbolic-ref", "refs/remotes/origin/HEAD"])
        .current_dir(repo_root)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let ref_name = String::from_utf8_lossy(&out.stdout).trim().to_string();
    ref_name
        .strip_prefix("refs/remotes/origin/")
        .map(str::to_string)
}

fn remote_branch_exists(repo_root: &Path, branch: &str) -> bool {
    Command::new("git")
        .args(["show-ref", "--verify", "--quiet", &format!("refs/remotes/origin/{branch}")])
        .current_dir(repo_root)
        .status()
        .map(|s| s.success())
        // Fail closed: if we can't probe, claim the remote exists so we
        // don't delete a branch on a transient git error.
        .unwrap_or(true)
}

/// Force-delete one local branch. `Err` carries git's own message (e.g.
/// `error: branch 'x' not found.`) so a failure can be reported against the
/// branch that failed instead of vanishing into the error tally (#4877).
fn force_delete_branch(repo_root: &Path, branch: &str) -> Result<(), String> {
    let mut cmd = Command::new("git");
    cmd.args(["branch", "-D", branch]).current_dir(repo_root);
    run_checked(cmd)
}

/// Name prefixes that signal "do not garbage-collect this" (issue #5737): a
/// human naming a branch `backup/...` or `preserve-...` is communicating
/// retention intent that no reachability or PR-status check can see. Under
/// `--safe` (without `--force`) a branch carrying one of these prefixes is
/// kept regardless of what [`classify_stale_branch`] would otherwise decide.
pub const RETAIN_PREFIXES: &[&str] = &["backup/", "preserve-"];

/// The [`RETAIN_PREFIXES`] entry `branch` starts with, if any.
#[must_use]
pub fn retained_prefix(branch: &str) -> Option<&'static str> {
    RETAIN_PREFIXES
        .iter()
        .copied()
        .find(|p| branch.starts_with(p))
}

/// Outcome of applying the `--safe` gate to a local branch with no remote
/// tracking counterpart (issue #5737).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StaleBranchDecision {
    /// Safe to delete: outside `--safe` the tracking branch's absence is (as
    /// before #5737) sufficient on its own; under `--safe`, every commit on
    /// the branch is additionally reachable from some remote ref, or the
    /// branch's own PR is merged.
    Remove,
    /// `--safe` only: no remote ref holds these commits and no PR merged
    /// them — deleting would destroy the only copy of the work.
    KeepUnreachable,
}

/// Decide whether a branch with no remote tracking ref is safe to delete.
///
/// "No remote tracking branch" and "safe to delete" are not the same fact: a
/// branch that was **never pushed** has no `origin/<branch>` either, and
/// outside this gate would be deleted identically to one whose PR merged and
/// whose remote was auto-deleted — under a flag documented as "merged-PR-only
/// mode". Pure decision logic, mirroring the shape of
/// [`evaluate_aggressive_candidate`](super::aggressive::evaluate_aggressive_candidate):
/// unit-testable without git/gh. In non-`--safe` mode `reachable`/`pr_merged`
/// are ignored — the mere absence of a tracking branch stays sufficient,
/// matching this function's pre-#5737 behavior.
#[must_use]
pub fn classify_stale_branch(safe: bool, reachable: bool, pr_merged: bool) -> StaleBranchDecision {
    if !safe || reachable || pr_merged {
        StaleBranchDecision::Remove
    } else {
        StaleBranchDecision::KeepUnreachable
    }
}

/// Short SHA for `branch`'s tip, for the "recoverable via `git reflog`"
/// deletion/keep hints (mirrors the worktree half's `HEAD=<sha>` line in
/// [`super::aggressive`]). `None` on any git failure — callers render an
/// empty hint rather than fail the whole pass over a cosmetic lookup.
fn branch_sha(repo_root: &Path, branch: &str) -> Option<String> {
    let out = Command::new("git")
        .args(["rev-parse", branch])
        .current_dir(repo_root)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if sha.is_empty() {
        None
    } else {
        Some(sha[..sha.len().min(12)].to_string())
    }
}

/// Render the `" (HEAD=<sha>, recoverable via `git reflog`)"` suffix used by
/// every branch deletion/keep line below, or `""` if the SHA can't be read.
fn sha_hint(repo_root: &Path, branch: &str) -> String {
    branch_sha(repo_root, branch)
        .map(|s| format!(" (HEAD={s}, recoverable via `git reflog`)"))
        .unwrap_or_default()
}

/// Whether every commit reachable from `branch` is also reachable from at
/// least one remote-tracking ref — i.e. deleting the local branch would not
/// lose the only copy of any commit. Automates the manual check from issue
/// #5737's report: `git rev-list --count --not --remotes <branch>` returning
/// `0` means no content would be lost.
///
/// Fails closed (`false`, "not proven safe") on any git/parse failure: a
/// probe this function cannot answer must never look like an answer of "safe
/// to delete".
///
/// Also the safety criterion the `PrStatus::ClosedNoMerge` arms of
/// [`classify_worktree`] / [`classify_pr_worktree`] require before ever
/// removing a closed-without-merge worktree's directory (issue #6418) — `pub`
/// for that reason, so [`crate::worktree_reaper`]'s production probe wiring
/// can call it directly.
#[must_use]
pub fn branch_reachable_from_remotes(repo_root: &Path, branch: &str) -> bool {
    // NOTE: `--not --remotes` must come AFTER `branch`, not before — git
    // parses a bare `--remotes` (no `=`) as taking the *next* token as its
    // glob pattern when one is given positionally, silently swallowing
    // `branch` itself and leaving no positive revision at all (which
    // degenerately reports "0 commits excluded", i.e. always reachable).
    let out = Command::new("git")
        .args(["rev-list", "--count", branch, "--not", "--remotes"])
        .current_dir(repo_root)
        .output();
    let Ok(out) = out else { return false };
    if !out.status.success() {
        return false;
    }
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<u64>()
        .is_ok_and(|n| n == 0)
}

/// Whether `branch`'s PR is merged (including squash-merged, whose original
/// commits are never reachable from a remote ref). REST first (the daemon's
/// separate, less-contended pool — see [`check_pr_merged_rest`]'s docs),
/// falling back to the GraphQL-backed [`check_pr_status_for_branch`] only
/// when REST cannot answer. Mirrors
/// [`super::aggressive`]'s `pr_is_merged` for an arbitrary branch name rather
/// than an issue-numbered one.
fn branch_pr_merged(repo_root: &Path, branch: &str) -> bool {
    let status = match repo_owner_rest(repo_root)
        .map(|owner| check_pr_status_for_branch_rest(repo_root, &owner, branch))
    {
        Some(PrStatus::Unknown) | None => check_pr_status_for_branch(repo_root, branch),
        Some(status) => status,
    };
    matches!(status, PrStatus::Merged { .. })
}

/// Delete (or, under `--dry-run`, report) one branch confirmed safe to
/// remove, tallying the outcome.
fn delete_stale_branch(repo_root: &Path, stats: &mut CleanupStats, dry_run: bool, branch: &str) {
    if dry_run {
        stats.cleaned_branches += 1;
        return;
    }
    match force_delete_branch(repo_root, branch) {
        Ok(()) => stats.cleaned_branches += 1,
        Err(cause) => stats.record_error(&format!("branch {branch}"), "git branch -D", &cause),
    }
}

/// Apply the `--safe` reachability/retain-prefix gates (or, outside
/// `--safe`, the unconditional pre-#5737 behavior) to one branch with no
/// remote tracking counterpart, and act on the outcome. Issue #5737.
fn handle_stale_branch(
    repo_root: &Path,
    stats: &mut CleanupStats,
    opts: &CleanOptions,
    branch: &str,
) {
    let hint = sha_hint(repo_root, branch);

    if !opts.safe {
        println!("  Stale (no origin/{branch}) - deleting {branch}{hint}");
        delete_stale_branch(repo_root, stats, opts.dry_run, branch);
        return;
    }

    if !opts.force {
        if let Some(prefix) = retained_prefix(branch) {
            println!(
                "  Retained ({prefix}* prefix under --safe) - keeping {branch}{hint}; \
                 pass --force to override"
            );
            stats.kept_branches += 1;
            return;
        }
    }

    let reachable = branch_reachable_from_remotes(repo_root, branch);
    let pr_merged = !reachable && branch_pr_merged(repo_root, branch);
    match classify_stale_branch(opts.safe, reachable, pr_merged) {
        StaleBranchDecision::Remove => {
            let why = if pr_merged {
                "PR merged"
            } else {
                "reachable from another remote ref"
            };
            println!("  Stale (no origin/{branch}), {why} - deleting {branch}{hint}");
            delete_stale_branch(repo_root, stats, opts.dry_run, branch);
        }
        StaleBranchDecision::KeepUnreachable => {
            println!(
                "  No remote tracking branch and commits unreachable from any remote{hint} - \
                 keeping {branch} (would lose work under --safe)"
            );
            stats.kept_branches += 1;
        }
    }
}

/// Two-pass local-branch cleanup. Mirrors `clean.py::clean_branches`.
///
/// Under `--safe` ("merged-PR-only mode", issue #5737) the first pass's bare
/// "no remote tracking branch" observation is no longer sufficient on its
/// own to authorize deletion — see [`classify_stale_branch`] and
/// [`handle_stale_branch`].
pub fn clean_branches(repo_root: &Path, stats: &mut CleanupStats, opts: &CleanOptions) {
    let out = Command::new("git")
        .args(["branch", "--format=%(refname:short)"])
        .current_dir(repo_root)
        .output();
    let branches: Vec<String> = match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .lines()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
        _ => Vec::new(),
    };
    if branches.is_empty() {
        println!("No local branches found");
        return;
    }

    let mut protected: std::collections::HashSet<String> =
        std::collections::HashSet::from(["main".to_string()]);
    if let Some(d) = default_branch(repo_root) {
        protected.insert(d);
    }
    if let Some(c) = current_branch(repo_root) {
        protected.insert(c);
    }
    protected.extend(checked_out_branches(repo_root));

    let mut issue_pass_candidates: Vec<String> = Vec::new();
    for branch in &branches {
        if protected.contains(branch) {
            continue;
        }
        if !remote_branch_exists(repo_root, branch) {
            handle_stale_branch(repo_root, stats, opts, branch);
        } else {
            issue_pass_candidates.push(branch.clone());
        }
    }

    for branch in &issue_pass_candidates {
        let Some(rest) = branch.strip_prefix(BRANCH_PREFIX) else {
            continue;
        };
        let Ok(issue_num) = rest.parse::<u32>() else {
            continue;
        };

        let status = gh::issue_state(repo_root, issue_num);
        match status.as_str() {
            "CLOSED" => {
                let hint = sha_hint(repo_root, branch);
                println!("  Issue #{issue_num} CLOSED - deleting {branch}{hint}");
                if !opts.dry_run {
                    match force_delete_branch(repo_root, branch) {
                        Ok(()) => stats.cleaned_branches += 1,
                        Err(cause) => stats.record_error(
                            &format!("branch {branch} (issue #{issue_num} CLOSED)"),
                            "git branch -D",
                            &cause,
                        ),
                    }
                } else {
                    stats.cleaned_branches += 1;
                }
            }
            "OPEN" => {
                println!("  Issue #{issue_num} OPEN - keeping {branch}");
                stats.kept_branches += 1;
            }
            _ => {
                eprintln!("  Could not probe issue #{issue_num} for {branch}: gh lookup returned {status}");
                stats.errored_branches += 1;
            }
        }
    }
}

fn list_loom_tmux_sessions() -> Vec<String> {
    let Ok(out) = Command::new("tmux")
        .args(["-L", "loom", "list-sessions", "-F", "#{session_name}"])
        .output()
    else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Whether `session` (on Loom's isolated `-L loom` socket) has an attached
/// client right now. A Manual-Orchestration-Mode terminal an operator is
/// actively looking at is exactly this — `tmux list-clients` returns one line
/// per attached client, and an empty (but successful) result means none.
///
/// Fails safe: any error running `tmux` (session gone, tmux missing, etc.)
/// reads as "not attached" — the caller has already confirmed the session is
/// live via `list-sessions`, so a probe failure here is not the thing that
/// should block cleanup of an orphaned session.
#[must_use]
fn session_has_attached_client(session: &str) -> bool {
    let Ok(out) = Command::new("tmux")
        .args(["-L", "loom", "list-clients", "-t", session])
        .output()
    else {
        return false;
    };
    out.status.success() && !String::from_utf8_lossy(&out.stdout).trim().is_empty()
}

/// The outcome of applying every tmux-session-removal safety gate to one
/// session. Extracted so the decision is unit-testable without a real tmux
/// server (issue #4890).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TmuxDecision {
    /// No gate applies — kill the session.
    Kill,
    /// `--safe` mode does not touch tmux sessions at all: a tmux session is
    /// not an artifact of a merged PR, so "merged-PR-only mode" has nothing
    /// to say about it, and killing it anyway breaks `--safe`'s promise.
    SkipSafeMode,
    /// The session has an attached client (a live operator terminal) and no
    /// explicit opt-in (`--force`) was given.
    SkipAttached,
}

/// Apply the tmux-removal safety gates, in order. Pure decision logic —
/// mirrors the shape of [`classify_worktree`].
#[must_use]
pub fn classify_tmux_session(safe: bool, attached: bool, force: bool) -> TmuxDecision {
    if safe && !force {
        return TmuxDecision::SkipSafeMode;
    }
    if attached && !force {
        return TmuxDecision::SkipAttached;
    }
    TmuxDecision::Kill
}

/// Tmux session cleanup. `--safe` mode (unless paired with the explicit
/// `--force` opt-in) skips tmux entirely — see [`TmuxDecision::SkipSafeMode`].
/// Outside `--safe`, a session with an attached client is preserved unless
/// `--force` is passed.
pub fn clean_tmux_sessions(stats: &mut CleanupStats, opts: &CleanOptions) {
    let sessions = list_loom_tmux_sessions();
    if sessions.is_empty() {
        println!("No Loom tmux sessions found");
        return;
    }
    println!("Found Loom tmux sessions:");
    for s in &sessions {
        println!("  - {s}");
    }
    println!();

    if opts.safe && !opts.force {
        println!(
            "--safe mode: tmux sessions are not tied to a merged PR, so `--safe` does not \
             touch them (a live Manual-Orchestration-Mode terminal has no PR association at \
             all) - preserving all {} session(s). Use plain `clean --tmux-only` (optionally \
             with `--force`) to clean tmux sessions.",
            sessions.len()
        );
        stats.skipped_tmux += sessions.len();
        return;
    }

    for s in &sessions {
        let attached = session_has_attached_client(s);
        match classify_tmux_session(opts.safe, attached, opts.force) {
            TmuxDecision::SkipSafeMode => {
                // Unreachable here (handled by the early return above), kept
                // so the match stays exhaustive if that guard ever moves.
                stats.skipped_tmux += 1;
            }
            TmuxDecision::SkipAttached => {
                println!("  {s}: has an attached client - preserving (use --force to override)");
                stats.skipped_tmux += 1;
            }
            TmuxDecision::Kill => {
                if opts.dry_run {
                    println!("Would kill: {s}");
                    stats.killed_tmux += 1;
                    continue;
                }
                let ok = Command::new("tmux")
                    .args(["-L", "loom", "kill-session", "-t", s])
                    .status()
                    .is_ok_and(|st| st.success());
                if ok {
                    println!("Killed: {s}");
                    stats.killed_tmux += 1;
                }
            }
        }
    }
}

pub fn clean_agent_config(repo_root: &Path, stats: &mut CleanupStats, dry_run: bool) {
    let base_dir = repo_root.join(".loom").join("claude-config");
    if !base_dir.is_dir() {
        println!("No agent config directories found");
        return;
    }
    let Ok(entries) = std::fs::read_dir(&base_dir) else {
        println!("No agent config directories found");
        return;
    };
    let dirs: Vec<_> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .collect();
    if dirs.is_empty() {
        println!("No agent config directories found");
        return;
    }
    if dry_run {
        println!("Would remove {} agent config dir(s) from {}", dirs.len(), base_dir.display());
        stats.cleaned_config_dirs = dirs.len();
        return;
    }
    let mut removed = 0usize;
    for entry in dirs {
        if std::fs::remove_dir_all(entry.path()).is_ok() {
            removed += 1;
        }
    }
    println!("Removed {removed} agent config dir(s)");
    stats.cleaned_config_dirs = removed;
}

/// `<repo_root>/.loom/sweep-checkpoint/` — where `/loom:sweep` keeps its
/// per-issue checkpoints (#3373) and RUN_ID-keyed main-clean baselines (#3768).
fn sweep_checkpoint_dir(repo_root: &Path) -> PathBuf {
    repo_root.join(".loom").join("sweep-checkpoint")
}

/// `<repo_root>/.loom/sweep-run/` — the sweep run registry written by
/// `sweep-run-registry.sh new`.
fn sweep_run_registry_dir(repo_root: &Path) -> PathBuf {
    repo_root.join(".loom").join("sweep-run")
}

/// Runtime dependencies of [`clean_sweep_transients`], injected so the
/// decision logic is unit-testable without a real clock, a live sweep, or
/// `gh` on PATH.
struct SweepTransientEnv<'a> {
    /// Wall clock the age guard measures against.
    now: SystemTime,
    /// Minimum age before a transient is eligible for pruning.
    min_age: Duration,
    /// `kill -0`-equivalent liveness probe for a registered run's PID.
    pid_alive: &'a dyn Fn(u32) -> bool,
    /// Forge issue-state probe: `"OPEN"` / `"CLOSED"` / `"UNKNOWN"`.
    issue_state: &'a dyn Fn(u32) -> String,
}

/// Whether `run_id` still names a live sweep run.
///
/// Fail-safe by construction: a *missing* registry entry is the only path to
/// "not live" other than a positively-dead PID. An entry that exists but whose
/// JSON (or `pid` field) cannot be read is treated as LIVE, so a corrupt
/// registry write never costs a running sweep its baseline.
fn sweep_run_is_live(repo_root: &Path, run_id: &str, pid_alive: &dyn Fn(u32) -> bool) -> bool {
    let entry = sweep_run_registry_dir(repo_root).join(format!("{run_id}.json"));
    let Ok(text) = std::fs::read_to_string(&entry) else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return true;
    };
    let Some(pid) = value
        .get("pid")
        .and_then(serde_json::Value::as_u64)
        .and_then(|p| u32::try_from(p).ok())
    else {
        return true;
    };
    pid_alive(pid)
}

/// Age of `path` relative to `now`. `None` when the mtime is unreadable
/// (caller treats that as "not eligible", never as "old enough to delete").
fn file_age(path: &Path, now: SystemTime) -> Option<Duration> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    Some(now.duration_since(modified).unwrap_or_default())
}

/// Remove one transient file (or report it under `--dry-run`). `Err` carries
/// the underlying `io::Error` so the caller can name it alongside the path.
fn remove_transient(path: &Path, label: &str, dry_run: bool) -> Result<(), String> {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    if dry_run {
        println!("  Would remove {label}: {name}");
        return Ok(());
    }
    match std::fs::remove_file(path) {
        Ok(()) => {
            println!("  Removed {label}: {name}");
            Ok(())
        }
        Err(e) => Err(e.to_string()),
    }
}

/// Bulk prune of `.loom/sweep-checkpoint/` per-run transients (#4450).
///
/// `sweep-run-registry.sh cleanup` deletes a run's own baseline at sweep end,
/// but that hook is best-effort — a SIGKILLed sweep skips it, and per-issue
/// checkpoints for issues that are never re-swept live forever. This is the
/// backstop that keeps the directory bounded. Three categories:
///
/// 1. **RUN_ID-keyed baselines** (`main-clean-baseline-<RUN_ID>.txt`) whose run
///    is not live (no registry entry, or a registered PID that is dead) *and*
///    which are older than `min_age`.
/// 2. **The legacy un-keyed baseline** (`main-clean-baseline.txt`, pre-#3768,
///    in either its `.loom/sweep-checkpoint/` or older `.loom/` location) — no
///    live run can own it, so age does not matter.
/// 3. **Per-issue checkpoints** (`issue-<N>.json`) older than `min_age` whose
///    issue the forge reports CLOSED and which no in-flight sweep is tracking.
///
/// Every category fails safe: unknown issue state, unreadable mtime, an
/// unparseable registry entry, or an in-flight claim all mean *keep*. Files
/// that match neither naming pattern are never touched.
fn clean_sweep_transients_with(
    repo_root: &Path,
    stats: &mut CleanupStats,
    dry_run: bool,
    env: &SweepTransientEnv,
) {
    // Category 2 first: no liveness or age question to answer.
    for legacy in [
        sweep_checkpoint_dir(repo_root).join("main-clean-baseline.txt"),
        repo_root.join(".loom").join("main-clean-baseline.txt"),
    ] {
        if legacy.is_file() {
            match remove_transient(&legacy, "legacy un-keyed baseline", dry_run) {
                Ok(()) => stats.cleaned_sweep_baselines += 1,
                Err(cause) => stats.record_error(
                    &legacy.display().to_string(),
                    "remove legacy un-keyed baseline",
                    &cause,
                ),
            }
        }
    }

    let dir = sweep_checkpoint_dir(repo_root);
    if !dir.is_dir() {
        println!("  No `.loom/sweep-checkpoint/` directory");
        return;
    }
    let Ok(entries) = std::fs::read_dir(&dir) else {
        println!("  Could not read `.loom/sweep-checkpoint/`");
        return;
    };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(std::fs::DirEntry::path);

    // Issues with an in-flight sweep right now (claim locks + spawn-loop
    // state). A daemon-owned sweep's checkpoint must survive even if its
    // issue already reads CLOSED on the forge.
    let live_issues = active_spawn_loop_issues(repo_root);

    for entry in entries {
        if !entry.file_type().is_ok_and(|t| t.is_file()) {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        let path = entry.path();

        // Already handled above (and only still visible under --dry-run).
        if name == "main-clean-baseline.txt" {
            continue;
        }

        if let Some(run_id) = name
            .strip_prefix("main-clean-baseline-")
            .and_then(|rest| rest.strip_suffix(".txt"))
        {
            if sweep_run_is_live(repo_root, run_id, env.pid_alive) {
                println!("  Keeping baseline of live sweep run: {name}");
                stats.kept_sweep_transients += 1;
                continue;
            }
            match file_age(&path, env.now) {
                Some(age) if age >= env.min_age => {
                    match remove_transient(&path, "stale sweep baseline", dry_run) {
                        Ok(()) => stats.cleaned_sweep_baselines += 1,
                        Err(cause) => stats.record_error(
                            &path.display().to_string(),
                            "remove stale sweep baseline",
                            &cause,
                        ),
                    }
                }
                _ => stats.kept_sweep_transients += 1,
            }
            continue;
        }

        if let Some(issue) = name
            .strip_prefix("issue-")
            .and_then(|rest| rest.strip_suffix(".json"))
            .and_then(|n| n.parse::<u32>().ok())
        {
            if live_issues.contains(&issue) {
                println!("  Keeping checkpoint of in-flight sweep: {name}");
                stats.kept_sweep_transients += 1;
                continue;
            }
            // The age gate also bounds how many forge probes one pass issues.
            match file_age(&path, env.now) {
                Some(age) if age >= env.min_age => {}
                _ => {
                    stats.kept_sweep_transients += 1;
                    continue;
                }
            }
            let state = (env.issue_state)(issue);
            if state == "CLOSED" {
                match remove_transient(&path, "closed-issue checkpoint", dry_run) {
                    Ok(()) => stats.cleaned_sweep_checkpoints += 1,
                    Err(cause) => stats.record_error(
                        &path.display().to_string(),
                        "remove closed-issue checkpoint",
                        &cause,
                    ),
                }
            } else {
                println!("  Issue #{issue} is {state} - keeping {name}");
                stats.kept_sweep_transients += 1;
            }
        }
        // Anything else in the directory is not ours to delete.
    }
}

/// Production entry point for [`clean_sweep_transients_with`]: real clock,
/// [`SWEEP_TRANSIENT_MIN_AGE_SECS`], `kill -0` liveness, and the REST
/// issue-state probe (never GraphQL — see [`gh::issue_state_rest`]).
pub fn clean_sweep_transients(repo_root: &Path, stats: &mut CleanupStats, dry_run: bool) {
    let pid_alive = |pid: u32| crate::sweep_registry::is_pid_alive(pid);
    let issue_state = |issue: u32| gh::issue_state_rest(repo_root, issue);
    let env = SweepTransientEnv {
        now: SystemTime::now(),
        min_age: Duration::from_secs(SWEEP_TRANSIENT_MIN_AGE_SECS),
        pid_alive: &pid_alive,
        issue_state: &issue_state,
    };
    clean_sweep_transients_with(repo_root, stats, dry_run, &env);
}

fn dir_size_human(path: &Path) -> String {
    fn walk(path: &Path, total: &mut u64) {
        let Ok(entries) = std::fs::read_dir(path) else {
            return;
        };
        for entry in entries.flatten() {
            if let Ok(ft) = entry.file_type() {
                if ft.is_dir() {
                    walk(&entry.path(), total);
                } else if let Ok(meta) = entry.metadata() {
                    *total += meta.len();
                }
            }
        }
    }
    let mut total = 0u64;
    walk(path, &mut total);
    if total >= 1024 * 1024 * 1024 {
        format!("{:.1}G", total as f64 / (1024.0 * 1024.0 * 1024.0))
    } else if total >= 1024 * 1024 {
        format!("{:.1}M", total as f64 / (1024.0 * 1024.0))
    } else if total >= 1024 {
        format!("{:.1}K", total as f64 / 1024.0)
    } else {
        format!("{total}B")
    }
}

/// The primary checkout's regenerable build-artifact directories — exactly what
/// `loom-daemon clean --deep` has always removed from `repo_root` itself.
///
/// Deliberately narrower than [`super::orphan_recovery::BUILD_ARTIFACT_PATTERNS`]
/// (which [`reclaim_worktree_artifacts`] walks inside a throwaway worktree): the
/// primary checkout is a human's working clone, so the automatic pass added in
/// #5919 must never remove more than the `clean --deep --safe` an operator would
/// run by hand. Keeping one list means the manual and scheduled paths can never
/// drift apart about what "deep" means.
pub const PRIMARY_CHECKOUT_ARTIFACTS: &[&str] = &["target", "node_modules"];

/// A build-artifact directory the sweep refused to remove because a live
/// process is executing a binary inside it (issue #6127).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtectedArtifact {
    /// Top-level directory name, e.g. `"target"`.
    pub name: String,
    /// The processes whose executable image lives inside it.
    pub holders: Vec<super::safety::LiveExecutable>,
}

impl ProtectedArtifact {
    /// One line naming what was skipped, who is holding it, and what the
    /// operator can do about it. Rendered identically by the CLI (stdout) and
    /// the scheduled pass (daemon log), so "why did disk not get freed?" has
    /// the same answer in both places.
    #[must_use]
    pub fn reason(&self) -> String {
        let holders = self
            .holders
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "{}/ is backing {} live process(es) [{holders}] — refusing to unlink a running \
             program's binary. Stop the service (or, better, stop launching it from a \
             build-output path) and re-run.",
            self.name,
            self.holders.len()
        )
    }
}

/// What [`sweep_primary_checkout_artifacts`] did to one entry of
/// [`PRIMARY_CHECKOUT_ARTIFACTS`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArtifactOutcome {
    /// The directory was removed (or, under `dry_run`, would have been).
    Reclaimed(ReclaimedArtifact),
    /// The directory exists but could not be removed.
    Failed(String),
    /// No such directory — nothing to do.
    Absent(String),
    /// The directory exists and was deliberately **kept**: a live process is
    /// executing a binary inside it (issue #6127).
    Protected(ProtectedArtifact),
}

/// Remove [`PRIMARY_CHECKOUT_ARTIFACTS`] from `repo_root` **itself** (not from a
/// worktree — that is [`reclaim_worktree_artifacts`]), reporting each entry's
/// outcome instead of printing it.
///
/// This is the shared engine behind both deep-clean paths: the interactive
/// [`clean_build_artifacts`] (which renders these outcomes to stdout) and the
/// daemon's scheduled pressure-triggered pass ([`crate::deep_clean`], #5919),
/// which needs the structured result to log *what* it reclaimed and to publish
/// it on `loom-daemon status`. One implementation, so an automatic pass can
/// never quietly delete something the manual command would not.
///
/// # The one gate it does apply (issue #6127)
///
/// A directory currently backing a **running program** is never removed — it is
/// reported as [`ArtifactOutcome::Protected`] instead. Callers still decide
/// *whether* to sweep at all (the CLI: the operator typed `--deep`; the daemon:
/// disk pressure plus the build-slot exclusion); this gate lives in the engine
/// on purpose, because it is the only place both paths pass through. Before
/// this, the CLI path had **no** protection whatsoever, and the scheduled path
/// had only [`crate::deep_clean::exe_is_inside_artifacts`] — a comparison
/// against `current_exe()` that protects loom-daemon's own binary and nothing
/// else. A 4-hourly `clean --deep --safe -y` therefore unlinked a live
/// `safehoused` on a fleet host, which kept running from the deleted inode for
/// three days and would have failed on its next start.
///
/// It is an **ungated floor**, with no `--force` override and no config toggle:
/// the whole failure mode is that the damage is invisible until restart, so an
/// escape hatch a scheduled job could set would reintroduce exactly the bug.
/// The reclaim is not lost, only deferred until the program is stopped.
///
/// Whole-directory granularity is deliberate too — a partial sweep that deleted
/// everything under `target/` *except* the live binary would leave a build tree
/// in a state neither cargo nor the operator can reason about, to save disk in
/// a situation the operator should be fixing instead.
#[must_use]
pub fn sweep_primary_checkout_artifacts(repo_root: &Path, dry_run: bool) -> Vec<ArtifactOutcome> {
    let mut outcomes = Vec::with_capacity(PRIMARY_CHECKOUT_ARTIFACTS.len());
    for name in PRIMARY_CHECKOUT_ARTIFACTS {
        let dir = repo_root.join(name);
        if !dir.is_dir() {
            outcomes.push(ArtifactOutcome::Absent((*name).to_string()));
            continue;
        }
        // Checked under `dry_run` too: a preview that claims it "would remove"
        // a live service's binary is a preview an operator would act on.
        let holders = super::safety::find_processes_executing_within(&dir);
        if !holders.is_empty() {
            outcomes.push(ArtifactOutcome::Protected(ProtectedArtifact {
                name: (*name).to_string(),
                holders,
            }));
            continue;
        }
        let size_human = dir_size_human(&dir);
        if dry_run || std::fs::remove_dir_all(&dir).is_ok() {
            outcomes.push(ArtifactOutcome::Reclaimed(ReclaimedArtifact {
                name: (*name).to_string(),
                size_human,
            }));
        } else {
            outcomes.push(ArtifactOutcome::Failed((*name).to_string()));
        }
    }
    outcomes
}

pub fn clean_build_artifacts(repo_root: &Path, dry_run: bool) {
    for outcome in sweep_primary_checkout_artifacts(repo_root, dry_run) {
        match outcome {
            ArtifactOutcome::Reclaimed(a) if dry_run => {
                println!("Would remove {}/ ({})", a.name, a.size_human);
            }
            ArtifactOutcome::Reclaimed(a) => {
                println!("Removed {}/ ({})", a.name, a.size_human);
            }
            ArtifactOutcome::Failed(name) => eprintln!("Failed to remove {name}/"),
            ArtifactOutcome::Absent(name) => println!("No {name}/ directory found"),
            // stdout, like the other outcomes: this is the line that explains
            // an unexpectedly small reclaim, and it belongs in the same log the
            // scheduled `--deep --safe -y` job already captures.
            ArtifactOutcome::Protected(p) => println!("SKIPPED {}", p.reason()),
        }
        println!();
    }
}

/// One build-artifact directory [`reclaim_worktree_artifacts`] removed (or
/// would remove, under `dry_run`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReclaimedArtifact {
    /// Top-level directory name relative to the worktree root, e.g. `"target"`.
    pub name: String,
    /// Human-readable size, matching [`clean_build_artifacts`]'s report format.
    pub size_human: String,
}

/// Reclaim regenerable build-artifact directories (`target/`, `node_modules`,
/// `.venv`, `coverage/`, ...) from **inside one worktree** without removing
/// the worktree itself — the AC3 follow-up carved out of #5177 into #5187.
///
/// Unlike [`clean_build_artifacts`] (which only ever reclaims from
/// `repo_root` itself, wired to `--deep`), this walks
/// [`super::orphan_recovery::BUILD_ARTIFACT_PATTERNS`] against
/// `worktree_path`'s **top-level** entries, trims a trailing `/`, and removes
/// only entries that are directories — a same-named file (`Cargo.lock`,
/// `pnpm-lock.yaml`, `.loom-checkpoint`, `.loom-in-use`) is never touched, so
/// reusing the dirty-detection pattern list here is safe even though most of
/// its entries are files rather than reclaimable directories.
///
/// Pure I/O, no eligibility gate of its own — callers (the worktree reaper's
/// artifact-reclaim pass) decide *whether* a worktree is eligible via
/// [`classify_worktree`] / [`WorktreeDecision`] before calling this.
#[must_use]
pub fn reclaim_worktree_artifacts(worktree_path: &Path, dry_run: bool) -> Vec<ReclaimedArtifact> {
    let mut reclaimed = Vec::new();
    for pattern in super::orphan_recovery::BUILD_ARTIFACT_PATTERNS {
        let name = pattern.trim_end_matches('/');
        let dir = worktree_path.join(name);
        if !dir.is_dir() {
            continue;
        }
        let size_human = dir_size_human(&dir);
        if dry_run {
            reclaimed.push(ReclaimedArtifact {
                name: name.to_string(),
                size_human,
            });
            continue;
        }
        if std::fs::remove_dir_all(&dir).is_ok() {
            reclaimed.push(ReclaimedArtifact {
                name: name.to_string(),
                size_human,
            });
        }
    }
    reclaimed
}

fn spawn_loop_locks_dir(repo_root: &Path) -> std::path::PathBuf {
    super::liveness::locks_dir(repo_root)
}

/// Remove `.loom/locks/issue-<N>/` dirs not backed by a live spawn-loop task.
/// Mirrors `clean.py::_clear_stale_spawn_loop_locks`.
pub fn clear_stale_spawn_loop_locks(repo_root: &Path, dry_run: bool) -> usize {
    let locks_dir = spawn_loop_locks_dir(repo_root);
    if !locks_dir.is_dir() {
        println!("  No `.loom/locks/` directory");
        return 0;
    }
    let state = super::spawn_loop_state::read_spawn_loop_state(repo_root);
    let live_issues: std::collections::HashSet<u32> = state
        .running
        .iter()
        .filter(|t| t.issue != 0)
        .map(|t| t.issue)
        .collect();

    let mut removed = 0usize;
    let mut found_any = false;
    let Ok(entries) = std::fs::read_dir(&locks_dir) else {
        return 0;
    };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(std::fs::DirEntry::path);
    for entry in entries {
        if !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(rest) = name.strip_prefix("issue-") else {
            continue;
        };
        found_any = true;
        let Ok(issue_num) = rest.parse::<u32>() else {
            eprintln!("  Skipping malformed lock dir: {name}");
            continue;
        };
        if live_issues.contains(&issue_num) {
            println!("  Keeping lock for live task: {name}");
            continue;
        }
        if dry_run {
            println!("  Would remove stale lock: {name}");
            removed += 1;
        } else if std::fs::remove_dir_all(entry.path()).is_ok() {
            println!("  Removed stale lock: {name}");
            removed += 1;
        } else {
            eprintln!("  Failed to remove {name}");
        }
    }
    if !found_any {
        println!("  No spawn-loop locks to inspect");
    }
    removed
}

fn revert_stale_building_labels_spawn_loop(repo_root: &Path, dry_run: bool) -> usize {
    let state_present = super::spawn_loop_state::read_spawn_loop_state(repo_root).present;
    let locked_issues = super::liveness::active_locked_issues(repo_root);
    if !state_present && locked_issues.is_empty() {
        println!(
            "  No authoritative liveness source (no spawn-loop-state.json, no \
             .loom/locks/issue-<N>/ locks) — skipping loom:building revert \
             (fail-safe: absent liveness data means treat claims as ALIVE, \
             not orphaned). See issue #3651."
        );
        return 0;
    }

    let active = active_spawn_loop_issues(repo_root);
    let building = match gh::list_building_issues(repo_root) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("  Could not list building issues: {e}");
            return 0;
        }
    };

    let orphans: Vec<u32> = building
        .iter()
        .map(|b| b.number)
        .filter(|n| !active.contains(n))
        .collect();
    if orphans.is_empty() {
        println!("  No orphaned `loom:building` labels found");
        return 0;
    }

    let mut reverted = 0usize;
    for issue_num in orphans {
        if dry_run {
            println!("  Would revert label on #{issue_num}: building -> issue");
            continue;
        }
        match gh::edit_labels(repo_root, issue_num, "loom:building", "loom:issue") {
            Ok(()) => {
                println!("  Reverted #{issue_num}: building -> issue");
                reverted += 1;
            }
            Err(e) => eprintln!("  Failed to revert #{issue_num}: {e}"),
        }
    }
    reverted
}

/// `--daemon` crash recovery. Mirrors `clean.py::clean_daemon_crash_state`.
pub fn clean_daemon_crash_state(repo_root: &Path, dry_run: bool) {
    println!("Step 1: Kill orphaned tmux sessions");
    let mut stats = CleanupStats::default();
    // Neither `--safe` nor `--force`: crash recovery is unattended, so a
    // session with an attached client (not actually orphaned — someone is
    // looking at it) is still preserved (issue #4890).
    let tmux_opts = CleanOptions {
        dry_run,
        ..CleanOptions::default()
    };
    clean_tmux_sessions(&mut stats, &tmux_opts);
    println!();

    println!("Step 2: Revert stale `loom:building` labels");
    revert_stale_building_labels_spawn_loop(repo_root, dry_run);
    println!();

    println!("Step 3: Clear stale spawn-loop claim locks");
    clear_stale_spawn_loop_locks(repo_root, dry_run);
    println!();

    println!("Step 4: Reset issue-failures.json");
    let failures_file = repo_root.join(".loom").join("issue-failures.json");
    if failures_file.exists() {
        if dry_run {
            println!("Would reset: issue-failures.json");
        } else {
            let _ = std::fs::write(&failures_file, "{\n  \"entries\": {}\n}\n");
            println!("Reset issue-failures.json");
        }
    } else {
        println!("No issue-failures.json to reset");
    }
    println!();
}

pub fn print_summary(stats: &CleanupStats, dry_run: bool, safe_mode: bool) {
    println!();
    println!("========================================");
    println!("  Summary");
    println!("========================================");
    println!();
    if dry_run {
        println!("  Would clean: {} worktree(s)", stats.cleaned_worktrees);
    } else {
        println!("  Cleaned: {} worktree(s)", stats.cleaned_worktrees);
    }
    if stats.stale_worktree_registrations > 0 {
        println!(
            "  Stale registration(s) already gone (no directory on disk): {}",
            stats.stale_worktree_registrations
        );
    }
    if stats.skipped_in_use > 0 {
        println!("  Skipped (in use by shepherd): {}", stats.skipped_in_use);
    }
    if stats.skipped_editable > 0 {
        println!("  Skipped (editable pip install): {}", stats.skipped_editable);
    }
    if safe_mode {
        println!("  Skipped (open/not merged): {}", stats.skipped_open + stats.skipped_not_merged);
        println!("  Skipped (grace period): {}", stats.skipped_grace);
        println!("  Skipped (uncommitted): {}", stats.skipped_uncommitted);
    }
    if stats.cleaned_branches > 0 || stats.kept_branches > 0 || stats.errored_branches > 0 {
        if dry_run {
            println!("  Would delete: {} branch(es)", stats.cleaned_branches);
        } else {
            println!("  Deleted: {} branch(es)", stats.cleaned_branches);
        }
        println!("  Kept: {} branch(es)", stats.kept_branches);
        if stats.errored_branches > 0 {
            println!("  Errored (gh probe failed): {} branch(es)", stats.errored_branches);
        }
    }
    if stats.killed_tmux > 0 {
        if dry_run {
            println!("  Would kill: {} tmux session(s)", stats.killed_tmux);
        } else {
            println!("  Killed: {} tmux session(s)", stats.killed_tmux);
        }
    }
    if stats.skipped_tmux > 0 {
        println!(
            "  Skipped (attached client or --safe mode): {} tmux session(s)",
            stats.skipped_tmux
        );
    }
    if stats.cleaned_config_dirs > 0 {
        if dry_run {
            println!("  Would remove: {} agent config dir(s)", stats.cleaned_config_dirs);
        } else {
            println!("  Removed: {} agent config dir(s)", stats.cleaned_config_dirs);
        }
    }
    if stats.cleaned_sweep_baselines > 0 || stats.cleaned_sweep_checkpoints > 0 {
        if dry_run {
            println!(
                "  Would remove: {} sweep baseline(s), {} closed-issue checkpoint(s)",
                stats.cleaned_sweep_baselines, stats.cleaned_sweep_checkpoints
            );
        } else {
            println!(
                "  Removed: {} sweep baseline(s), {} closed-issue checkpoint(s)",
                stats.cleaned_sweep_baselines, stats.cleaned_sweep_checkpoints
            );
        }
    }
    if stats.kept_sweep_transients > 0 {
        println!("  Kept: {} sweep transient(s)", stats.kept_sweep_transients);
    }
    if stats.errors > 0 {
        println!("  Errors: {}", stats.errors);
        for detail in &stats.error_details {
            println!("    - {detail}");
        }
    }
    println!();
}

/// Run the standard (non-`--aggressive`, non-`--daemon`) clean pass. Returns
/// the process exit code (1 if any errors were recorded, else 0) — mirrors
/// `clean.py::main`'s non-interactive branches (this native port always runs
/// non-interactively: an unattended CLI has no stdin to prompt against, so
/// the "no flag given" case behaves like a safe no-op skip rather than
/// blocking on a prompt — see `clean_worktrees`'s final `else` branch).
pub fn run_clean(repo_root: &Path, opts: &CleanOptions) -> i32 {
    let all_targets = !opts.worktrees_only && !opts.branches_only && !opts.tmux_only;
    let mut stats = CleanupStats::default();

    println!();
    println!("========================================");
    if opts.deep {
        println!("  Loom Deep Cleanup");
    } else if opts.safe {
        println!("  Loom Safe Cleanup");
    } else {
        println!("  Loom Cleanup");
    }
    if opts.dry_run {
        println!("  (DRY RUN MODE)");
    }
    println!("========================================");
    println!();

    let confirmed = confirm_destructive_action(opts.dry_run, opts.force);
    if !confirmed {
        println!("Cleanup cancelled");
        return 0;
    }
    println!();

    if !opts.branches_only && !opts.tmux_only {
        println!("Cleaning Worktrees\n");
        clean_worktrees(repo_root, &mut stats, opts);
        prune_orphaned_worktrees(repo_root, opts.dry_run);
        println!();
        println!("Cleaning Stale Spawn-Loop Locks\n");
        clear_stale_spawn_loop_locks(repo_root, opts.dry_run);
        println!();
    }

    if !opts.worktrees_only && !opts.tmux_only {
        println!("Cleaning Merged Branches\n");
        clean_branches(repo_root, &mut stats, opts);
        println!();
    }

    if !opts.worktrees_only && !opts.branches_only {
        println!("Cleaning Loom Tmux Sessions\n");
        clean_tmux_sessions(&mut stats, opts);
        println!();
    }

    if all_targets {
        println!("Cleaning Agent Config Directories\n");
        clean_agent_config(repo_root, &mut stats, opts.dry_run);
        println!();

        println!("Cleaning Sweep Checkpoint Transients\n");
        clean_sweep_transients(repo_root, &mut stats, opts.dry_run);
        println!();
    }

    if opts.deep {
        println!("Deep Cleaning Build Artifacts\n");
        clean_build_artifacts(repo_root, opts.dry_run);
        println!();
    }

    print_summary(&stats, opts.dry_run, opts.safe);

    println!("{}", completion_line("Cleanup", opts.dry_run, stats.errors));

    exit_code(stats.errors)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    // --- select_pr_status / rows_to_status / classify_pr_row (#6746) ------
    //
    // A branch can have more than one PR opened against it over its
    // lifetime; both `gh pr list --head` and the REST `pulls?head=` list
    // endpoint default to newest-created-first, so a naive "take the first
    // row" reads whichever PR was opened *last*, not whichever one is
    // actually relevant (e.g. a merged PR, then a later unrelated PR from
    // the same branch name that closed without merging — observed live for
    // `feature/issue-5179` during #6653's curation).

    fn pr_row(state: &str, merged_at: Option<&str>, closed_at: Option<&str>) -> PrRow {
        // `PrRow`'s fields are private but it derives `Deserialize`, so tests
        // construct rows the same way the production code parses them: from
        // the exact `gh pr list --json number,state,mergedAt,closedAt` shape.
        let json = serde_json::json!({
            "number": 1,
            "state": state,
            "mergedAt": merged_at,
            "closedAt": closed_at,
        });
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn rows_to_status_prefers_merged_over_a_later_closed_no_merge_row() {
        // Exact shape of the live #6746 repro: the newer row (closed, no
        // merge) sorts first; the older row (merged) sorts second. The
        // result must still be `Merged`.
        let rows = vec![
            pr_row("CLOSED", None, Some("2026-08-04T02:43:56Z")),
            pr_row(
                "CLOSED", // GitHub reports a merged PR's `state` as CLOSED too
                Some("2026-08-04T02:33:48Z"),
                None,
            ),
        ];
        let status = rows_to_status(Some(rows));
        assert_eq!(
            status,
            PrStatus::Merged {
                merged_at: "2026-08-04T02:33:48Z".to_string()
            }
        );
    }

    #[test]
    fn rows_to_status_prefers_merged_regardless_of_row_order() {
        // Same rows, merged-first this time — order must not matter.
        let rows = vec![
            pr_row("CLOSED", Some("2026-08-04T02:33:48Z"), None),
            pr_row("CLOSED", None, Some("2026-08-04T02:43:56Z")),
        ];
        let status = rows_to_status(Some(rows));
        assert_eq!(
            status,
            PrStatus::Merged {
                merged_at: "2026-08-04T02:33:48Z".to_string()
            }
        );
    }

    #[test]
    fn rows_to_status_prefers_open_over_closed_no_merge_when_no_merge_present() {
        // No `Merged` row anywhere: `Open` (an actively-reviewed PR) beats an
        // older `ClosedNoMerge` row. The "Merged always wins" rule alone does
        // not disambiguate this case, so the order is picked and documented
        // explicitly (test plan item 3 in the issue's curation notes).
        let rows = vec![
            pr_row("CLOSED", None, Some("2026-08-01T00:00:00Z")),
            pr_row("OPEN", None, None),
        ];
        assert_eq!(rows_to_status(Some(rows)), PrStatus::Open);
    }

    #[test]
    fn rows_to_status_falls_back_to_first_row_when_all_unmerged_and_closed() {
        // No `Merged`, no `Open`: the first (i.e. most-recently-created,
        // given the forge's default ordering) `ClosedNoMerge` row wins.
        let rows = vec![
            pr_row("CLOSED", None, Some("2026-08-04T02:43:56Z")),
            pr_row("CLOSED", None, Some("2026-08-01T00:00:00Z")),
        ];
        assert_eq!(
            rows_to_status(Some(rows)),
            PrStatus::ClosedNoMerge {
                closed_at: Some("2026-08-04T02:43:56Z".to_string())
            }
        );
    }

    #[test]
    fn rows_to_status_empty_rows_is_no_pr() {
        assert_eq!(rows_to_status(Some(Vec::new())), PrStatus::NoPr);
    }

    #[test]
    fn rows_to_status_none_is_unknown() {
        assert_eq!(rows_to_status(None), PrStatus::Unknown);
    }

    #[test]
    fn select_pr_status_prefers_merged_across_three_rows() {
        let statuses = vec![
            PrStatus::ClosedNoMerge {
                closed_at: Some("2026-08-04T02:43:56Z".to_string()),
            },
            PrStatus::Open,
            PrStatus::Merged {
                merged_at: "2026-08-04T02:33:48Z".to_string(),
            },
        ];
        assert_eq!(
            select_pr_status(statuses),
            PrStatus::Merged {
                merged_at: "2026-08-04T02:33:48Z".to_string()
            }
        );
    }

    #[test]
    fn classify_pr_row_multi_row_rest_path_prefers_merged() {
        // REST counterpart of `rows_to_status_prefers_merged_over_a_later_closed_no_merge_row`
        // (issue #6746's third acceptance criterion) — same preference logic,
        // driven through `classify_pr_row` the way `check_pr_status_for_branch_rest`
        // now feeds `select_pr_status`.
        let statuses = vec![
            classify_pr_row("closed", None, Some("2026-08-04T02:43:56Z")),
            classify_pr_row("closed", Some("2026-08-04T02:33:48Z"), None),
        ];
        assert_eq!(
            select_pr_status(statuses),
            PrStatus::Merged {
                merged_at: "2026-08-04T02:33:48Z".to_string()
            }
        );
    }

    // --- confirm_destructive_action (#5736) -------------------------------

    #[test]
    fn confirm_destructive_action_dry_run_bypasses_prompt_regardless_of_force() {
        // Neither branch touches stdin, so these are safe to assert directly
        // without a subprocess harness (see `clean_aggressive_confirmation.rs`
        // for the closed-stdin end-to-end case).
        assert!(confirm_destructive_action(true, false));
        assert!(confirm_destructive_action(true, true));
    }

    #[test]
    fn confirm_destructive_action_force_bypasses_prompt() {
        assert!(confirm_destructive_action(false, true));
    }

    #[test]
    fn grace_period_not_passed_reports_remaining() {
        let now = Utc::now();
        let merged = now - chrono::Duration::seconds(100);
        let (passed, remaining) = check_grace_period(merged, 600, now);
        assert!(!passed);
        assert_eq!(remaining, 500);
    }

    #[test]
    fn grace_period_passed_reports_zero_remaining() {
        let now = Utc::now();
        let merged = now - chrono::Duration::seconds(700);
        let (passed, remaining) = check_grace_period(merged, 600, now);
        assert!(passed);
        assert_eq!(remaining, 0);
    }

    #[test]
    fn dir_size_human_handles_missing_dir() {
        // A missing directory contributes 0 bytes, not an error.
        assert_eq!(dir_size_human(Path::new("/does/not/exist/at/all")), "0B");
    }

    // --- reclaim_worktree_artifacts (#5187) ------------------------------

    #[test]
    fn reclaim_removes_target_and_node_modules_but_nothing_else() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("target/debug")).unwrap();
        std::fs::write(tmp.path().join("target/debug/binary"), b"x").unwrap();
        std::fs::create_dir_all(tmp.path().join("node_modules/.bin")).unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::write(tmp.path().join("src/main.rs"), b"fn main() {}").unwrap();
        std::fs::write(tmp.path().join("Cargo.lock"), b"lockfile").unwrap();

        let reclaimed = reclaim_worktree_artifacts(tmp.path(), false);
        let names: Vec<_> = reclaimed.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(
            names.into_iter().collect::<std::collections::HashSet<_>>(),
            std::collections::HashSet::from(["target", "node_modules"])
        );
        assert!(!tmp.path().join("target").exists());
        assert!(!tmp.path().join("node_modules").exists());
        // Everything else — git history stand-ins, source, lockfiles — is untouched.
        assert!(tmp.path().join("src/main.rs").is_file());
        assert!(tmp.path().join("Cargo.lock").is_file());
    }

    #[test]
    fn reclaim_never_removes_a_same_named_file() {
        // `Cargo.lock` and `pnpm-lock.yaml` are both entries in
        // BUILD_ARTIFACT_PATTERNS, but they are files, not directories — the
        // reclaim pass must never `remove_dir_all` a file.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("Cargo.lock"), b"lockfile").unwrap();
        std::fs::write(tmp.path().join("pnpm-lock.yaml"), b"lockfile").unwrap();
        std::fs::write(tmp.path().join(".loom-in-use"), b"{}").unwrap();

        let reclaimed = reclaim_worktree_artifacts(tmp.path(), false);
        assert!(reclaimed.is_empty(), "{reclaimed:?}");
        assert!(tmp.path().join("Cargo.lock").is_file());
        assert!(tmp.path().join("pnpm-lock.yaml").is_file());
        assert!(tmp.path().join(".loom-in-use").is_file());
    }

    #[test]
    fn reclaim_dry_run_reports_without_removing() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("target")).unwrap();

        let reclaimed = reclaim_worktree_artifacts(tmp.path(), true);
        assert_eq!(reclaimed.len(), 1);
        assert_eq!(reclaimed[0].name, "target");
        assert!(tmp.path().join("target").is_dir(), "dry-run must not remove");
    }

    #[test]
    fn reclaim_with_no_artifact_dirs_is_a_clean_no_op() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("README.md"), b"hi").unwrap();
        let reclaimed = reclaim_worktree_artifacts(tmp.path(), false);
        assert!(reclaimed.is_empty());
    }

    // --- sweep_primary_checkout_artifacts (#5919) ------------------------

    #[test]
    fn primary_checkout_sweep_removes_the_checkouts_own_build_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("target/release")).unwrap();
        std::fs::write(tmp.path().join("target/release/loom-daemon"), b"x").unwrap();
        std::fs::create_dir_all(tmp.path().join("node_modules")).unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), b"[package]").unwrap();

        let outcomes = sweep_primary_checkout_artifacts(tmp.path(), false);
        let reclaimed: Vec<_> = outcomes
            .iter()
            .filter_map(|o| match o {
                ArtifactOutcome::Reclaimed(a) => Some(a.name.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(reclaimed, vec!["target", "node_modules"]);
        assert!(!tmp.path().join("target").exists());
        assert!(!tmp.path().join("node_modules").exists());
        // The working clone itself is untouched — this is a developer's repo,
        // not a throwaway worktree.
        assert!(tmp.path().join("src").is_dir());
        assert!(tmp.path().join("Cargo.toml").is_file());
    }

    #[test]
    fn primary_checkout_sweep_reports_absent_dirs_instead_of_failing() {
        let tmp = tempfile::tempdir().unwrap();
        let outcomes = sweep_primary_checkout_artifacts(tmp.path(), false);
        assert_eq!(outcomes.len(), PRIMARY_CHECKOUT_ARTIFACTS.len());
        assert!(outcomes
            .iter()
            .all(|o| matches!(o, ArtifactOutcome::Absent(_))));
    }

    #[test]
    fn primary_checkout_sweep_dry_run_removes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("target")).unwrap();
        let outcomes = sweep_primary_checkout_artifacts(tmp.path(), true);
        assert!(matches!(outcomes[0], ArtifactOutcome::Reclaimed(_)));
        assert!(tmp.path().join("target").is_dir());
    }

    // --- live-binary protection (#6127) ----------------------------------

    /// Copy the host's `sleep` into `<dir>/<name>` and run it, so a live
    /// process's executable image sits inside a directory the sweep would
    /// otherwise delete. Deliberately a *different* process than this test
    /// binary: the pre-existing `deep_clean::exe_is_inside_artifacts` gate only
    /// ever compared `current_exe()`, which is exactly the gap #6127 reports.
    fn spawn_service_in(dir: &Path, name: &str) -> std::process::Child {
        let source = ["/bin/sleep", "/usr/bin/sleep"]
            .iter()
            .map(Path::new)
            .find(|p| p.is_file())
            .expect("a `sleep` binary is needed to stand in for a service");
        std::fs::create_dir_all(dir).unwrap();
        let program = dir.join(name);
        std::fs::copy(source, &program).unwrap();
        // Re-sign the relocated copy on macOS: a plain `fs::copy` of a system binary
        // carries over the original embedded code signature (bound to the source
        // path's identity), so Gatekeeper SIGKILLs the exec'd copy asynchronously —
        // `Command::spawn()` still returns `Ok`, so the "live" process can already
        // be dead by the time this test asserts on it. Same mitigation this repo
        // already applies to its own compiled test binaries via
        // `.cargo/macos-test-runner.sh` (#2298). Test-only; not a production fix.
        // See #6430 / #6343.
        #[cfg(target_os = "macos")]
        {
            std::process::Command::new("codesign")
                .args(["-f", "-s", "-", program.to_str().unwrap()])
                .status()
                .expect("failed to ad-hoc codesign test binary");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        // Retried on ETXTBSY: a concurrent test thread forking while our write
        // fd was open leaves the child holding it, and Linux then refuses to
        // exec. A harness race, not a property of the code under test.
        let mut last_err = None;
        for _ in 0..100 {
            match std::process::Command::new(&program)
                .arg("300")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
            {
                Ok(child) => {
                    std::thread::sleep(std::time::Duration::from_millis(250));
                    return child;
                }
                Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                    last_err = Some(e);
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                Err(e) => panic!("stand-in service must spawn: {e}"),
            }
        }
        panic!("stand-in service never became executable: {last_err:?}");
    }

    #[test]
    fn primary_checkout_sweep_keeps_a_dir_backing_another_processs_binary() {
        let tmp = tempfile::tempdir().unwrap();
        let mut service = spawn_service_in(&tmp.path().join("target/release"), "loom-svc-fixture");
        std::fs::create_dir_all(tmp.path().join("node_modules")).unwrap();

        let outcomes = sweep_primary_checkout_artifacts(tmp.path(), false);

        let target_survived = tmp.path().join("target").is_dir();
        let _ = service.kill();
        let _ = service.wait();

        let protected = outcomes
            .iter()
            .find_map(|o| match o {
                ArtifactOutcome::Protected(p) => Some(p),
                _ => None,
            })
            .expect("target/ must report as Protected, not Reclaimed");
        assert_eq!(protected.name, "target");
        assert!(protected.holders.iter().any(|h| h.pid == service.id()));
        assert!(
            target_survived,
            "the live service's binary must still be on disk after the sweep"
        );
        assert!(
            protected.reason().contains("live process"),
            "the skip must explain itself: {}",
            protected.reason()
        );

        // Protection is per-directory, not all-or-nothing: node_modules/ has
        // nothing running inside it and is still reclaimed.
        assert!(outcomes
            .iter()
            .any(|o| matches!(o, ArtifactOutcome::Reclaimed(a) if a.name == "node_modules")));
        assert!(!tmp.path().join("node_modules").exists());
    }

    #[test]
    fn primary_checkout_sweep_dry_run_does_not_promise_to_remove_a_live_dir() {
        // A preview that says "Would remove target/" while a service is running
        // from it is a preview an operator would act on.
        let tmp = tempfile::tempdir().unwrap();
        let mut service = spawn_service_in(&tmp.path().join("target/release"), "loom-svc-dryrun");

        let outcomes = sweep_primary_checkout_artifacts(tmp.path(), true);

        let _ = service.kill();
        let _ = service.wait();

        assert!(matches!(outcomes[0], ArtifactOutcome::Protected(_)), "{outcomes:?}");
    }

    #[test]
    fn the_scheduled_and_manual_deep_paths_share_one_artifact_list() {
        // The automatic pass added in #5919 must never remove more than a
        // hand-typed `clean --deep --safe` would. One list, asserted.
        assert_eq!(PRIMARY_CHECKOUT_ARTIFACTS, ["target", "node_modules"]);
    }

    // --- untracked-orphan worktree removal (#5177 AC5) -------------------

    #[test]
    fn untracked_worktree_error_recognizes_gits_message() {
        // git's actual message for a path that exists but is not a worktree.
        assert!(is_untracked_worktree_error(
            "fatal: '/repo/.loom/worktrees/issue-42' is not a working tree"
        ));
        assert!(is_untracked_worktree_error("IS NOT A WORKING TREE")); // case-insensitive
                                                                       // Any other failure must NOT be treated as an orphan directory.
        assert!(!is_untracked_worktree_error(
            "fatal: validation failed, cannot remove working tree"
        ));
        assert!(!is_untracked_worktree_error("Permission denied (os error 13)"));
        assert!(!is_untracked_worktree_error(""));
    }

    #[test]
    fn force_remove_orphan_requires_all_three_conditions() {
        let ok = "fatal: 'x' is not a working tree";
        let other = "some other failure";
        // All three present → fall back to direct removal.
        assert!(should_force_remove_orphan_dir(ok, true, true));
        // Missing any single guard → refuse (never a blanket rm -rf).
        assert!(!should_force_remove_orphan_dir(ok, false, true)); // no sentinel
        assert!(!should_force_remove_orphan_dir(ok, true, false)); // outside root
        assert!(!should_force_remove_orphan_dir(other, true, true)); // different error
    }

    /// End-to-end: a directory under the managed worktree root that git no
    /// longer tracks as a worktree (the #5177 "is not a working tree" orphan) is
    /// removed by `cleanup_worktree` instead of erroring out.
    #[test]
    #[serial_test::serial]
    fn cleanup_worktree_removes_untracked_orphan_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_root = tmp.path().canonicalize().unwrap();

        // A real git repo so `git worktree remove` produces the genuine
        // "is not a working tree" error rather than "not a git repository".
        assert!(Command::new("git")
            .args(["init", "-q"])
            .current_dir(&repo_root)
            .status()
            .unwrap()
            .success());

        // An orphan worktree directory under the resolved (override-aware)
        // managed root, carrying the `.loom-managed` sentinel — but with no
        // corresponding `git worktree list` entry.
        let orphan = crate::worktree_root::worktree_root(&repo_root).join("issue-999");
        std::fs::create_dir_all(&orphan).unwrap();
        std::fs::write(orphan.join(".loom-managed"), "test").unwrap();
        std::fs::write(orphan.join("some-build-artifact"), "junk").unwrap();
        assert!(orphan.is_dir());

        let result = cleanup_worktree(&repo_root, &orphan, 999, false, "test");
        assert_eq!(
            result,
            Ok(CleanupOutcome::Removed),
            "orphan removal should succeed and report Removed: {result:?}"
        );
        assert!(!orphan.exists(), "orphan directory should be gone");
    }

    // --- already-gone stale registration (#5895) --------------------------

    /// The second orphan shape from #5895: `git worktree remove` fails with
    /// "is not a working tree" (never registered, or already deregistered)
    /// AND the directory does not exist on disk either — there is nothing
    /// unsafe left to remove, so this must succeed as `AlreadyGone`, not
    /// error out the way it did before this fix.
    #[test]
    #[serial_test::serial]
    fn cleanup_worktree_treats_missing_directory_as_already_gone() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_root = tmp.path().canonicalize().unwrap();
        assert!(Command::new("git")
            .args(["init", "-q"])
            .current_dir(&repo_root)
            .status()
            .unwrap()
            .success());

        let gone = crate::worktree_root::worktree_root(&repo_root).join("issue-4525");
        assert!(!gone.exists(), "fixture must not exist on disk");

        let result = cleanup_worktree(&repo_root, &gone, 4525, false, "test");
        assert_eq!(
            result,
            Ok(CleanupOutcome::AlreadyGone),
            "a stale registration with no directory on disk must not error: {result:?}"
        );
    }

    /// AC1: a worktree registration whose directory has already been deleted
    /// by something other than `git worktree remove` never becomes a
    /// `read_dir` entry, so `cleanup_worktree`'s per-entry backstop alone
    /// can't reach it — `clean_worktrees` must proactively `git worktree
    /// prune` before enumerating so the stale metadata doesn't linger
    /// forever (this is the literal repro from the issue: `.git/worktrees/*`
    /// entries surviving indefinitely once their directory is gone).
    #[test]
    #[serial_test::serial]
    fn clean_worktrees_prunes_a_registration_whose_directory_is_already_gone() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_root = tmp.path().canonicalize().unwrap();
        assert!(Command::new("git")
            .args(["init", "-q"])
            .current_dir(&repo_root)
            .status()
            .unwrap()
            .success());
        // CI runners have no global git identity — set one explicitly or
        // `git commit` refuses with "Please tell me who you are."
        git(&repo_root, &["config", "user.email", "loom@example.com"]);
        git(&repo_root, &["config", "user.name", "Loom Test"]);
        assert!(Command::new("git")
            .args(["commit", "--allow-empty", "-q", "-m", "init"])
            .current_dir(&repo_root)
            .status()
            .unwrap()
            .success());

        let worktrees_dir = crate::worktree_root::worktree_root(&repo_root);
        std::fs::create_dir_all(&worktrees_dir).unwrap();
        let wt_path = worktrees_dir.join("issue-777");
        assert!(Command::new("git")
            .args(["worktree", "add", "-q"])
            .arg(&wt_path)
            .args(["-b", "wt-777-branch"])
            .current_dir(&repo_root)
            .status()
            .unwrap()
            .success());

        // Removed by something other than `git worktree remove` (a manual
        // `rm -rf`, a reaper that deleted the directory without
        // deregistering, an interrupted sweep — see the issue body). The
        // registration is left behind and, because the directory is gone,
        // invisible to `clean_worktrees`'s `read_dir`-based enumeration.
        std::fs::remove_dir_all(&wt_path).unwrap();

        let list_before = String::from_utf8_lossy(
            &Command::new("git")
                .args(["worktree", "list", "--porcelain"])
                .current_dir(&repo_root)
                .output()
                .unwrap()
                .stdout,
        )
        .to_string();
        assert!(
            list_before.contains("issue-777"),
            "fixture must still be registered before the run: {list_before}"
        );

        let mut stats = CleanupStats::default();
        let opts = CleanOptions::default();
        clean_worktrees(&repo_root, &mut stats, &opts);

        let list_after = String::from_utf8_lossy(
            &Command::new("git")
                .args(["worktree", "list", "--porcelain"])
                .current_dir(&repo_root)
                .output()
                .unwrap()
                .stdout,
        )
        .to_string();
        assert!(
            !list_after.contains("issue-777"),
            "stale registration should be pruned before enumeration, not left to rot: {list_after}"
        );
        assert_eq!(
            stats.errors, 0,
            "a directory that never became a read_dir entry must never surface as an error"
        );
    }

    #[test]
    fn record_cleanup_result_buckets_already_gone_as_not_an_error() {
        let mut stats = CleanupStats::default();
        record_cleanup_result(
            &mut stats,
            Path::new("/tmp/issue-4525"),
            Ok(CleanupOutcome::AlreadyGone),
        );
        assert_eq!(stats.stale_worktree_registrations, 1);
        assert_eq!(stats.cleaned_worktrees, 0);
        assert_eq!(stats.errors, 0);
    }

    #[test]
    fn record_cleanup_result_counts_removed_and_errors_separately() {
        let mut stats = CleanupStats::default();
        record_cleanup_result(&mut stats, Path::new("/tmp/issue-1"), Ok(CleanupOutcome::Removed));
        record_cleanup_result(&mut stats, Path::new("/tmp/issue-2"), Err("boom".to_string()));
        assert_eq!(stats.cleaned_worktrees, 1);
        assert_eq!(stats.errors, 1);
        assert_eq!(stats.stale_worktree_registrations, 0);
    }

    /// A removal failure that is NOT the untracked-orphan signature must still
    /// error (and must never trigger the direct-removal fallback), even for a
    /// managed path under the root.
    #[test]
    #[serial_test::serial]
    fn cleanup_worktree_does_not_force_remove_on_other_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_root = tmp.path().canonicalize().unwrap();
        // Not a git repository at all → `git worktree remove` fails with a
        // "not a git repository" error, which is NOT the orphan signature.
        let managed = crate::worktree_root::worktree_root(&repo_root).join("issue-1000");
        std::fs::create_dir_all(&managed).unwrap();
        std::fs::write(managed.join(".loom-managed"), "test").unwrap();

        let result = cleanup_worktree(&repo_root, &managed, 1000, false, "test");
        assert!(result.is_err(), "non-orphan failure must propagate");
        assert!(managed.exists(), "directory must be left in place on a non-orphan failure");
    }

    // --- pr-<N> worktree cleanup (#5939) ----------------------------------

    /// A `pr-<N>` worktree's branch is read from the worktree itself
    /// (`current_branch`), not constructed from `pr_num` — unlike
    /// `cleanup_worktree`, which always has a `feature/issue-<N>` name to
    /// build.
    #[test]
    #[serial_test::serial]
    fn cleanup_pr_worktree_deletes_the_worktrees_own_checked_out_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_root = tmp.path().canonicalize().unwrap();
        assert!(Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .current_dir(&repo_root)
            .status()
            .unwrap()
            .success());
        // A minimal commit so `git worktree add` has something to branch from.
        std::fs::write(repo_root.join("README.md"), "x").unwrap();
        assert!(Command::new("git")
            .args(["add", "."])
            .current_dir(&repo_root)
            .status()
            .unwrap()
            .success());
        assert!(Command::new("git")
            .args([
                "-c",
                "user.email=t@example.com",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "-m",
                "init"
            ])
            .current_dir(&repo_root)
            .status()
            .unwrap()
            .success());

        let wt_path = crate::worktree_root::worktree_root(&repo_root).join("pr-777");
        assert!(Command::new("git")
            .args(["worktree", "add", "-b", "some-external-fork-branch"])
            .arg(&wt_path)
            .current_dir(&repo_root)
            .status()
            .unwrap()
            .success());
        std::fs::write(wt_path.join(".loom-managed"), "test").unwrap();

        // The merged PR's head SHA == the local branch tip: the #4100 safety
        // criterion holds, so the force-delete is provably lossless.
        let tip = local_branch_tip(&repo_root, "some-external-fork-branch").unwrap();
        let result =
            cleanup_pr_worktree(&repo_root, &wt_path, 777, false, "test", Some(tip.as_str()));
        assert!(result.is_ok(), "{result:?}");
        assert!(!wt_path.exists());

        let branches = Command::new("git")
            .args(["branch", "--list", "some-external-fork-branch"])
            .current_dir(&repo_root)
            .output()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&branches.stdout).trim().is_empty(),
            "the PR worktree's own branch must be deleted, not left behind"
        );

        // #5950: the removal is in the ledger. This is the invariant that
        // makes both an entry and its absence evidence — before the #5939
        // review this was the one deletion path that wrote nothing.
        let ledger_path = crate::worktree_ops::removal_log::ledger_path(&repo_root);
        let ledger = std::fs::read_to_string(&ledger_path)
            .expect("the pr-<N> removal must append a ledger line");
        assert_eq!(ledger.lines().count(), 1, "exactly one line per removal: {ledger}");
        assert!(ledger.contains(r#""mechanism":"test""#), "{ledger}");
        assert!(ledger.contains(r#""branch":"some-external-fork-branch""#), "{ledger}");
        assert!(ledger.contains("classify_pr_worktree=Remove"), "{ledger}");
        assert!(ledger.contains("pr-777"), "{ledger}");
    }

    /// #6264 AC3: confirm `classify_pr_worktree`/`cleanup_pr_worktree` reap a
    /// `pr-<N>` worktree stuck on a **detached HEAD** — the exact state
    /// observed in the reported incident (`git worktree list` showing
    /// `pr-111 ... (detached HEAD)`), reproduced by #6264's investigation as
    /// the result of `pr-worktree.sh`'s `gh pr checkout --force` failing when
    /// the PR's branch is already checked out in another worktree — the same
    /// as a normal named-branch `pr-<N>` worktree
    /// ([`cleanup_pr_worktree_deletes_the_worktrees_own_checked_out_branch`]
    /// immediately above). This is a "confirm, don't build" AC: the
    /// classifier and remover are already branch-state-independent (keyed by
    /// worktree PATH + the PR's own status, resolved directly by PR number —
    /// see the module doc comment on `PrWorktreeProbes`), so this test is
    /// expected to pass unmodified; it exists to make that guarantee
    /// permanent rather than merely observed once during code review.
    #[test]
    #[serial_test::serial]
    fn classify_and_cleanup_pr_worktree_treat_a_detached_head_the_same_as_a_named_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_root = tmp.path().canonicalize().unwrap();
        assert!(Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .current_dir(&repo_root)
            .status()
            .unwrap()
            .success());
        std::fs::write(repo_root.join("README.md"), "x").unwrap();
        assert!(Command::new("git")
            .args(["add", "."])
            .current_dir(&repo_root)
            .status()
            .unwrap()
            .success());
        assert!(Command::new("git")
            .args([
                "-c",
                "user.email=t@example.com",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "-m",
                "init"
            ])
            .current_dir(&repo_root)
            .status()
            .unwrap()
            .success());

        // Mirrors pr-worktree.sh's own creation shape: `git worktree add
        // --detach` at a commit, with NO subsequent branch switch — modeling
        // the collision case where `gh pr checkout --force` failed and left
        // the worktree parked on detached HEAD instead of the PR's branch.
        let wt_path = crate::worktree_root::worktree_root(&repo_root).join("pr-888");
        assert!(Command::new("git")
            .args(["worktree", "add", "--detach"])
            .arg(&wt_path)
            .arg("main")
            .current_dir(&repo_root)
            .status()
            .unwrap()
            .success());
        std::fs::write(wt_path.join(".loom-managed"), "test").unwrap();

        // Sanity-check the fixture actually reproduces detached HEAD (i.e.
        // this test would fail loudly if the git invocation above ever
        // stopped doing what the comment claims).
        assert_eq!(
            current_branch(&wt_path),
            None,
            "fixture must be on a detached HEAD, matching the incident's own `git worktree list` output"
        );

        // classify_pr_worktree: a merged, grace-period-elapsed PR must decide
        // Remove for the detached worktree exactly as it would for a named
        // branch — no code path here consults the branch at all. Reuses the
        // production reaper's own option set (`safe: true`,
        // `require_managed_sentinel: true`) rather than hand-rolling one, so
        // this test exercises the same gates the live daemon reaper does.
        let opts = crate::worktree_reaper::reaper_clean_options(0);
        let merged_at = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
        let pr_status = |_: u32| PrStatus::Merged {
            merged_at: merged_at.clone(),
        };
        let probes = PrWorktreeProbes {
            in_use_marker: &|_: &Path| None,
            processes_using: &|_: &Path| Vec::new(),
            editable_installs: &|_: &Path| Vec::new(),
            is_managed: &|p: &Path| is_loom_managed(p),
            pr_status: &pr_status,
            branch_reachable_from_remotes: &|_: &Path| true,
            uncommitted: &check_uncommitted_or_untracked_changes,
            now: chrono::Utc::now(),
        };
        let decision = classify_pr_worktree(&wt_path, 888, &opts, &probes);
        assert_eq!(
            decision,
            WorktreeDecision::Remove,
            "a merged PR's detached-HEAD pr-<N> worktree must classify as Remove, same as a named-branch one: {decision:?}"
        );

        // cleanup_pr_worktree: actually removes it. No branch-delete is
        // attempted (there is no branch — current_branch returned None
        // above), only `git worktree remove --force` runs; that alone must
        // still succeed and the directory must be gone.
        let result = cleanup_pr_worktree(&repo_root, &wt_path, 888, false, "test", None);
        assert!(result.is_ok(), "{result:?}");
        assert!(!wt_path.exists(), "detached-HEAD pr-<N> worktree must be removed");
    }

    /// The #5939-review fix: a `pr-<N>` worktree whose local branch tip is NOT
    /// what the forge merged carries commits nobody pushed. The worktree is
    /// still removed (its contents are reclaimable), but the branch — the only
    /// surviving reference to those commits — must NOT be force-deleted.
    ///
    /// Before this, `git branch -d` was tried and *any* failure escalated to
    /// `-D`. Since this repo squash-merges, `-d` fails essentially always, so
    /// `-D` was the normal path, not the exception.
    #[test]
    #[serial_test::serial]
    fn cleanup_pr_worktree_keeps_a_branch_whose_tip_is_not_the_merged_head() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_root = tmp.path().canonicalize().unwrap();
        assert!(Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .current_dir(&repo_root)
            .status()
            .unwrap()
            .success());
        std::fs::write(repo_root.join("README.md"), "x").unwrap();
        assert!(Command::new("git")
            .args(["add", "."])
            .current_dir(&repo_root)
            .status()
            .unwrap()
            .success());
        assert!(Command::new("git")
            .args([
                "-c",
                "user.email=t@example.com",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "-m",
                "init"
            ])
            .current_dir(&repo_root)
            .status()
            .unwrap()
            .success());

        let wt_path = crate::worktree_root::worktree_root(&repo_root).join("pr-4242");
        assert!(Command::new("git")
            .args(["worktree", "add", "-b", "contributor/feature"])
            .arg(&wt_path)
            .current_dir(&repo_root)
            .status()
            .unwrap()
            .success());
        std::fs::write(wt_path.join(".loom-managed"), "test").unwrap();

        // A local commit that was never pushed — the branch tip now differs
        // from whatever the forge merged.
        std::fs::write(wt_path.join("unpushed.txt"), "work nobody has a copy of").unwrap();
        assert!(Command::new("git")
            .args(["add", "."])
            .current_dir(&wt_path)
            .status()
            .unwrap()
            .success());
        assert!(Command::new("git")
            .args([
                "-c",
                "user.email=t@example.com",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "-m",
                "local only"
            ])
            .current_dir(&wt_path)
            .status()
            .unwrap()
            .success());

        let result = cleanup_pr_worktree(
            &repo_root,
            &wt_path,
            4242,
            false,
            "test",
            Some("0000000000000000000000000000000000000000"),
        );
        assert!(result.is_ok(), "{result:?}");
        assert!(!wt_path.exists(), "the worktree itself is still reclaimed");

        let branches = Command::new("git")
            .args(["branch", "--list", "contributor/feature"])
            .current_dir(&repo_root)
            .output()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&branches.stdout).contains("contributor/feature"),
            "a branch whose tip is not the merged head must survive — it is the only \
             reference to the unpushed commit"
        );
    }

    /// An unresolvable head SHA (`None` — the forge probe failed) is the safe
    /// side, not a licence to force-delete.
    #[test]
    fn branch_delete_mode_never_forces_without_a_matching_tip() {
        assert_eq!(
            branch_delete_mode("feature/x", Some("abc123"), Some("abc123")),
            BranchDeleteMode::ForceSafe
        );
        assert_eq!(
            branch_delete_mode("feature/x", Some("abc123"), Some("def456")),
            BranchDeleteMode::SafeOnly
        );
        assert_eq!(
            branch_delete_mode("feature/x", Some("abc123"), None),
            BranchDeleteMode::SafeOnly
        );
        assert_eq!(
            branch_delete_mode("feature/x", None, Some("abc123")),
            BranchDeleteMode::SafeOnly
        );
        assert_eq!(branch_delete_mode("feature/x", None, None), BranchDeleteMode::SafeOnly);
        // An empty local tip is not a match against an empty expectation.
        assert_eq!(branch_delete_mode("feature/x", Some(""), Some("")), BranchDeleteMode::SafeOnly);
        // The protected-name floor wins over an otherwise-safe match.
        assert_eq!(
            branch_delete_mode("develop", Some("abc123"), Some("abc123")),
            BranchDeleteMode::Refuse
        );
    }

    /// Same untracked-orphan fallback `cleanup_worktree` has (#5177), exercised
    /// through the `pr-<N>` path.
    #[test]
    #[serial_test::serial]
    fn cleanup_pr_worktree_removes_untracked_orphan_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_root = tmp.path().canonicalize().unwrap();
        assert!(Command::new("git")
            .args(["init", "-q"])
            .current_dir(&repo_root)
            .status()
            .unwrap()
            .success());

        let orphan = crate::worktree_root::worktree_root(&repo_root).join("pr-999");
        std::fs::create_dir_all(&orphan).unwrap();
        std::fs::write(orphan.join(".loom-managed"), "test").unwrap();
        assert!(orphan.is_dir());

        let result = cleanup_pr_worktree(&repo_root, &orphan, 999, false, "test", None);
        assert!(result.is_ok(), "orphan removal should succeed: {result:?}");
        assert!(!orphan.exists(), "orphan directory should be gone");
    }

    #[test]
    fn cleanup_pr_worktree_dry_run_makes_no_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let wt_path = tmp.path().join("pr-42");
        std::fs::create_dir_all(&wt_path).unwrap();
        let result = cleanup_pr_worktree(tmp.path(), &wt_path, 42, true, "test", None);
        assert!(result.is_ok());
        assert!(wt_path.exists(), "dry-run must not remove anything");
    }

    #[test]
    fn current_branch_returns_none_for_a_non_git_directory() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(current_branch(tmp.path()), None);
    }

    /// The `pr-<N>` path is the only one whose branch name comes from outside
    /// Loom, so it is the only one that needs a protected-name floor before
    /// `git branch -D`.
    #[test]
    fn integration_branches_are_never_deletion_candidates() {
        for protected in [
            // The original four.
            "main",
            "master",
            "develop",
            "trunk",
            // Widened by the #5939 review — an allowlist of four names left
            // `staging`, `release/1.x` and `gh-pages` force-deletable.
            "development",
            "staging",
            "stage",
            "production",
            "prod",
            "gh-pages",
            "release/1.x",
            "releases/2026-08",
            "hotfix/2.3",
            "support/1.0",
            "maint/4",
            "stable/v2",
        ] {
            assert!(is_protected_branch_name(protected), "{protected}");
        }
        for deletable in [
            "feature/issue-5014",
            "docs/guide-update-20260810-005516",
            "hygiene/repo-all-20260731",
            "main-thing",
            "staging-area",
            "fix/release-notes",
        ] {
            assert!(!is_protected_branch_name(deletable), "{deletable}");
        }
    }

    /// A `pr-<N>` worktree parked on an integration branch is still removed —
    /// only the branch survives.
    #[test]
    #[serial_test::serial]
    fn cleanup_pr_worktree_removes_the_worktree_but_spares_an_integration_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_root = tmp.path().canonicalize().unwrap();
        assert!(Command::new("git")
            .args(["init", "-q", "-b", "primary"])
            .current_dir(&repo_root)
            .status()
            .unwrap()
            .success());
        std::fs::write(repo_root.join("README.md"), "x").unwrap();
        assert!(Command::new("git")
            .args(["add", "."])
            .current_dir(&repo_root)
            .status()
            .unwrap()
            .success());
        assert!(Command::new("git")
            .args([
                "-c",
                "user.email=t@example.com",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "-m",
                "init"
            ])
            .current_dir(&repo_root)
            .status()
            .unwrap()
            .success());

        // `develop` exists but is NOT the primary checkout's branch, so git's
        // own "checked out elsewhere" refusal would not save it.
        let wt_path = crate::worktree_root::worktree_root(&repo_root).join("pr-888");
        assert!(Command::new("git")
            .args(["worktree", "add", "-b", "develop"])
            .arg(&wt_path)
            .current_dir(&repo_root)
            .status()
            .unwrap()
            .success());
        std::fs::write(wt_path.join(".loom-managed"), "test").unwrap();

        let result = cleanup_pr_worktree(&repo_root, &wt_path, 888, false, "test", None);
        assert!(result.is_ok(), "{result:?}");
        assert!(!wt_path.exists(), "the worktree itself is still removed");

        let branches = Command::new("git")
            .args(["branch", "--list", "develop"])
            .current_dir(&repo_root)
            .output()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&branches.stdout).contains("develop"),
            "an integration branch must survive the pr-<N> worktree that held it"
        );
    }

    // --- tmux cleanup safety gates (#4890) -------------------------------

    #[test]
    fn tmux_safe_mode_skips_even_an_unattached_session() {
        // `--safe` is documented as "merged-PR-only mode", but a tmux session
        // has no PR association at all — the core of #4890 is that `--safe`
        // must not silently kill tmux sessions just because they are not
        // attached right now.
        assert_eq!(classify_tmux_session(true, false, false), TmuxDecision::SkipSafeMode);
    }

    #[test]
    fn tmux_safe_mode_skips_an_attached_session_too() {
        assert_eq!(classify_tmux_session(true, true, false), TmuxDecision::SkipSafeMode);
    }

    #[test]
    fn tmux_force_overrides_safe_mode() {
        // `--safe --force` together is the existing "trust me" combination
        // used elsewhere in this module (e.g. the grace-period/uncommitted
        // gates in `classify_worktree`).
        assert_eq!(classify_tmux_session(true, false, true), TmuxDecision::Kill);
    }

    #[test]
    fn tmux_attached_session_skipped_outside_safe_mode() {
        // A live operator terminal (attached client) must never be killed
        // without an explicit opt-in, even in plain (non-`--safe`) mode.
        assert_eq!(classify_tmux_session(false, true, false), TmuxDecision::SkipAttached);
    }

    #[test]
    fn tmux_force_overrides_attached_gate() {
        assert_eq!(classify_tmux_session(false, true, true), TmuxDecision::Kill);
    }

    #[test]
    fn tmux_unattached_session_killed_by_default() {
        assert_eq!(classify_tmux_session(false, false, false), TmuxDecision::Kill);
    }

    #[test]
    fn clear_stale_locks_no_dir_is_zero() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(clear_stale_spawn_loop_locks(dir.path(), true), 0);
    }

    // --- sweep-checkpoint transient pruning (#4450) ---------------------

    const HOUR: u64 = 3600;

    /// Build a checkpoint-dir fixture and return `(tempdir, checkpoint_dir)`.
    fn sweep_fixture() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let ckpt = sweep_checkpoint_dir(dir.path());
        std::fs::create_dir_all(&ckpt).unwrap();
        (dir, ckpt)
    }

    fn register_run(repo_root: &Path, run_id: &str, pid: u32) {
        let reg = sweep_run_registry_dir(repo_root);
        std::fs::create_dir_all(&reg).unwrap();
        std::fs::write(
            reg.join(format!("{run_id}.json")),
            format!(r#"{{"run_id": "{run_id}", "pid": {pid}, "timestamp": "now"}}"#),
        )
        .unwrap();
    }

    /// Run the pass with a clock advanced by `age_hours`, so every fixture
    /// file reads as exactly that old without touching filesystem mtimes.
    fn run_transients(
        repo_root: &Path,
        dry_run: bool,
        age_hours: u64,
        alive: &[u32],
        states: &[(u32, &str)],
    ) -> CleanupStats {
        let mut stats = CleanupStats::default();
        let alive: Vec<u32> = alive.to_vec();
        let states: Vec<(u32, String)> =
            states.iter().map(|(n, s)| (*n, (*s).to_string())).collect();
        let pid_alive = |pid: u32| alive.contains(&pid);
        let issue_state = |issue: u32| {
            states
                .iter()
                .find(|(n, _)| *n == issue)
                .map_or_else(|| "UNKNOWN".to_string(), |(_, s)| s.clone())
        };
        let env = SweepTransientEnv {
            now: SystemTime::now() + Duration::from_secs(age_hours * HOUR),
            min_age: Duration::from_secs(SWEEP_TRANSIENT_MIN_AGE_SECS),
            pid_alive: &pid_alive,
            issue_state: &issue_state,
        };
        clean_sweep_transients_with(repo_root, &mut stats, dry_run, &env);
        stats
    }

    #[test]
    fn sweep_transients_missing_dir_is_a_noop() {
        let dir = tempfile::tempdir().unwrap();
        let stats = run_transients(dir.path(), false, 100, &[], &[]);
        assert_eq!(stats.cleaned_sweep_baselines, 0);
        assert_eq!(stats.cleaned_sweep_checkpoints, 0);
        assert_eq!(stats.errors, 0);
    }

    #[test]
    fn sweep_transients_prunes_orphan_baseline_past_threshold() {
        let (dir, ckpt) = sweep_fixture();
        let orphan = ckpt.join("main-clean-baseline-sweep-dead.txt");
        std::fs::write(&orphan, "").unwrap();
        let stats = run_transients(dir.path(), false, 100, &[], &[]);
        assert!(!orphan.exists());
        assert_eq!(stats.cleaned_sweep_baselines, 1);
    }

    #[test]
    fn sweep_transients_keeps_young_orphan_baseline() {
        let (dir, ckpt) = sweep_fixture();
        let young = ckpt.join("main-clean-baseline-sweep-dead.txt");
        std::fs::write(&young, "").unwrap();
        let stats = run_transients(dir.path(), false, 1, &[], &[]);
        assert!(young.exists(), "mtime guard must spare a young baseline");
        assert_eq!(stats.cleaned_sweep_baselines, 0);
        assert_eq!(stats.kept_sweep_transients, 1);
    }

    #[test]
    fn sweep_transients_keep_live_run_baseline_regardless_of_age() {
        let (dir, ckpt) = sweep_fixture();
        let live = ckpt.join("main-clean-baseline-sweep-live.txt");
        std::fs::write(&live, "").unwrap();
        register_run(dir.path(), "sweep-live", 4242);
        // 1000h old, but the registered PID is alive.
        let stats = run_transients(dir.path(), false, 1000, &[4242], &[]);
        assert!(live.exists(), "a live run's baseline must never be pruned");
        assert_eq!(stats.cleaned_sweep_baselines, 0);
        assert_eq!(stats.kept_sweep_transients, 1);
    }

    /// #4691: a run whose PID exists but cannot be signalled by this process
    /// (`kill(2)` → `EPERM`) is LIVE. Wiring the real
    /// [`crate::sweep_registry::is_pid_alive_with`] decision core in here — with
    /// only the raw syscall mocked — proves the production `pid_alive` closure,
    /// not just the test double, keeps such a baseline.
    #[cfg(unix)]
    #[test]
    fn sweep_transients_keep_baseline_of_unsignallable_but_live_run() {
        use crate::sweep_registry::{is_pid_alive_with, EPERM};

        let (dir, ckpt) = sweep_fixture();
        let baseline = ckpt.join("main-clean-baseline-sweep-eperm.txt");
        std::fs::write(&baseline, "").unwrap();
        register_run(dir.path(), "sweep-eperm", 4242);

        let pid_alive = |pid: u32| is_pid_alive_with(pid, |_| Err(EPERM));
        let issue_state = |_: u32| "UNKNOWN".to_string();
        let mut stats = CleanupStats::default();
        let env = SweepTransientEnv {
            // 1000h old: only the liveness verdict can spare it.
            now: SystemTime::now() + Duration::from_secs(1000 * HOUR),
            min_age: Duration::from_secs(SWEEP_TRANSIENT_MIN_AGE_SECS),
            pid_alive: &pid_alive,
            issue_state: &issue_state,
        };
        clean_sweep_transients_with(dir.path(), &mut stats, false, &env);

        assert!(
            baseline.exists(),
            "an unsignallable (EPERM) PID means the sweep is still running — \
             its baseline must not be pruned"
        );
        assert_eq!(stats.cleaned_sweep_baselines, 0);
        assert_eq!(stats.kept_sweep_transients, 1);
    }

    /// The ESRCH counterpart of the test above: the same wiring, but the raw
    /// syscall reports "no such process" — the one failure mode that really does
    /// authorize pruning. Guards against an over-broad #4691 fix that makes
    /// every `kill(2)` failure mean "alive" and silently reinstates the leak.
    #[cfg(unix)]
    #[test]
    fn sweep_transients_still_prune_baseline_when_pid_is_esrch() {
        use crate::sweep_registry::is_pid_alive_with;
        const ESRCH: i32 = 3;

        let (dir, ckpt) = sweep_fixture();
        let baseline = ckpt.join("main-clean-baseline-sweep-gone.txt");
        std::fs::write(&baseline, "").unwrap();
        register_run(dir.path(), "sweep-gone", 4242);

        let pid_alive = |pid: u32| is_pid_alive_with(pid, |_| Err(ESRCH));
        let issue_state = |_: u32| "UNKNOWN".to_string();
        let mut stats = CleanupStats::default();
        let env = SweepTransientEnv {
            now: SystemTime::now() + Duration::from_secs(1000 * HOUR),
            min_age: Duration::from_secs(SWEEP_TRANSIENT_MIN_AGE_SECS),
            pid_alive: &pid_alive,
            issue_state: &issue_state,
        };
        clean_sweep_transients_with(dir.path(), &mut stats, false, &env);

        assert!(!baseline.exists(), "ESRCH means gone — prune must still fire");
        assert_eq!(stats.cleaned_sweep_baselines, 1);
    }

    #[test]
    fn sweep_transients_prunes_registered_but_dead_pid_baseline() {
        let (dir, ckpt) = sweep_fixture();
        let dead = ckpt.join("main-clean-baseline-sweep-crashed.txt");
        std::fs::write(&dead, "").unwrap();
        // Registry entry survives a SIGKILL — the PID liveness check is what
        // distinguishes it from a running sweep.
        register_run(dir.path(), "sweep-crashed", 999_999);
        let stats = run_transients(dir.path(), false, 100, &[], &[]);
        assert!(!dead.exists());
        assert_eq!(stats.cleaned_sweep_baselines, 1);
    }

    #[test]
    fn sweep_transients_keeps_baseline_with_unparseable_registry_entry() {
        let (dir, ckpt) = sweep_fixture();
        let path = ckpt.join("main-clean-baseline-sweep-corrupt.txt");
        std::fs::write(&path, "").unwrap();
        let reg = sweep_run_registry_dir(dir.path());
        std::fs::create_dir_all(&reg).unwrap();
        std::fs::write(reg.join("sweep-corrupt.json"), "{not json").unwrap();
        let stats = run_transients(dir.path(), false, 100, &[], &[]);
        assert!(path.exists(), "corrupt registry entry must fail safe (keep)");
        assert_eq!(stats.cleaned_sweep_baselines, 0);
    }

    #[test]
    fn sweep_transients_removes_legacy_unkeyed_baselines() {
        let (dir, ckpt) = sweep_fixture();
        let legacy = ckpt.join("main-clean-baseline.txt");
        std::fs::write(&legacy, "").unwrap();
        let older = dir.path().join(".loom").join("main-clean-baseline.txt");
        std::fs::write(&older, "").unwrap();
        // Age 0: the legacy files have no owner, so the threshold does not apply.
        let stats = run_transients(dir.path(), false, 0, &[], &[]);
        assert!(!legacy.exists());
        assert!(!older.exists());
        assert_eq!(stats.cleaned_sweep_baselines, 2);
    }

    #[test]
    fn sweep_transients_ignores_unrelated_files() {
        let (dir, ckpt) = sweep_fixture();
        let other = ckpt.join("notes.txt");
        std::fs::write(&other, "").unwrap();
        let weird = ckpt.join("main-clean-baseline-sweep-x.json");
        std::fs::write(&weird, "").unwrap();
        let stats = run_transients(dir.path(), false, 1000, &[], &[]);
        assert!(other.exists());
        assert!(weird.exists());
        assert_eq!(stats.cleaned_sweep_baselines, 0);
        assert_eq!(stats.cleaned_sweep_checkpoints, 0);
    }

    #[test]
    fn sweep_transients_dry_run_deletes_nothing_but_counts() {
        let (dir, ckpt) = sweep_fixture();
        let baseline = ckpt.join("main-clean-baseline-sweep-dead.txt");
        std::fs::write(&baseline, "").unwrap();
        let legacy = ckpt.join("main-clean-baseline.txt");
        std::fs::write(&legacy, "").unwrap();
        let checkpoint = ckpt.join("issue-3784.json");
        std::fs::write(&checkpoint, "{}").unwrap();
        let stats = run_transients(dir.path(), true, 100, &[], &[(3784, "CLOSED")]);
        assert!(baseline.exists());
        assert!(legacy.exists());
        assert!(checkpoint.exists());
        assert_eq!(stats.cleaned_sweep_baselines, 2);
        assert_eq!(stats.cleaned_sweep_checkpoints, 1);
    }

    #[test]
    fn sweep_transients_prunes_closed_issue_checkpoint_only() {
        let (dir, ckpt) = sweep_fixture();
        let closed = ckpt.join("issue-3784.json");
        let open = ckpt.join("issue-4450.json");
        let unknown = ckpt.join("issue-4451.json");
        for p in [&closed, &open, &unknown] {
            std::fs::write(p, "{}").unwrap();
        }
        let stats =
            run_transients(dir.path(), false, 100, &[], &[(3784, "CLOSED"), (4450, "OPEN")]);
        assert!(!closed.exists());
        assert!(open.exists(), "OPEN issue checkpoint must be kept");
        assert!(unknown.exists(), "an unverified issue state must never delete");
        assert_eq!(stats.cleaned_sweep_checkpoints, 1);
        assert_eq!(stats.kept_sweep_transients, 2);
    }

    #[test]
    fn sweep_transients_keeps_young_closed_issue_checkpoint() {
        let (dir, ckpt) = sweep_fixture();
        let closed = ckpt.join("issue-3784.json");
        std::fs::write(&closed, "{}").unwrap();
        let stats = run_transients(dir.path(), false, 1, &[], &[(3784, "CLOSED")]);
        assert!(closed.exists(), "age gate also bounds forge probes");
        assert_eq!(stats.cleaned_sweep_checkpoints, 0);
    }

    #[test]
    fn sweep_transients_keeps_checkpoint_of_in_flight_sweep() {
        let (dir, ckpt) = sweep_fixture();
        let inflight = ckpt.join("issue-3784.json");
        std::fs::write(&inflight, "{}").unwrap();
        // A daemon-owned sweep holds a claim lock for this issue.
        std::fs::create_dir_all(super::super::liveness::locks_dir(dir.path()).join("issue-3784"))
            .unwrap();
        let stats = run_transients(dir.path(), false, 1000, &[], &[(3784, "CLOSED")]);
        assert!(
            inflight.exists(),
            "an in-flight sweep's checkpoint must survive even when its issue is CLOSED"
        );
        assert_eq!(stats.cleaned_sweep_checkpoints, 0);
        assert_eq!(stats.kept_sweep_transients, 1);
    }

    // --- actionable error reporting (#4877) ------------------------------

    fn git(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "git {args:?} failed in {}", dir.display());
    }

    /// A recorded error must name the target, the operation, and the cause —
    /// the three things `Errors: 1` on its own withholds.
    #[test]
    fn record_error_names_target_operation_and_cause() {
        let mut stats = CleanupStats::default();
        stats.record_error("branch feature/issue-42", "git branch -D", "error: branch not found");
        assert_eq!(stats.errors, 1);
        assert_eq!(
            stats.error_details,
            vec![
                "git branch -D failed for branch feature/issue-42: error: branch not found"
                    .to_string()
            ]
        );
    }

    #[test]
    fn error_line_without_a_cause_still_names_target_and_operation() {
        assert_eq!(
            error_line("/tmp/wt", "git worktree remove --force", "  "),
            "git worktree remove --force failed for /tmp/wt"
        );
    }

    /// The reported original symptom: a failed `git branch -D` bumped the
    /// counter and printed nothing. The failure must now surface a diagnostic
    /// naming the branch *and* git's own message.
    #[test]
    fn failed_branch_delete_reports_branch_and_git_error() {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "-q"]);

        let branch = "feature/issue-4877";
        let cause = force_delete_branch(dir.path(), branch)
            .expect_err("deleting a nonexistent branch must fail");

        let mut stats = CleanupStats::default();
        stats.record_error(&format!("branch {branch}"), "git branch -D", &cause);

        assert_eq!(stats.errors, 1);
        let detail = &stats.error_details[0];
        assert!(detail.contains(branch), "diagnostic must name the branch: {detail}");
        assert!(detail.contains("git branch -D"), "diagnostic must name the operation: {detail}");
        assert!(
            detail.to_lowercase().contains("not found"),
            "diagnostic must carry git's underlying error: {detail}"
        );
    }

    /// A successful `git branch -D` must stay silent (no error inflation).
    #[test]
    fn successful_branch_delete_records_no_error() {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "-q", "--initial-branch=main"]);
        git(dir.path(), &["config", "user.email", "loom@example.com"]);
        git(dir.path(), &["config", "user.name", "Loom Test"]);
        git(dir.path(), &["commit", "-q", "--allow-empty", "-m", "seed"]);
        git(dir.path(), &["branch", "feature/issue-4877"]);

        assert!(force_delete_branch(dir.path(), "feature/issue-4877").is_ok());
    }

    // --- `--safe --branches-only` reachability gate (#5737) --------------

    #[test]
    fn classify_stale_branch_outside_safe_mode_always_removes() {
        // Pre-#5737 behavior: absence of a tracking branch is sufficient on
        // its own outside `--safe`, regardless of reachability/PR status.
        assert_eq!(classify_stale_branch(false, false, false), StaleBranchDecision::Remove);
        assert_eq!(classify_stale_branch(false, true, true), StaleBranchDecision::Remove);
    }

    #[test]
    fn classify_stale_branch_safe_mode_requires_reachability_or_merged_pr() {
        assert_eq!(classify_stale_branch(true, true, false), StaleBranchDecision::Remove);
        assert_eq!(classify_stale_branch(true, false, true), StaleBranchDecision::Remove);
    }

    #[test]
    fn classify_stale_branch_safe_mode_keeps_truly_unreachable_work() {
        // The #5737 repro: no remote ref holds these commits and no PR
        // merged them - deleting would destroy the only copy.
        assert_eq!(classify_stale_branch(true, false, false), StaleBranchDecision::KeepUnreachable);
    }

    #[test]
    fn retained_prefix_matches_backup_and_preserve() {
        assert_eq!(retained_prefix("backup/issue-4749-doctor-rebase"), Some("backup/"));
        assert_eq!(retained_prefix("preserve-bf0d1b83-version-bump"), Some("preserve-"));
        assert_eq!(retained_prefix("feature/issue-42"), None);
    }

    fn local_branch_names(repo_root: &Path) -> Vec<String> {
        String::from_utf8_lossy(
            &Command::new("git")
                .args(["branch", "--format=%(refname:short)"])
                .current_dir(repo_root)
                .output()
                .unwrap()
                .stdout,
        )
        .lines()
        .map(str::to_string)
        .collect()
    }

    /// Regression lock for issue #5737's own repro + suggested AC: a
    /// local-only branch holding an unpushed commit and no tracking branch
    /// must survive `--safe --branches-only`, while a branch whose commits
    /// are already reachable via another remote ref (the "PR merged, remote
    /// auto-deleted" shape) is still deleted.
    #[test]
    fn safe_branch_cleanup_keeps_unpushed_work_but_deletes_reachable_stale_branch() {
        let origin_dir = tempfile::tempdir().unwrap();
        git(origin_dir.path(), &["init", "-q", "--bare"]);

        let repo_dir = tempfile::tempdir().unwrap();
        let repo_root = repo_dir.path();
        git(repo_root, &["init", "-q", "--initial-branch=main"]);
        git(repo_root, &["config", "user.email", "loom@example.com"]);
        git(repo_root, &["config", "user.name", "Loom Test"]);
        git(repo_root, &["commit", "-q", "--allow-empty", "-m", "seed"]);
        git(
            repo_root,
            &[
                "remote",
                "add",
                "origin",
                origin_dir.path().to_str().unwrap(),
            ],
        );
        git(repo_root, &["push", "-q", "origin", "main"]);

        // A branch whose tip is already reachable via origin/main (e.g. its
        // own remote branch was auto-deleted after a merge) - no NEW
        // commits, so deleting it loses nothing.
        git(repo_root, &["branch", "already-landed"]);

        // A branch holding a genuinely unpushed commit: no tracking branch,
        // AND its commit is reachable from no remote ref at all. This is
        // the #5737 repro - it must survive `--safe`.
        git(repo_root, &["checkout", "-q", "-b", "unpushed-work"]);
        git(repo_root, &["commit", "-q", "--allow-empty", "-m", "never pushed"]);
        git(repo_root, &["checkout", "-q", "main"]);

        let mut stats = CleanupStats::default();
        let opts = CleanOptions {
            safe: true,
            ..CleanOptions::default()
        };
        clean_branches(repo_root, &mut stats, &opts);

        let remaining = local_branch_names(repo_root);
        assert!(
            !remaining.contains(&"already-landed".to_string()),
            "a branch already reachable from another remote ref must still be deleted: {remaining:?}"
        );
        assert!(
            remaining.contains(&"unpushed-work".to_string()),
            "unpushed work with no remote ref anywhere must survive --safe: {remaining:?}"
        );
        assert_eq!(stats.cleaned_branches, 1);
        assert_eq!(stats.kept_branches, 1);
    }

    /// `backup/`/`preserve-` prefixed branches are retained by default under
    /// `--safe`, even without any reachability computation needed to save
    /// them.
    #[test]
    fn safe_branch_cleanup_retains_backup_prefixed_branch_by_default() {
        let repo_dir = tempfile::tempdir().unwrap();
        let repo_root = repo_dir.path();
        git(repo_root, &["init", "-q", "--initial-branch=main"]);
        git(repo_root, &["config", "user.email", "loom@example.com"]);
        git(repo_root, &["config", "user.name", "Loom Test"]);
        git(repo_root, &["commit", "-q", "--allow-empty", "-m", "seed"]);
        git(repo_root, &["branch", "backup/issue-4749-doctor-rebase"]);

        let mut stats = CleanupStats::default();
        let opts = CleanOptions {
            safe: true,
            ..CleanOptions::default()
        };
        clean_branches(repo_root, &mut stats, &opts);

        let remaining = local_branch_names(repo_root);
        assert!(
            remaining.contains(&"backup/issue-4749-doctor-rebase".to_string()),
            "backup/ prefix must be retained under --safe: {remaining:?}"
        );
        assert_eq!(stats.kept_branches, 1);
        assert_eq!(stats.cleaned_branches, 0);
    }

    /// `--safe --force` (the same "trust me" combination used elsewhere in
    /// this module) overrides the retain-prefix gate specifically - proven
    /// by pairing the prefix with commits that ARE independently reachable
    /// (so the underlying safety net alone would already permit removal;
    /// only the prefix gate is what changes with `--force`).
    #[test]
    fn safe_force_overrides_retain_prefix_gate() {
        let origin_dir = tempfile::tempdir().unwrap();
        git(origin_dir.path(), &["init", "-q", "--bare"]);

        let repo_dir = tempfile::tempdir().unwrap();
        let repo_root = repo_dir.path();
        git(repo_root, &["init", "-q", "--initial-branch=main"]);
        git(repo_root, &["config", "user.email", "loom@example.com"]);
        git(repo_root, &["config", "user.name", "Loom Test"]);
        git(repo_root, &["commit", "-q", "--allow-empty", "-m", "seed"]);
        git(
            repo_root,
            &[
                "remote",
                "add",
                "origin",
                origin_dir.path().to_str().unwrap(),
            ],
        );
        git(repo_root, &["push", "-q", "origin", "main"]);
        // Same tip as origin/main -> reachable from a remote ref.
        git(repo_root, &["branch", "backup/reachable-but-prefixed"]);

        // Without --force: the prefix gate wins even though the branch is
        // independently reachable.
        let mut stats = CleanupStats::default();
        let opts = CleanOptions {
            safe: true,
            ..CleanOptions::default()
        };
        clean_branches(repo_root, &mut stats, &opts);
        assert!(
            local_branch_names(repo_root).contains(&"backup/reachable-but-prefixed".to_string()),
            "prefix gate must retain the branch without --force"
        );
        assert_eq!(stats.kept_branches, 1);

        // With --force: the prefix gate no longer applies, and the branch's
        // own reachability already permits removal.
        let mut stats = CleanupStats::default();
        let opts = CleanOptions {
            safe: true,
            force: true,
            ..CleanOptions::default()
        };
        clean_branches(repo_root, &mut stats, &opts);
        assert!(
            !local_branch_names(repo_root).contains(&"backup/reachable-but-prefixed".to_string()),
            "--force must override the retain-prefix gate"
        );
        assert_eq!(stats.cleaned_branches, 1);
    }

    /// AC #3: deletion output must print the branch SHA (matching the
    /// worktree half's `HEAD=<sha> (recoverable via `git reflog`)` hint).
    #[test]
    fn branch_sha_and_hint_render_the_head_commit() {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "-q", "--initial-branch=main"]);
        git(dir.path(), &["config", "user.email", "loom@example.com"]);
        git(dir.path(), &["config", "user.name", "Loom Test"]);
        git(dir.path(), &["commit", "-q", "--allow-empty", "-m", "seed"]);

        let sha = branch_sha(dir.path(), "main").expect("HEAD must resolve");
        assert_eq!(sha.len(), 12, "short SHA must be 12 chars: {sha}");

        let hint = sha_hint(dir.path(), "main");
        assert!(hint.contains(&sha), "hint must carry the SHA: {hint}");
        assert!(hint.contains("recoverable via"), "hint must name the recovery path: {hint}");
        assert!(hint.contains("git reflog"), "hint must name git reflog: {hint}");
    }

    #[test]
    fn branch_sha_is_none_for_a_nonexistent_branch() {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "-q"]);
        assert!(branch_sha(dir.path(), "does-not-exist").is_none());
        assert_eq!(sha_hint(dir.path(), "does-not-exist"), "");
    }

    /// AC #3: a run that recorded errors must not read like a clean one.
    #[test]
    fn completion_line_differs_when_errors_occurred() {
        let clean_run = completion_line("Cleanup", false, 0);
        let errored_run = completion_line("Cleanup", false, 1);
        assert_eq!(clean_run, "Cleanup complete!");
        assert_ne!(clean_run, errored_run);
        assert!(errored_run.contains('1'), "closing line must carry the count: {errored_run}");
        assert!(errored_run.contains("error"), "closing line must say error: {errored_run}");
        // Plural agreement, and the same rule for aggressive mode.
        assert!(completion_line("Cleanup", false, 2).contains("2 errors"));
        assert!(completion_line("Aggressive cleanup", false, 0).starts_with("Aggressive cleanup"));
        assert_ne!(
            completion_line("Aggressive cleanup", false, 0),
            completion_line("Aggressive cleanup", false, 3)
        );
    }

    /// A dry run that hit errors (e.g. an unresolvable PR status) must also
    /// read differently from a clean dry run.
    #[test]
    fn completion_line_dry_run_reflects_errors() {
        let clean_run = completion_line("Cleanup", true, 0);
        let errored_run = completion_line("Cleanup", true, 1);
        assert_eq!(clean_run, "Dry run complete - no changes made");
        assert_ne!(clean_run, errored_run);
        assert!(errored_run.contains("1 error"), "{errored_run}");
    }

    /// AC #4 regression lock: the exit status distinguishes "completed with
    /// errors" from "completed cleanly".
    #[test]
    fn exit_code_is_nonzero_exactly_when_errors_occurred() {
        assert_eq!(exit_code(0), 0);
        assert_eq!(exit_code(1), 1);
        assert_eq!(exit_code(7), 1);
    }

    /// End-to-end lock on the clean-run contract: a pass with nothing to do
    /// returns 0. Scoped to `worktrees_only` so the pass touches nothing
    /// outside the temp repo (no tmux sockets, no branches).
    #[test]
    fn run_clean_returns_zero_when_no_errors_occurred() {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "-q"]);
        let opts = CleanOptions {
            force: true,
            worktrees_only: true,
            ..CleanOptions::default()
        };
        assert_eq!(run_clean(dir.path(), &opts), 0);
    }

    #[test]
    fn clear_stale_locks_keeps_live_and_removes_dead() {
        let dir = tempfile::tempdir().unwrap();
        let locks = spawn_loop_locks_dir(dir.path());
        std::fs::create_dir_all(locks.join("issue-1")).unwrap();
        std::fs::create_dir_all(locks.join("issue-2")).unwrap();
        std::fs::create_dir_all(dir.path().join(".loom")).unwrap();
        std::fs::write(
            dir.path().join(".loom").join("spawn-loop-state.json"),
            r#"{"running": [{"issue": 1, "pid": 1}]}"#,
        )
        .unwrap();
        let removed = clear_stale_spawn_loop_locks(dir.path(), false);
        assert_eq!(removed, 1);
        assert!(locks.join("issue-1").exists());
        assert!(!locks.join("issue-2").exists());
    }

    // --- quarantine_dirty_worktree (#6653) --------------------------------

    fn init_repo_with_seed_commit(dir: &Path) {
        git(dir, &["init", "-q", "--initial-branch=main"]);
        git(dir, &["config", "user.email", "loom@example.com"]);
        git(dir, &["config", "user.name", "Loom Test"]);
        git(dir, &["commit", "-q", "--allow-empty", "-m", "seed"]);
    }

    #[test]
    fn quarantine_dirty_worktree_stashes_uncommitted_and_untracked_changes() {
        let dir = tempfile::tempdir().unwrap();
        init_repo_with_seed_commit(dir.path());
        std::fs::write(dir.path().join("tracked.txt"), "v1").unwrap();
        git(dir.path(), &["add", "tracked.txt"]);
        git(dir.path(), &["commit", "-q", "-m", "add tracked"]);
        std::fs::write(dir.path().join("tracked.txt"), "v2 (uncommitted)").unwrap();
        std::fs::write(dir.path().join("untracked.txt"), "new file").unwrap();

        let sha = quarantine_dirty_worktree(dir.path(), "issue=6653 reason=test")
            .expect("dirty worktree must be quarantined");
        assert!(!sha.trim().is_empty());

        // The working tree is clean again — the dirt moved into the stash.
        assert!(!check_uncommitted_or_untracked_changes(dir.path()));
        assert_eq!(std::fs::read_to_string(dir.path().join("tracked.txt")).unwrap(), "v1");
        assert!(!dir.path().join("untracked.txt").exists());

        // The stash carries the load-bearing `loom-quarantine:` label so the
        // existing stash_retirement / quarantine_stash_status machinery can
        // find it.
        let list = String::from_utf8_lossy(
            &Command::new("git")
                .args(["log", "-g", "--format=%gs", "refs/stash"])
                .current_dir(dir.path())
                .output()
                .unwrap()
                .stdout,
        )
        .to_string();
        assert!(list.contains("loom-quarantine: issue=6653 reason=test"), "{list}");
    }

    #[test]
    fn quarantine_dirty_worktree_returns_none_when_nothing_to_stash() {
        let dir = tempfile::tempdir().unwrap();
        init_repo_with_seed_commit(dir.path());

        assert_eq!(quarantine_dirty_worktree(dir.path(), "issue=1 reason=test"), None);
    }
}
