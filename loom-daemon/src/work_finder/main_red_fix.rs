//! The red-main-fix lane (#9244 §6).
//!
//! An issue whose body carries `<!-- loom:main-red-fix -->` claims to fix a
//! red `main`. Two things follow, and **only while that repo's `main` is
//! verified red**:
//!
//! - it sorts ahead of every unstarred candidate (key 3 of
//!   [`super::ordering::candidate_cmp`]);
//! - a repo the main-health gate has halted still admits it — and nothing
//!   else. Every other ready issue in that repo, starred or not, keeps the
//!   `WorkspaceHalted` disposition.
//!
//! "Verified red" is [`WorkspaceHealthStates::is_halted`], never the
//! work finder's per-root `halted` hold, which is also set while a gate run is
//! merely in flight (`suppress_dispatch_during_gate`), during a drain, under
//! the host breaker and by the pre-flight / pool holds. None of those is a red
//! `main`, so none of them opens the lane. A repo whose gate is disabled has
//! no verified-red signal at all, so the latest default-branch CI conclusion
//! stands in for it — one cached forge read per repo per tick, made only when
//! the repo has a marker-bearing candidate.
//!
//! A marker on a green repo is inert: no boost, no halt bypass.
//!
//! # Unpromoted fixes (#10118)
//!
//! A fix is usually filed in `loom:triage` (or curated but not yet promoted),
//! so it is not in the `loom:issue` listing and could never reach the lane.
//! The work finder therefore also lists `loom:triage` and `loom:curated` and
//! keeps the marker-bearing rows ([`merge_red_fix_candidates`]). Those rows
//! are admitted **only while the repo is red** ([`evaluate`] drops them
//! otherwise), so a marker on a green repo is still inert. Dispatching an
//! unpromoted issue needs no new path: the registry's pre-flip classifier
//! returns `NotYetApproved` and the child sweep starts from Curator.
//!
//! Because that path skips the human `loom:issue` promotion, the marker on an
//! unpromoted row counts **only when a trusted identity filed the issue**
//! (the [`crate::comment_trust`] rules, #9548: a repo insider, this fleet's
//! Apps, the daemon's own identity, or the configured allowlist / fleet admin
//! roster). The repo is public and the marker is an invisible HTML comment,
//! so an outsider's issue that reaches triage carrying it is content, not
//! control: it is not admitted, and waits for promotion like any other issue.
//!
//! If a fix stays unclaimed on a red repo for longer than
//! [`RED_FIX_ESCALATE_AFTER`] (config:
//! `autonomous.workFinder.redFixEscalateAfterSecs`), the operator is alerted
//! once ([`RedFixWatch`]) instead of the repo waiting silently.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::{WorkDispatcher, WorkItem};
use crate::cmd_out::{CmdOutcome, Unavailable};
use crate::main_health_gate::{MainHealthState, WorkspaceHealthStates};

/// The body marker declaring an issue a fix for a red `main` (#9244).
pub const MAIN_RED_FIX_MARKER: &str = "<!-- loom:main-red-fix -->";

/// How long one repo's CI-fallback verdict is reused: about one tick, so the
/// fallback costs at most one forge read per repo per tick.
pub const CI_FALLBACK_TTL: Duration = Duration::from_secs(60);

/// The pre-promotion labels listed for unpromoted fixes (#10118). An issue
/// with no workflow label at all is not listable this way (a known limit), so
/// fix filers apply `loom:triage`.
pub const UNPROMOTED_LABELS: [&str; 2] = ["loom:triage", "loom:curated"];

/// Default for how long a fix may sit unclaimed on a red repo before the
/// operator is alerted (#10118). An initial value: with the unpromoted
/// listing, a fix should be claimed within a tick or two, so half an hour
/// unclaimed means something is stuck. Retune from measured time-to-claim via
/// `autonomous.workFinder.redFixEscalateAfterSecs` (read live).
pub const RED_FIX_ESCALATE_AFTER: Duration = Duration::from_secs(30 * 60);

impl WorkItem {
    /// A marker-bearing row that only the unpromoted listing contributed:
    /// not `loom:issue` and not starred (a starred row is a candidate on its
    /// own). Such a row is a candidate only while the repo is red (#10118).
    #[must_use]
    pub fn is_unpromoted_red_fix(&self) -> bool {
        self.is_main_red_fix()
            && !self.labels.iter().any(|l| l == "loom:issue")
            && !self.is_operator_priority()
    }

    /// Builder-style setter for [`Self::author`] (#10118).
    #[must_use]
    pub fn with_author(mut self, author: Option<crate::comment_trust::Author>) -> Self {
        self.author = author;
        self
    }

    /// True when [`Self::body`] carries [`MAIN_RED_FIX_MARKER`] at the start
    /// of a line (the same line-anchored style as the complexity and
    /// recheck-interval markers, so prose quoting the marker mid-sentence does
    /// not fire).
    #[must_use]
    pub fn is_main_red_fix(&self) -> bool {
        self.body.as_deref().is_some_and(|b| {
            b.lines()
                .any(|l| l.trim_start().starts_with(MAIN_RED_FIX_MARKER))
        })
    }
}

/// Fold one unpromoted listing ([`UNPROMOTED_LABELS`]) into the candidate
/// rows (#10118): keep only marker-bearing rows not already listed, and drop
/// the rows [`super::operator_priority::excluded_from_side_listing`] drops
/// from the starred listing (claimed, being curated, or Champion-path). Every
/// kept row still goes through the normal skip filters.
///
/// A row is kept only when `trusts` believes its [`WorkItem::author`] (#9548):
/// an unpromoted row skips the human promotion, so its marker must come from
/// a trusted filer. A row with no author (a listing without one) is never
/// kept. `trusts` runs only for marker-bearing rows, so a repo with none never
/// resolves a trust policy.
#[must_use]
pub fn merge_red_fix_candidates(
    mut ready: Vec<WorkItem>,
    rows: Vec<WorkItem>,
    mut trusts: impl FnMut(&crate::comment_trust::Author) -> bool,
) -> Vec<WorkItem> {
    let listed: HashSet<u32> = ready.iter().map(|i| i.number).collect();
    ready.extend(rows.into_iter().filter(|i| {
        if !i.is_main_red_fix()
            || listed.contains(&i.number)
            || super::operator_priority::excluded_from_side_listing(i)
        {
            return false;
        }
        let trusted = i.author.as_ref().is_some_and(&mut trusts);
        if !trusted {
            log::debug!(
                "work_finder: #{} carries the red-main-fix marker but its author ({}) is not \
                 trusted; not admitting it unpromoted (#10118, #9548)",
                i.number,
                i.author
                    .as_ref()
                    .and_then(|a| a.login.as_deref())
                    .unwrap_or("unknown"),
            );
        }
        trusted
    }));
    ready
}

/// Resolve one repo's lane for this tick's listing: whether `main` counts as
/// red, with the unpromoted fixes dropped from `ready` when it does not
/// (#10118), and the escalation watch fed with the fixes still waiting.
pub fn evaluate<D: WorkDispatcher + ?Sized>(
    lane: RedMainLane,
    ready: &mut Vec<WorkItem>,
    dispatcher: &mut D,
) -> bool {
    let red = lane.is_red(ready, || dispatcher.main_red_via_ci());
    if !red {
        ready.retain(|i| !i.is_unpromoted_red_fix());
    }
    let waiting: Vec<u32> = ready
        .iter()
        .filter(|i| red && i.is_main_red_fix())
        .map(|i| i.number)
        .collect();
    dispatcher.escalate_red_fix(red, &waiting);
    red
}

/// Per-repo record of how long each fix has waited unclaimed on a red `main`
/// (#10118), so the operator is alerted once per fix past the threshold.
#[derive(Debug, Default)]
pub struct RedFixWatch {
    first_seen: HashMap<u32, Instant>,
    alerted: HashSet<u32>,
    /// Fixes whose alert filing failed: consecutive failures and the earliest
    /// next attempt, so a transient forge failure is retried with backoff.
    retry: HashMap<u32, (u32, Instant)>,
}

/// First retry delay after a failed alert filing; doubles per failure.
const ALERT_RETRY_BASE: Duration = Duration::from_secs(60);
/// Ceiling on the alert retry delay.
const ALERT_RETRY_MAX: Duration = Duration::from_secs(3600);

impl RedFixWatch {
    /// Record this tick's waiting fixes and return the ones due an alert now,
    /// with how long each has waited. A fix is due once, when it has waited
    /// at least `after` and keeps being due until [`Self::record_attempt`]
    /// reports a successful filing (failed attempts back off). A green tick,
    /// or a fix leaving the waiting set (it was claimed), resets its clock.
    pub fn observe(
        &mut self,
        now: Instant,
        red: bool,
        waiting: &[u32],
        after: Duration,
    ) -> Vec<(u32, Duration)> {
        let keep: HashSet<u32> = if red {
            waiting.iter().copied().collect()
        } else {
            HashSet::new()
        };
        self.first_seen.retain(|n, _| keep.contains(n));
        self.alerted.retain(|n| keep.contains(n));
        self.retry.retain(|n, _| keep.contains(n));
        let mut due = Vec::new();
        for &n in waiting.iter().filter(|n| keep.contains(n)) {
            let waited = now.saturating_duration_since(*self.first_seen.entry(n).or_insert(now));
            let backed_off = self.retry.get(&n).is_some_and(|(_, at)| now < *at);
            if waited >= after && !self.alerted.contains(&n) && !backed_off {
                due.push((n, waited));
            }
        }
        due
    }

    /// Record the outcome of filing `n`'s alert: success acknowledges it (no
    /// later tick files again); failure schedules a retry with doubling
    /// backoff, capped at [`ALERT_RETRY_MAX`].
    pub fn record_attempt(&mut self, now: Instant, n: u32, filed: bool) {
        if filed {
            self.retry.remove(&n);
            self.alerted.insert(n);
            return;
        }
        let failures = self.retry.get(&n).map_or(0, |(f, _)| *f).saturating_add(1);
        let delay = ALERT_RETRY_BASE
            .saturating_mul(1u32 << (failures - 1).min(16))
            .min(ALERT_RETRY_MAX);
        self.retry.insert(n, (failures, now + delay));
    }
}

/// The escalation threshold for `root`: `autonomous.workFinder.
/// redFixEscalateAfterSecs` when set to a positive integer, else
/// [`RED_FIX_ESCALATE_AFTER`]. Read on each call, so a change applies live.
#[must_use]
pub fn escalate_after(root: &Path) -> Duration {
    let effective = crate::config_resolver::resolve_effective_config(root);
    crate::config_resolver::get_path(&effective, "autonomous.workFinder.redFixEscalateAfterSecs")
        .and_then(serde_json::Value::as_u64)
        .filter(|s| *s > 0)
        .map_or(RED_FIX_ESCALATE_AFTER, Duration::from_secs)
}

fn watches() -> &'static Mutex<HashMap<PathBuf, RedFixWatch>> {
    static WATCHES: OnceLock<Mutex<HashMap<PathBuf, RedFixWatch>>> = OnceLock::new();
    WATCHES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The production escalation for `root` (the work finder rebuilds its
/// dispatchers every tick, so the watch is process-wide): feed the repo's
/// watch and file one `loom:operator` alert per fix that is due.
pub fn escalate_global(root: &Path, red: bool, waiting: &[u32]) {
    let after = if red && !waiting.is_empty() {
        escalate_after(root)
    } else {
        RED_FIX_ESCALATE_AFTER
    };
    let due = {
        let mut guard = watches()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard
            .entry(root.to_path_buf())
            .or_default()
            .observe(Instant::now(), red, waiting, after)
    };
    for (issue, waited) in due {
        let filed = file_alert(root, issue, waited);
        watches()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(root.to_path_buf())
            .or_default()
            .record_attempt(Instant::now(), issue, filed);
    }
}

/// The alert issue's title for an unclaimed fix.
#[must_use]
pub fn alert_title(issue: u32) -> String {
    format!("Red main: main-red-fix #{issue} is still unclaimed")
}

/// How `create-issue.sh` answered an alert filing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AlertOutcome {
    /// Created, or blocked by this fix's own alert: the alert exists.
    Exists,
    /// Blocked by a similar open issue that is not this fix's alert (exit 3):
    /// nothing exists for this fix yet.
    BlockedBySimilar,
    Failed,
}

/// Classify a `create-issue.sh` run. Exit 3 means a *similar* open issue
/// blocked creation — alerts for different fixes differ only by number — so it
/// counts as this fix's alert only when a blocking `#N: <title> (similarity:
/// X%)` row on `stderr` carries exactly [`alert_title`] for `issue`.
fn classify_alert_run(code: Option<i32>, stderr: &str, issue: u32) -> AlertOutcome {
    match code {
        Some(0) => AlertOutcome::Exists,
        Some(3) => {
            let title = alert_title(issue);
            let own = stderr.lines().any(|l| {
                l.trim()
                    .strip_prefix('#')
                    .and_then(|r| r.split_once(": "))
                    .is_some_and(|(_, t)| {
                        t.rsplit_once(" (similarity:").map_or(t, |(t, _)| t) == title
                    })
            });
            if own {
                AlertOutcome::Exists
            } else {
                AlertOutcome::BlockedBySimilar
            }
        }
        _ => AlertOutcome::Failed,
    }
}

/// File the alert as a `loom:operator` issue via `create-issue.sh`, the same
/// path the CI billing alert uses. The script's duplicate backstop dedups
/// across daemon restarts and hosts, but a similar alert for a *different* fix
/// can trip it, so a block is accepted only when it names this fix's own alert
/// (exact title); otherwise the alert is filed with `--force`. Returns whether
/// this fix's alert now exists.
fn file_alert(root: &Path, issue: u32, waited: Duration) -> bool {
    let minutes = waited.as_secs() / 60;
    log::error!(
        "work_finder: main of {} has been red with main-red-fix #{issue} unclaimed for \
         {minutes} min; alerting operator (#10118)",
        root.display()
    );
    let Some(script) = crate::watchdog::escalate::resolve_issue_script(Some(root), root, None)
    else {
        log::error!("work_finder: no create-issue.sh to alert with for #{issue} (#10118)");
        return false;
    };
    let body = format!(
        "`main` is verified red and #{issue} (carrying `{MAIN_RED_FIX_MARKER}`) has waited \
         unclaimed for about {minutes} minutes. The work finder admits it ahead of other work \
         while `main` is red, so something is holding it: check its labels (park/skip, \
         host constraint), backoff, and the repo's holds in the ready queue.\n\n\
         Filed once per fix by the red-main-fix lane (#10118); threshold: \
         `autonomous.workFinder.redFixEscalateAfterSecs`."
    );
    let run = |force: bool| {
        let mut cmd = std::process::Command::new(&script);
        cmd.current_dir(root)
            .args(["--title", &alert_title(issue), "--body", &body])
            .args(["--label", "loom:operator"]);
        if force {
            cmd.arg("--force");
        }
        crate::sweep_registry::output_with_timeout(cmd, Duration::from_secs(60))
            .ok()
            .flatten()
            .map_or(AlertOutcome::Failed, |o| {
                classify_alert_run(o.status.code(), &String::from_utf8_lossy(&o.stderr), issue)
            })
    };
    let mut outcome = run(false);
    if outcome == AlertOutcome::BlockedBySimilar {
        log::warn!(
            "work_finder: a similar open issue blocked the red-main-fix alert for #{issue}; \
             filing it anyway (#10118)"
        );
        outcome = run(true);
    }
    let ok = outcome == AlertOutcome::Exists;
    if !ok {
        log::error!("work_finder: filing the red-main-fix alert for #{issue} failed (#10118)");
    }
    ok
}

/// One repo's inputs to the red-main-fix lane, resolved once per tick.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RedMainLane {
    /// The main-health gate has verified this repo's `main` red
    /// ([`WorkspaceHealthStates::is_halted`]).
    pub verified_red: bool,
    /// This repo has no enabled `buildGate`, so the CI fallback decides red.
    pub gate_disabled: bool,
    /// Any hold on this repo other than the verified-red halt: a gate run in
    /// flight, a drain, the host breaker, a pre-flight or token-pool hold.
    pub other_hold: bool,
}

impl RedMainLane {
    /// Whether a halted repo still admits its red-main fixes this tick: only
    /// when the halt is the verified-red one and nothing else holds the repo.
    #[must_use]
    pub fn admits_fixes_while_halted(&self) -> bool {
        self.verified_red && !self.other_hold
    }

    /// Whether this repo's `main` counts as red for key 3. `ci_red` is called
    /// only when the gate is disabled and `ready` has a marker-bearing issue.
    pub fn is_red(&self, ready: &[WorkItem], ci_red: impl FnOnce() -> bool) -> bool {
        self.verified_red
            || (self.gate_disabled && ready.iter().any(WorkItem::is_main_red_fix) && ci_red())
    }
}

/// The lane for one repo, from its own health state.
#[must_use]
pub fn lane_for(
    health: &MainHealthState,
    root: &Path,
    suppress_dispatch_during_gate: bool,
    other_hold: bool,
) -> RedMainLane {
    RedMainLane {
        verified_red: health.is_halted(),
        gate_disabled: crate::main_health_gate::read_build_gate_config(root).is_none(),
        other_hold: other_hold || (suppress_dispatch_during_gate && health.is_gate_in_flight()),
    }
}

/// The lanes for every root, parallel to `roots`. `preflight_held` is the
/// per-root pre-flight / pool hold slice; `global_hold` is the daemon-wide
/// drain / host-breaker hold.
#[must_use]
pub fn lanes_per_root(
    health_states: &WorkspaceHealthStates,
    roots: &[PathBuf],
    suppress_dispatch_during_gate: bool,
    preflight_held: &[bool],
    global_hold: bool,
) -> Vec<RedMainLane> {
    roots
        .iter()
        .enumerate()
        .map(|(i, root)| {
            let held = global_hold || preflight_held.get(i).copied().unwrap_or(false);
            lane_for(&health_states.get_or_create(root), root, suppress_dispatch_during_gate, held)
        })
        .collect()
}

fn ci_cache() -> &'static Mutex<HashMap<PathBuf, (Instant, bool)>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, (Instant, bool)>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Whether `root`'s latest default-branch CI conclusion is a failure — the fallback
/// for a repo with no enabled gate. Cached for [`CI_FALLBACK_TTL`]. Any
/// failure to answer (breaker suppressing, `gh` missing, no runs) is "not
/// red": the fallback can only ever grant the boost on positive evidence.
#[must_use]
pub fn ci_main_red(root: &Path) -> bool {
    let now = Instant::now();
    if let Some((at, red)) = ci_cache().lock().ok().and_then(|c| c.get(root).copied()) {
        if now.saturating_duration_since(at) < CI_FALLBACK_TTL {
            return red;
        }
    }
    let red =
        !crate::rate_limit_breaker::global_skip_pass("work_finder") && probe_ci_main_red(root);
    if let Ok(mut cache) = ci_cache().lock() {
        cache.insert(root.to_path_buf(), (now, red));
    }
    red
}

/// The branch the CI fallback reads: the repo's default branch from
/// `origin/HEAD`, else `main` when that symbolic ref is unset.
pub(super) fn default_branch_for(root: &Path) -> String {
    crate::worktree_ops::clean::default_branch(root).unwrap_or_else(|| "main".to_string())
}

fn probe_ci_main_red(root: &Path) -> bool {
    let branch = default_branch_for(root);
    let probe = crate::gh_invocation::GhInvocation::new(
        crate::gh_invocation::Operation::new("run.list"),
        crate::gh_invocation::AccessIntent::Read,
        crate::gh_invocation::GhTarget::None,
        Duration::from_secs(30),
    )
    .forge_op(crate::forge_call_stats::ops::CI_WORKFLOW_RUNS_FOR_SHA)
    .args(["run", "list", "--branch", &branch, "--limit", "30"])
    .args(["--json", "headSha,status,conclusion,workflowName"])
    .current_dir(root)
    .run();
    let unavailable = |why: &str| {
        log::debug!("work_finder: red-main CI fallback unavailable for {} ({why})", root.display());
        false
    };
    match probe {
        CmdOutcome::Ran(out) if out.status.success() => {
            latest_run_is_failure(&String::from_utf8_lossy(&out.stdout))
        }
        CmdOutcome::Ran(out) => {
            // Feed the rate-limit breaker like every other forge read, so an
            // exhausted pool trips it instead of being retried every minute.
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            crate::rate_limit_breaker::global_observe_failure(&stderr, "work_finder_main_red_ci");
            unavailable(&stderr)
        }
        CmdOutcome::Unavailable(Unavailable::TimedOut { .. }) => unavailable("timed out"),
        CmdOutcome::Unavailable(other) => unavailable(&format!("{other:?}")),
    }
}

/// Whether the newest commit in a `gh run list --json headSha,…` payload
/// (newest first) has a failed CI run: any run for that SHA concluded
/// `failure`, `timed_out` or `startup_failure` — the same conclusions the
/// main-health gate's forge-CI reducer treats as a failure verdict.
#[must_use]
pub fn latest_run_is_failure(stdout: &str) -> bool {
    let Ok(runs) = serde_json::from_str::<Vec<serde_json::Value>>(stdout) else {
        return false;
    };
    let field = |r: &serde_json::Value, k: &str| {
        r.get(k)
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
    };
    let Some(sha) = runs.first().and_then(|r| field(r, "headSha")) else {
        return false;
    };
    runs.iter()
        .filter(|r| field(r, "headSha").as_deref() == Some(sha.as_str()))
        .any(|r| {
            matches!(
                field(r, "conclusion").as_deref(),
                Some("failure" | "timed_out" | "startup_failure")
            )
        })
}

#[cfg(test)]
mod alert_sink_tests {
    use super::*;

    const SIMILAR_OTHER: &str = "create-issue.sh: NOT FILED -- this looks like a duplicate of open work:\n  #800: Red main: main-red-fix #7 is still unclaimed (similarity: 90%)\nNothing was created.\n";

    #[test]
    fn exit_3_is_this_fixs_alert_only_on_an_exact_title_match() {
        let own = "  #800: Red main: main-red-fix #9 is still unclaimed (similarity: 100%)\n";
        assert_eq!(classify_alert_run(Some(3), own, 9), AlertOutcome::Exists);
        assert_eq!(
            classify_alert_run(Some(3), SIMILAR_OTHER, 9),
            AlertOutcome::BlockedBySimilar,
            "another fix's alert must not stand in for this one"
        );
        // #9 must not match #90's alert by prefix.
        let longer = "  #801: Red main: main-red-fix #90 is still unclaimed (similarity: 95%)\n";
        assert_eq!(classify_alert_run(Some(3), longer, 9), AlertOutcome::BlockedBySimilar);
        assert_eq!(classify_alert_run(Some(0), "", 9), AlertOutcome::Exists);
        assert_eq!(classify_alert_run(Some(1), "", 9), AlertOutcome::Failed);
        assert_eq!(classify_alert_run(None, "", 9), AlertOutcome::Failed);
    }

    /// Install a stub `create-issue.sh` that records its argv and answers with
    /// `stderr`/`code` unless `--force` is passed (then it "creates").
    fn root_with_stub(stderr: &str, code: i32) -> (tempfile::TempDir, PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let scripts = dir.path().join(".loom/scripts");
        std::fs::create_dir_all(&scripts).unwrap();
        let log = dir.path().join("calls.log");
        let script = scripts.join("create-issue.sh");
        std::fs::write(
            &script,
            format!(
                "#!/bin/bash\nfor a in \"$@\"; do [ \"$a\" = --force ] && f=force; done\necho \"call ${{f:-plain}}\" >> '{}'\nfor a in \"$@\"; do [ \"$a\" = --force ] && exit 0; done\nprintf '%s' '{stderr}' >&2\nexit {code}\n",
                log.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        (dir, log)
    }

    #[test]
    fn a_similar_alert_for_another_fix_does_not_suppress_this_fixs_alert() {
        let (dir, log) = root_with_stub(SIMILAR_OTHER, 3);
        assert!(file_alert(dir.path(), 9, Duration::from_secs(1800)));
        let calls = std::fs::read_to_string(log).unwrap();
        assert_eq!(calls.lines().count(), 2, "blocked, then filed with --force: {calls}");
        assert_eq!(calls.lines().nth(1), Some("call force"));
    }

    #[test]
    fn this_fixs_own_existing_alert_is_accepted_without_refiling() {
        let own = "  #800: Red main: main-red-fix #9 is still unclaimed (similarity: 100%)\n";
        let (dir, log) = root_with_stub(own, 3);
        assert!(file_alert(dir.path(), 9, Duration::from_secs(1800)));
        let calls = std::fs::read_to_string(log).unwrap();
        assert_eq!(calls.lines().count(), 1, "no --force re-run: {calls}");
    }

    #[test]
    fn a_script_failure_is_reported_as_not_filed() {
        let (dir, _log) = root_with_stub("boom", 1);
        assert!(!file_alert(dir.path(), 9, Duration::from_secs(1800)));
    }
}
