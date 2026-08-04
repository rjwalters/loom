//! Cross-host collision **detection** for role-runner ticks (Issue #4623) —
//! the role-runner counterpart of sweep dispatch's `autonomous.collisionDetection`
//! (Issue #4085, Phase 0 of #4028).
//!
//! # The gap this closes
//!
//! [`crate::role_runner`] has two overlap guards, and neither is cross-host:
//!
//! * **In-process** — [`crate::role_runner::RoleRunGuard`] /
//!   [`crate::role_runner::InProgressGuard`] (an `Arc<Mutex<HashSet<(root,
//!   role)>>>`) stops a given `(repo root, role)` pair running twice
//!   concurrently *inside one `loom-daemon` process* (the interval loop and the
//!   `onIdle` edge share it).
//! * **Host-local** — `loom-daemon-start.sh` refuses a second start while
//!   `.loom/.daemon.pid` points at a live process.
//!
//! Two independent daemons (two hosts, or two clones on one host each with
//! their own PID file) pointed at the **same forge repo** with
//! `autonomous.roleRunner.enabled=true` therefore run Champion/Curator/Judge/
//! Doctor/Auditor/Guide ticks with *zero* mutual awareness: each in-memory
//! guard only knows its own invocations, and the forge — the only shared
//! state — was never consulted. That is the leading explanation for #4586 (eight duplicate
//! "Cannot Auto-Merge" Champion comments on PR #4540 inside ~5 minutes, far
//! above the documented 10-minute Champion cadence). The same shape applies to
//! a repo that leaves the GitHub Actions `loom-*.yml` cron schedules enabled
//! *alongside* the daemon role runner.
//!
//! # What this module does (and deliberately does not do)
//!
//! **Detection only, opt-in, never acts.** Exactly like #4085: a detected
//! collision is counted and logged at `WARN`; the role tick then runs
//! completely unchanged. Nothing here suppresses, delays, or reorders a role
//! invocation — establishing a trustworthy baseline comes before any
//! enforcement (the enforcement tier is #4028 Phase 2, a real cross-host CAS,
//! out of scope here).
//!
//! # The heuristic: foreign activity on the role's own work queue
//!
//! Each standalone role acts on a **label-defined queue** (the lifecycle in
//! CLAUDE.md): Champion works open PRs labeled `loom:pr`, Judge works
//! `loom:review-requested`, Doctor works `loom:changes-requested` (#5272),
//! Curator works `loom:curating`, Guide works `loom:triage`, Auditor works
//! its own `loom:auditor` proposals. Any pass of that role — comment, label
//! write, close — bumps `updated_at` on the queue items it touches.
//!
//! So, immediately before a tick for `(root, role)`:
//!
//! 1. List that role's queue over the **ETag-cached REST** listing
//!    ([`crate::forge_listing`]) — a poll where nothing changed is a `304` at
//!    *zero* rate-limit cost, which is what makes a per-tick probe affordable
//!    (and keeps it off the GraphQL budget that #4429 exhausted).
//! 2. Take the newest `updated_at` across the queue.
//! 3. Compare it against the wall-clock window of **this process's own last
//!    completed tick** for that `(root, role)`. Queue activity strictly after
//!    our own last pass finished (plus a clock-skew margin) was not written by
//!    us — it is evidence of a *foreign* pass for this role.
//!
//! [`classify_queue_activity`] is a pure function over that data, so the whole
//! decision is unit-testable with no `gh`, no network, and no clock.
//!
//! ## Known false positives / negatives (read before trusting the count)
//!
//! This is a heuristic baseline, not proof, and it is deliberately biased to
//! **under**-count rather than over-count:
//!
//! * **Under-counts.** A peer pass whose writes land *inside* our own last
//!   tick's window is invisible. So is a pass that changes nothing on the queue
//!   (a Champion tick that finds nothing mergeable writes nothing). The first
//!   probe after daemon start has no self-run baseline at all and resolves to
//!   [`RoleCollisionClass::Unknown`] — fail-closed, never counted.
//! * **Over-counts (bounded).** A human, a Builder, or a *sweep-internal*
//!   Judge/Doctor/Champion touching the same queue also bumps `updated_at`, and
//!   is indistinguishable here from a peer host's role runner. The `WARN` line
//!   says so explicitly. Since nothing acts on the signal, a false positive
//!   costs one log line.
//!
//! # Config surface (and why it is layered on #4085's)
//!
//! The role runner **honors `autonomous.collisionDetection.enabled` /
//! `LOOM_DETECT_COLLISIONS` directly** — it is the same problem class and one
//! operator switch should cover "tell me about cross-host duplication" for both
//! dispatch shapes. But it also accepts a role-runner-specific override,
//! because the two invocation shapes have genuinely different probe costs and
//! cadences (one `gh issue view` *per dispatch* vs. one cached REST listing
//! *per role per tick*), so an operator must be able to run one without the
//! other. Precedence, highest first:
//!
//! 1. `LOOM_ROLE_RUNNER_DETECT_COLLISIONS` (env)
//! 2. `autonomous.roleRunner.collisionDetection` (config)
//! 3. `LOOM_DETECT_COLLISIONS` / `autonomous.collisionDetection.enabled`
//!    (#4085's shared toggle)
//! 4. default **off**

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::Duration;

use chrono::{DateTime, Utc};

// ============================================================================
// Constants
// ============================================================================

/// Env var toggling role-runner collision detection on its own, independent of
/// #4085's sweep-dispatch toggle. `1`/`true`/`yes`/`on` (case-insensitive)
/// enable; any other value disables (an explicit `0` therefore turns the role
/// runner's probe OFF even when the shared `LOOM_DETECT_COLLISIONS` is on).
pub const ROLE_COLLISION_DETECT_ENV: &str = "LOOM_ROLE_RUNNER_DETECT_COLLISIONS";

/// Env var overriding the lookback window (seconds) for the queue-activity
/// probe. Unset falls through to `autonomous.roleRunner.collisionWindowSecs`
/// and then to the role's own tick interval (see [`resolve_window`]).
pub const ROLE_COLLISION_WINDOW_ENV: &str = "LOOM_ROLE_RUNNER_COLLISION_WINDOW_SECS";

/// Clock-skew margin applied to **both** ends of this process's own last-run
/// window before treating queue activity as foreign. The forge stamps
/// `updated_at` with *its* clock while the run window is stamped with *ours*;
/// without a margin a few seconds of ordinary NTP drift would attribute our own
/// writes to a phantom peer. Deliberately a constant, not a knob: it exists to
/// absorb drift, not to tune sensitivity (that is what the window is for).
pub const CLOCK_SKEW_MARGIN: Duration = Duration::from_secs(90);

/// Floor for the resolved lookback window. A window shorter than this cannot
/// meaningfully overlap a real role pass (a `claude -p "/loom:champion"`
/// session runs for minutes), so it would report `Clean` unconditionally.
pub const MIN_WINDOW: Duration = Duration::from_secs(60);

/// Ceiling for the resolved lookback window. Beyond this the probe stops
/// describing "concurrent" activity and starts reporting ordinary backlog
/// churn, which would swamp the baseline with false positives.
pub const MAX_WINDOW: Duration = Duration::from_secs(3600);

// ============================================================================
// Per-role probe target
// ============================================================================

/// Whether a role's queue is made of pull requests or issues. REST issue
/// listings return **both** (a PR row carries a `pull_request` key —
/// [`crate::forge_listing::RestIssue::is_pull_request`]), so the probe filters
/// on this to avoid attributing, say, an issue's churn to Champion's PR pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetKind {
    /// Only rows that are pull requests count.
    PullRequest,
    /// Only rows that are plain issues count.
    Issue,
}

/// The forge-visible work queue a role's pass would leave fingerprints on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProbeTarget {
    /// The label defining the queue (`.github/labels.yml` / the CLAUDE.md
    /// lifecycle diagrams).
    pub label: &'static str,
    /// Whether the queue is PRs or issues.
    pub kind: TargetKind,
}

/// The queue to probe for `role`, or `None` for a role with no single
/// label-defined queue (detection is then a documented no-op for it).
///
/// The mappings mirror the label lifecycle in CLAUDE.md:
/// * `champion` → open PRs labeled `loom:pr` (the auto-merge queue — where
///   #4586's duplicate "Cannot Auto-Merge" burst landed)
/// * `judge` → open PRs labeled `loom:review-requested`
/// * `doctor` → open PRs labeled `loom:changes-requested` (#5272's standalone
///   queue scan — the counterpart of `judge`'s probe target, one stage later
///   in the PR lifecycle)
/// * `curator` → open issues labeled `loom:curating` (the in-flight marker a
///   Curator pass writes before it enriches)
/// * `guide` → open issues labeled `loom:triage`
/// * `auditor` → open issues labeled `loom:auditor` (its own proposals)
#[must_use]
pub fn probe_target_for_role(role: &str) -> Option<ProbeTarget> {
    let target = match role {
        "champion" => ProbeTarget {
            label: "loom:pr",
            kind: TargetKind::PullRequest,
        },
        "judge" => ProbeTarget {
            label: "loom:review-requested",
            kind: TargetKind::PullRequest,
        },
        "doctor" => ProbeTarget {
            label: "loom:changes-requested",
            kind: TargetKind::PullRequest,
        },
        "curator" => ProbeTarget {
            label: "loom:curating",
            kind: TargetKind::Issue,
        },
        "guide" => ProbeTarget {
            label: "loom:triage",
            kind: TargetKind::Issue,
        },
        "auditor" => ProbeTarget {
            label: "loom:auditor",
            kind: TargetKind::Issue,
        },
        _ => return None,
    };
    Some(target)
}

// ============================================================================
// Pure classification
// ============================================================================

/// One row of a role's queue, reduced to what the classifier needs. Kept
/// separate from [`crate::forge_listing::RestIssue`] so the pure decision logic
/// has no dependency on the listing layer (and tests need no REST payloads).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueItem {
    /// Issue / PR number, for the diagnostic record.
    pub number: u32,
    /// Parsed `updated_at`. `None` (absent or unparseable) is ignored rather
    /// than guessed at — fail-closed, consistent with the rest of the module.
    pub updated_at: Option<DateTime<Utc>>,
    /// Whether this row is a pull request.
    pub is_pull_request: bool,
}

/// The wall-clock window of one role invocation this process ran itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelfRunWindow {
    /// When this process started the invocation.
    pub started: DateTime<Utc>,
    /// When it finished. `None` means still running (only possible for a
    /// concurrent idle-path run — the interval path probes before it starts).
    pub ended: Option<DateTime<Utc>>,
}

/// What the pre-tick probe concluded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoleCollisionClass {
    /// No queue activity newer than this process's own last pass.
    Clean,
    /// Not determinable — no self-run baseline yet, or the listing failed.
    /// **Never** counted as a collision (fail-closed), so an unverifiable probe
    /// can never inflate the baseline.
    Unknown(&'static str),
    /// Queue activity strictly newer than this process's own last pass: some
    /// other pass for this role touched the queue.
    Collision {
        /// The most recently touched queue item.
        number: u32,
        /// Its `updated_at`.
        updated_at: DateTime<Utc>,
        /// How many queue items were touched after our own last pass.
        touched: usize,
    },
}

/// Classify a role's queue for foreign activity — the whole decision, pure.
///
/// `Collision` iff some queue row of the target kind has `updated_at` strictly
/// after **both**:
/// * `now - window` (the lookback bound — older churn is not "concurrent"), and
/// * `last_self_run.ended + CLOCK_SKEW_MARGIN` (everything our own last pass
///   could plausibly have written).
///
/// Taking the max of those two bounds is what makes a single stored self-run
/// window sufficient: a window longer than the tick interval can span several
/// of our own past passes, but only the latest one matters, since anything
/// older is by construction older than the latest pass's end.
///
/// `last_self_run == None` ⇒ [`RoleCollisionClass::Unknown`]: with no baseline,
/// recent activity is as likely to be this daemon's own pre-restart work as a
/// peer's.
#[must_use]
pub fn classify_queue_activity(
    items: &[QueueItem],
    now: DateTime<Utc>,
    window: Duration,
    kind: TargetKind,
    last_self_run: Option<SelfRunWindow>,
) -> RoleCollisionClass {
    let Some(self_run) = last_self_run else {
        return RoleCollisionClass::Unknown(
            "no self-run baseline yet (first tick for this (root, role) since daemon start)",
        );
    };
    let want_pr = matches!(kind, TargetKind::PullRequest);
    let lookback = match chrono::Duration::from_std(window) {
        Ok(d) => now - d,
        Err(_) => return RoleCollisionClass::Unknown("lookback window out of range"),
    };
    let skew = chrono::Duration::from_std(CLOCK_SKEW_MARGIN).unwrap_or_else(|_| {
        // CLOCK_SKEW_MARGIN is a small compile-time constant; this is
        // unreachable in practice and only exists to keep the fn total.
        chrono::Duration::seconds(90)
    });
    let self_end = self_run.ended.unwrap_or(now) + skew;
    let cutoff = if self_end > lookback {
        self_end
    } else {
        lookback
    };

    let mut touched = 0usize;
    let mut newest: Option<(u32, DateTime<Utc>)> = None;
    for item in items {
        if item.is_pull_request != want_pr {
            continue;
        }
        let Some(updated) = item.updated_at else {
            continue;
        };
        if updated <= cutoff {
            continue;
        }
        touched += 1;
        if newest.is_none_or(|(_, best)| updated > best) {
            newest = Some((item.number, updated));
        }
    }
    match newest {
        Some((number, updated_at)) => RoleCollisionClass::Collision {
            number,
            updated_at,
            touched,
        },
        None => RoleCollisionClass::Clean,
    }
}

// ============================================================================
// Config resolution
// ============================================================================

/// Resolve whether the role runner's collision probe runs for `repo_root`.
///
/// Precedence (see the module docs for the rationale):
/// `LOOM_ROLE_RUNNER_DETECT_COLLISIONS` > `autonomous.roleRunner.collisionDetection`
/// > #4085's shared `LOOM_DETECT_COLLISIONS` / `autonomous.collisionDetection.enabled`
/// > `false`.
#[must_use]
pub fn resolve_detection_enabled(repo_root: &Path) -> bool {
    if let Ok(v) = std::env::var(ROLE_COLLISION_DETECT_ENV) {
        return is_truthy(&v);
    }
    let effective = crate::config_resolver::resolve_effective_config(repo_root);
    if let Some(explicit) = crate::config_resolver::get_path(&effective, "autonomous.roleRunner")
        .and_then(|r| r.get("collisionDetection"))
        .and_then(serde_json::Value::as_bool)
    {
        return explicit;
    }
    // Fall through to #4085's shared toggle (env > config > false).
    crate::sweep_registry::resolve_collision_detection(repo_root)
}

/// Resolve the probe's lookback window, precedence highest first:
/// `LOOM_ROLE_RUNNER_COLLISION_WINDOW_SECS` (env), then
/// `autonomous.roleRunner.collisionWindowSecs` (config), then `role_interval` —
/// clamped to `[MIN_WINDOW, MAX_WINDOW]`.
///
/// Defaulting to the role's own cadence is the natural choice: a peer running
/// the same role on the same cadence writes to the queue at least once per
/// interval, so one interval of lookback is exactly enough to see it without
/// reaching back into unrelated backlog churn.
#[must_use]
pub fn resolve_window(repo_root: &Path, role_interval: Duration) -> Duration {
    let from_env = std::env::var(ROLE_COLLISION_WINDOW_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&s| s > 0);
    let secs = match from_env {
        Some(s) => Some(s),
        None => {
            let effective = crate::config_resolver::resolve_effective_config(repo_root);
            crate::config_resolver::get_path(&effective, "autonomous.roleRunner")
                .and_then(|r| r.get("collisionWindowSecs"))
                .and_then(serde_json::Value::as_u64)
                .filter(|&s| s > 0)
        }
    };
    let window = secs.map_or(role_interval, Duration::from_secs);
    window.clamp(MIN_WINDOW, MAX_WINDOW)
}

fn is_truthy(v: &str) -> bool {
    matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
}

// ============================================================================
// Process-global tracker
// ============================================================================

/// Per-`(root, role)` self-run baselines plus the running collision count.
///
/// Held in a process-global (see [`tracker`]) rather than threaded through the
/// role-runner APIs because the two paths that must share it — the per-role
/// multi-workspace interval loop and the work-finder-driven `onIdle` edge —
/// are wired from different call sites; the same reasoning already makes
/// `role_runner`'s `ROLE_RUN_START_GENERATION` a static. The type itself has no
/// global state, so tests construct their own instance.
#[derive(Debug, Default)]
pub struct RoleCollisionTracker {
    last_runs: HashMap<(PathBuf, &'static str), SelfRunWindow>,
    collisions: u64,
}

impl RoleCollisionTracker {
    /// Construct an empty tracker.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that this process started a `(root, role)` invocation at
    /// `started`, clearing the previous window's `ended` stamp.
    pub fn record_run_started(&mut self, root: &Path, role: &'static str, started: DateTime<Utc>) {
        self.last_runs.insert(
            (root.to_path_buf(), role),
            SelfRunWindow {
                started,
                ended: None,
            },
        );
    }

    /// Record that this process finished a `(root, role)` invocation at
    /// `ended`. A finish with no recorded start (possible only if the tracker
    /// was cleared mid-run) seeds the window with `started == ended` so the
    /// next probe still has a usable baseline.
    pub fn record_run_finished(&mut self, root: &Path, role: &'static str, ended: DateTime<Utc>) {
        let key = (root.to_path_buf(), role);
        let entry = self.last_runs.entry(key).or_insert(SelfRunWindow {
            started: ended,
            ended: None,
        });
        entry.ended = Some(ended);
    }

    /// This process's last recorded invocation window for `(root, role)`.
    #[must_use]
    pub fn last_run(&self, root: &Path, role: &str) -> Option<SelfRunWindow> {
        self.last_runs
            .iter()
            .find(|((r, n), _)| r == root && *n == role)
            .map(|(_, w)| *w)
    }

    /// Count one detected collision and return the new running total.
    pub fn note_collision(&mut self) -> u64 {
        self.collisions += 1;
        self.collisions
    }

    /// Running total of collisions detected this process's lifetime.
    #[must_use]
    pub fn collisions(&self) -> u64 {
        self.collisions
    }
}

fn tracker() -> &'static Mutex<RoleCollisionTracker> {
    static TRACKER: OnceLock<Mutex<RoleCollisionTracker>> = OnceLock::new();
    TRACKER.get_or_init(|| Mutex::new(RoleCollisionTracker::new()))
}

/// Running total of role-runner collisions detected in this process (#4623).
/// Always `0` when detection is disabled everywhere.
#[must_use]
pub fn collision_count() -> u64 {
    tracker()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .collisions()
}

/// Record the start of a role invocation this process is running.
pub fn record_run_started(root: &Path, role: &'static str, started: DateTime<Utc>) {
    tracker()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .record_run_started(root, role, started);
}

/// Record the end of a role invocation this process ran — the baseline the
/// *next* probe for that `(root, role)` attributes queue activity against.
pub fn record_run_finished(root: &Path, role: &'static str, ended: DateTime<Utc>) {
    tracker()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .record_run_finished(root, role, ended);
}

/// This process's last recorded invocation window for `(root, role)`, if any.
/// Exposed so the role-runner wiring is testable without reaching into the
/// tracker's internals; also the natural hook for a future status surface.
#[must_use]
pub fn last_self_run(root: &Path, role: &str) -> Option<SelfRunWindow> {
    tracker()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .last_run(root, role)
}

// ============================================================================
// The probe (I/O) + logging
// ============================================================================

/// Fetch a role's queue. Injected so [`probe_before_tick`] is exercisable
/// without a forge (the pure classifier is tested directly; this trait keeps
/// the wiring around it testable too).
pub trait QueueSource {
    /// List open items carrying `label` in `root`'s repo. An `Err` is
    /// fail-closed: the probe resolves to [`RoleCollisionClass::Unknown`].
    fn list_queue(&mut self, root: &Path, label: &str) -> Result<Vec<QueueItem>, String>;
}

/// The production [`QueueSource`]: the ETag-cached REST issue listing
/// ([`crate::forge_listing::list_issues_cached`]), so an unchanged queue costs
/// a free `304` instead of rate-limit budget.
pub struct RestQueueSource {
    gh_bin: PathBuf,
}

impl RestQueueSource {
    /// Construct one using `gh` from `PATH` (or `LOOM_GH_BIN` when set, the
    /// same override the rest of the daemon honors).
    #[must_use]
    pub fn new() -> Self {
        let gh_bin = std::env::var("LOOM_GH_BIN")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .map_or_else(|| PathBuf::from("gh"), PathBuf::from);
        Self { gh_bin }
    }
}

impl QueueSource for RestQueueSource {
    fn list_queue(&mut self, root: &Path, label: &str) -> Result<Vec<QueueItem>, String> {
        let rows =
            crate::forge_listing::list_issues_cached(&self.gh_bin, Some(root), None, label, "open")
                .map_err(|e| e.to_string())?;
        Ok(rows
            .into_iter()
            .map(|r| QueueItem {
                number: r.number,
                updated_at: r
                    .updated_at
                    .as_deref()
                    .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                    .map(|d| d.with_timezone(&Utc)),
                is_pull_request: r.is_pull_request,
            })
            .collect())
    }
}

/// Run the pre-tick probe for `(root, role)` and record/log a collision.
///
/// A **no-op that makes no forge call at all** when detection is disabled for
/// `root` (so the disabled path is byte-for-byte the pre-#4623 path), and for a
/// role with no label-defined queue. Never returns a decision: the caller
/// always proceeds with its tick — detection only.
pub fn probe_before_tick(root: &Path, role: &'static str, interval: Duration) {
    if !resolve_detection_enabled(root) {
        return;
    }
    let mut source = RestQueueSource::new();
    probe_before_tick_with(&mut source, root, role, interval, Utc::now());
}

/// [`probe_before_tick`] with the queue source and clock injected, and the
/// enabled-gate already applied by the caller. Returns the classification so
/// tests can assert on it; production ignores the value (detection only).
pub fn probe_before_tick_with<S: QueueSource>(
    source: &mut S,
    root: &Path,
    role: &'static str,
    interval: Duration,
    now: DateTime<Utc>,
) -> RoleCollisionClass {
    let Some(target) = probe_target_for_role(role) else {
        log::debug!(
            "role_collision: no label-defined queue for role {role} — collision detection is a \
             no-op for it (#4623)"
        );
        return RoleCollisionClass::Unknown("role has no label-defined queue");
    };
    let window = resolve_window(root, interval);
    let last_self_run = tracker()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .last_run(root, role);
    let items = match source.list_queue(root, target.label) {
        Ok(items) => items,
        Err(e) => {
            // Fail-closed: an unverifiable read is never a collision.
            log::debug!(
                "role_collision: queue probe for {role} in {} inconclusive ({e}) — fail-closed, \
                 not counted (#4623)",
                root.display()
            );
            return RoleCollisionClass::Unknown("queue listing failed");
        }
    };
    let class = classify_queue_activity(&items, now, window, target.kind, last_self_run);
    log_class(root, role, target, window, &class);
    class
}

fn log_class(
    root: &Path,
    role: &'static str,
    target: ProbeTarget,
    window: Duration,
    class: &RoleCollisionClass,
) {
    match class {
        RoleCollisionClass::Collision {
            number,
            updated_at,
            touched,
        } => {
            let count = tracker()
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .note_collision();
            log::warn!(
                "role_collision: cross-invocation role collision (#4623) — {role} queue \
                 ({label}) in {repo} shows {touched} item(s) touched after host {host}'s own \
                 last {role} pass finished; newest is #{number} at {updated_at} (lookback \
                 {window}s). Another {role} pass — a peer host's role runner, a second daemon \
                 on this host, or an enabled GitHub Actions cron — most likely ran concurrently. \
                 A same-host sweep-internal role or a human touching the same queue looks \
                 identical here, so treat this as a baseline signal, not proof. Running \
                 role-collision count={count} (detection only — the tick proceeds unchanged).",
                label = target.label,
                repo = root.display(),
                host = crate::sweep_registry::host_identity(),
                window = window.as_secs(),
            );
        }
        RoleCollisionClass::Unknown(reason) => {
            log::debug!(
                "role_collision: {role} probe for {} inconclusive ({reason}) — fail-closed, not \
                 counted (#4623)",
                root.display()
            );
        }
        RoleCollisionClass::Clean => {
            log::debug!(
                "role_collision: {role} queue ({}) in {} clean since this host's last pass \
                 (#4623)",
                target.label,
                root.display()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    fn ts(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn pr(number: u32, updated: &str) -> QueueItem {
        QueueItem {
            number,
            updated_at: Some(ts(updated)),
            is_pull_request: true,
        }
    }

    fn issue(number: u32, updated: &str) -> QueueItem {
        QueueItem {
            number,
            updated_at: Some(ts(updated)),
            is_pull_request: false,
        }
    }

    fn self_run(started: &str, ended: &str) -> SelfRunWindow {
        SelfRunWindow {
            started: ts(started),
            ended: Some(ts(ended)),
        }
    }

    // ===================================================================
    // probe_target_for_role
    // ===================================================================

    #[test]
    fn every_default_role_has_a_probe_queue() {
        // Detection must not silently no-op for a shipped standalone role.
        for spec in crate::role_runner::DEFAULT_ROLES {
            assert!(
                probe_target_for_role(spec.name).is_some(),
                "no probe target for shipped role {}",
                spec.name
            );
        }
    }

    #[test]
    fn probe_targets_match_the_documented_lifecycle_labels() {
        assert_eq!(
            probe_target_for_role("champion"),
            Some(ProbeTarget {
                label: "loom:pr",
                kind: TargetKind::PullRequest
            })
        );
        assert_eq!(
            probe_target_for_role("judge"),
            Some(ProbeTarget {
                label: "loom:review-requested",
                kind: TargetKind::PullRequest
            })
        );
        assert_eq!(
            probe_target_for_role("doctor"),
            Some(ProbeTarget {
                label: "loom:changes-requested",
                kind: TargetKind::PullRequest
            }),
            "#5272: doctor's standalone queue scan needs a probe target too"
        );
        assert_eq!(
            probe_target_for_role("curator"),
            Some(ProbeTarget {
                label: "loom:curating",
                kind: TargetKind::Issue
            })
        );
        assert_eq!(
            probe_target_for_role("guide"),
            Some(ProbeTarget {
                label: "loom:triage",
                kind: TargetKind::Issue
            })
        );
        assert_eq!(
            probe_target_for_role("auditor"),
            Some(ProbeTarget {
                label: "loom:auditor",
                kind: TargetKind::Issue
            })
        );
    }

    #[test]
    fn unknown_role_has_no_probe_queue() {
        assert_eq!(probe_target_for_role("builder"), None);
        assert_eq!(probe_target_for_role(""), None);
    }

    // ===================================================================
    // classify_queue_activity
    // ===================================================================

    #[test]
    fn no_self_run_baseline_is_unknown_not_a_collision() {
        // Fail-closed: the first probe after daemon start cannot attribute
        // recent activity, so it is never counted.
        let items = vec![pr(1, "2026-07-30T12:00:00Z")];
        let class = classify_queue_activity(
            &items,
            ts("2026-07-30T12:01:00Z"),
            Duration::from_secs(600),
            TargetKind::PullRequest,
            None,
        );
        assert!(matches!(class, RoleCollisionClass::Unknown(_)), "got {class:?}");
    }

    #[test]
    fn activity_after_our_own_pass_is_a_collision() {
        let items = vec![pr(4540, "2026-07-30T12:05:00Z")];
        let class = classify_queue_activity(
            &items,
            ts("2026-07-30T12:06:00Z"),
            Duration::from_secs(600),
            TargetKind::PullRequest,
            // Our pass ended at 12:00; skew margin pushes the cutoff to 12:01:30.
            Some(self_run("2026-07-30T11:58:00Z", "2026-07-30T12:00:00Z")),
        );
        assert_eq!(
            class,
            RoleCollisionClass::Collision {
                number: 4540,
                updated_at: ts("2026-07-30T12:05:00Z"),
                touched: 1,
            }
        );
    }

    #[test]
    fn activity_during_our_own_pass_is_clean() {
        // Our own writes must never be attributed to a peer.
        let items = vec![pr(4540, "2026-07-30T11:59:00Z")];
        let class = classify_queue_activity(
            &items,
            ts("2026-07-30T12:06:00Z"),
            Duration::from_secs(600),
            TargetKind::PullRequest,
            Some(self_run("2026-07-30T11:58:00Z", "2026-07-30T12:00:00Z")),
        );
        assert_eq!(class, RoleCollisionClass::Clean);
    }

    #[test]
    fn activity_within_the_clock_skew_margin_is_clean() {
        // 60s after our pass "ended" — inside the 90s skew margin, so it is
        // still plausibly our own write with a drifting forge clock.
        let items = vec![pr(4540, "2026-07-30T12:01:00Z")];
        let class = classify_queue_activity(
            &items,
            ts("2026-07-30T12:06:00Z"),
            Duration::from_secs(600),
            TargetKind::PullRequest,
            Some(self_run("2026-07-30T11:58:00Z", "2026-07-30T12:00:00Z")),
        );
        assert_eq!(class, RoleCollisionClass::Clean);
    }

    #[test]
    fn activity_older_than_the_lookback_window_is_clean() {
        // Our last pass is ancient (role was disabled for a day), so the
        // lookback bound — not the self-run bound — decides.
        let items = vec![pr(4540, "2026-07-29T12:00:00Z")];
        let class = classify_queue_activity(
            &items,
            ts("2026-07-30T12:00:00Z"),
            Duration::from_secs(600),
            TargetKind::PullRequest,
            Some(self_run("2026-07-28T00:00:00Z", "2026-07-28T00:05:00Z")),
        );
        assert_eq!(class, RoleCollisionClass::Clean);
    }

    #[test]
    fn a_still_running_self_pass_suppresses_everything() {
        // `ended: None` (an idle-path run still in flight) makes the cutoff
        // `now + skew`, so nothing can be attributed to a peer — under-count,
        // never over-count.
        let items = vec![pr(4540, "2026-07-30T12:05:59Z")];
        let class = classify_queue_activity(
            &items,
            ts("2026-07-30T12:06:00Z"),
            Duration::from_secs(600),
            TargetKind::PullRequest,
            Some(SelfRunWindow {
                started: ts("2026-07-30T12:00:00Z"),
                ended: None,
            }),
        );
        assert_eq!(class, RoleCollisionClass::Clean);
    }

    #[test]
    fn wrong_target_kind_rows_are_ignored() {
        // A REST issue listing returns PRs too; an issue's churn must not be
        // attributed to Champion's PR queue (and vice versa).
        let items = vec![issue(99, "2026-07-30T12:05:00Z")];
        let class = classify_queue_activity(
            &items,
            ts("2026-07-30T12:06:00Z"),
            Duration::from_secs(600),
            TargetKind::PullRequest,
            Some(self_run("2026-07-30T11:58:00Z", "2026-07-30T12:00:00Z")),
        );
        assert_eq!(class, RoleCollisionClass::Clean);

        let class = classify_queue_activity(
            &items,
            ts("2026-07-30T12:06:00Z"),
            Duration::from_secs(600),
            TargetKind::Issue,
            Some(self_run("2026-07-30T11:58:00Z", "2026-07-30T12:00:00Z")),
        );
        assert!(matches!(class, RoleCollisionClass::Collision { number: 99, .. }));
    }

    #[test]
    fn missing_updated_at_is_ignored() {
        let items = vec![QueueItem {
            number: 7,
            updated_at: None,
            is_pull_request: true,
        }];
        let class = classify_queue_activity(
            &items,
            ts("2026-07-30T12:06:00Z"),
            Duration::from_secs(600),
            TargetKind::PullRequest,
            Some(self_run("2026-07-30T11:58:00Z", "2026-07-30T12:00:00Z")),
        );
        assert_eq!(class, RoleCollisionClass::Clean);
    }

    #[test]
    fn newest_item_wins_and_touched_counts_all() {
        let items = vec![
            pr(1, "2026-07-30T12:03:00Z"),
            pr(2, "2026-07-30T12:05:00Z"),
            pr(3, "2026-07-30T12:04:00Z"),
            pr(4, "2026-07-30T11:00:00Z"), // before our pass — not counted
        ];
        let class = classify_queue_activity(
            &items,
            ts("2026-07-30T12:06:00Z"),
            Duration::from_secs(600),
            TargetKind::PullRequest,
            Some(self_run("2026-07-30T11:58:00Z", "2026-07-30T12:00:00Z")),
        );
        assert_eq!(
            class,
            RoleCollisionClass::Collision {
                number: 2,
                updated_at: ts("2026-07-30T12:05:00Z"),
                touched: 3,
            }
        );
    }

    #[test]
    fn empty_queue_is_clean() {
        let class = classify_queue_activity(
            &[],
            ts("2026-07-30T12:06:00Z"),
            Duration::from_secs(600),
            TargetKind::PullRequest,
            Some(self_run("2026-07-30T11:58:00Z", "2026-07-30T12:00:00Z")),
        );
        assert_eq!(class, RoleCollisionClass::Clean);
    }

    // ===================================================================
    // Tracker
    // ===================================================================

    #[test]
    fn tracker_records_and_closes_a_self_run_window() {
        let mut t = RoleCollisionTracker::new();
        let root = PathBuf::from("/repo");
        assert_eq!(t.last_run(&root, "champion"), None);
        t.record_run_started(&root, "champion", ts("2026-07-30T12:00:00Z"));
        assert_eq!(
            t.last_run(&root, "champion"),
            Some(SelfRunWindow {
                started: ts("2026-07-30T12:00:00Z"),
                ended: None
            })
        );
        t.record_run_finished(&root, "champion", ts("2026-07-30T12:04:00Z"));
        assert_eq!(
            t.last_run(&root, "champion"),
            Some(self_run("2026-07-30T12:00:00Z", "2026-07-30T12:04:00Z"))
        );
    }

    #[test]
    fn tracker_keys_on_root_and_role_independently() {
        let mut t = RoleCollisionTracker::new();
        let a = PathBuf::from("/repo-a");
        let b = PathBuf::from("/repo-b");
        t.record_run_finished(&a, "champion", ts("2026-07-30T12:00:00Z"));
        assert!(t.last_run(&a, "champion").is_some());
        assert!(t.last_run(&a, "judge").is_none(), "role is part of the key");
        assert!(t.last_run(&b, "champion").is_none(), "root is part of the key");
    }

    #[test]
    fn tracker_finish_without_start_still_seeds_a_baseline() {
        let mut t = RoleCollisionTracker::new();
        let root = PathBuf::from("/repo");
        t.record_run_finished(&root, "guide", ts("2026-07-30T12:00:00Z"));
        assert_eq!(
            t.last_run(&root, "guide"),
            Some(self_run("2026-07-30T12:00:00Z", "2026-07-30T12:00:00Z"))
        );
    }

    #[test]
    fn tracker_counts_collisions_monotonically() {
        let mut t = RoleCollisionTracker::new();
        assert_eq!(t.collisions(), 0);
        assert_eq!(t.note_collision(), 1);
        assert_eq!(t.note_collision(), 2);
        assert_eq!(t.collisions(), 2);
    }

    #[test]
    fn tracker_restarting_a_run_reopens_the_window() {
        let mut t = RoleCollisionTracker::new();
        let root = PathBuf::from("/repo");
        t.record_run_started(&root, "curator", ts("2026-07-30T12:00:00Z"));
        t.record_run_finished(&root, "curator", ts("2026-07-30T12:02:00Z"));
        t.record_run_started(&root, "curator", ts("2026-07-30T12:10:00Z"));
        assert_eq!(
            t.last_run(&root, "curator"),
            Some(SelfRunWindow {
                started: ts("2026-07-30T12:10:00Z"),
                ended: None
            }),
            "a new run must clear the previous window's end stamp"
        );
    }

    // ===================================================================
    // Config resolution
    // ===================================================================

    fn write_config(root: &Path, contents: &str) {
        let dir = root.join(".loom");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.json"), contents).unwrap();
    }

    /// Isolate config reads from any host-level private defaults tier.
    fn with_isolated_config<T>(f: impl FnOnce() -> T) -> T {
        std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
        let out = f();
        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
        out
    }

    #[test]
    #[serial(loom_config_env)]
    fn detection_defaults_off() {
        let tmp = tempfile::tempdir().unwrap();
        std::env::remove_var(ROLE_COLLISION_DETECT_ENV);
        std::env::remove_var(crate::sweep_registry::COLLISION_DETECT_ENV);
        assert!(!with_isolated_config(|| resolve_detection_enabled(tmp.path())));
    }

    #[test]
    #[serial(loom_config_env)]
    fn detection_inherits_the_shared_4085_toggle() {
        let tmp = tempfile::tempdir().unwrap();
        std::env::remove_var(ROLE_COLLISION_DETECT_ENV);
        std::env::remove_var(crate::sweep_registry::COLLISION_DETECT_ENV);
        write_config(tmp.path(), r#"{"autonomous": {"collisionDetection": {"enabled": true}}}"#);
        assert!(with_isolated_config(|| resolve_detection_enabled(tmp.path())));
    }

    #[test]
    #[serial(loom_config_env)]
    fn role_runner_config_overrides_the_shared_toggle_both_ways() {
        let tmp = tempfile::tempdir().unwrap();
        std::env::remove_var(ROLE_COLLISION_DETECT_ENV);
        std::env::remove_var(crate::sweep_registry::COLLISION_DETECT_ENV);
        // Shared toggle ON, role-runner override OFF -> off.
        write_config(
            tmp.path(),
            r#"{"autonomous": {"collisionDetection": {"enabled": true},
                 "roleRunner": {"collisionDetection": false}}}"#,
        );
        assert!(!with_isolated_config(|| resolve_detection_enabled(tmp.path())));
        // Shared toggle absent, role-runner override ON -> on.
        write_config(tmp.path(), r#"{"autonomous": {"roleRunner": {"collisionDetection": true}}}"#);
        assert!(with_isolated_config(|| resolve_detection_enabled(tmp.path())));
    }

    #[test]
    #[serial(loom_config_env)]
    fn detection_env_beats_config() {
        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), r#"{"autonomous": {"roleRunner": {"collisionDetection": true}}}"#);
        std::env::set_var(ROLE_COLLISION_DETECT_ENV, "0");
        let off = with_isolated_config(|| resolve_detection_enabled(tmp.path()));
        std::env::set_var(ROLE_COLLISION_DETECT_ENV, "on");
        let on = with_isolated_config(|| resolve_detection_enabled(tmp.path()));
        std::env::remove_var(ROLE_COLLISION_DETECT_ENV);
        assert!(!off, "explicit env 0 must disable even with config true");
        assert!(on);
    }

    #[test]
    #[serial(loom_config_env)]
    fn window_defaults_to_the_role_interval() {
        let tmp = tempfile::tempdir().unwrap();
        std::env::remove_var(ROLE_COLLISION_WINDOW_ENV);
        let w = with_isolated_config(|| resolve_window(tmp.path(), Duration::from_secs(600)));
        assert_eq!(w, Duration::from_secs(600));
    }

    #[test]
    #[serial(loom_config_env)]
    fn window_is_clamped_to_the_supported_range() {
        let tmp = tempfile::tempdir().unwrap();
        std::env::remove_var(ROLE_COLLISION_WINDOW_ENV);
        let small = with_isolated_config(|| resolve_window(tmp.path(), Duration::from_secs(5)));
        let big = with_isolated_config(|| resolve_window(tmp.path(), Duration::from_secs(86_400)));
        assert_eq!(small, MIN_WINDOW);
        assert_eq!(big, MAX_WINDOW);
    }

    #[test]
    #[serial(loom_config_env)]
    fn window_config_then_env_take_precedence() {
        let tmp = tempfile::tempdir().unwrap();
        std::env::remove_var(ROLE_COLLISION_WINDOW_ENV);
        write_config(tmp.path(), r#"{"autonomous": {"roleRunner": {"collisionWindowSecs": 300}}}"#);
        let from_config =
            with_isolated_config(|| resolve_window(tmp.path(), Duration::from_secs(600)));
        std::env::set_var(ROLE_COLLISION_WINDOW_ENV, "900");
        let from_env =
            with_isolated_config(|| resolve_window(tmp.path(), Duration::from_secs(600)));
        std::env::set_var(ROLE_COLLISION_WINDOW_ENV, "0");
        let zero_dropped =
            with_isolated_config(|| resolve_window(tmp.path(), Duration::from_secs(600)));
        std::env::remove_var(ROLE_COLLISION_WINDOW_ENV);
        assert_eq!(from_config, Duration::from_secs(300));
        assert_eq!(from_env, Duration::from_secs(900));
        assert_eq!(zero_dropped, Duration::from_secs(300), "zero env falls through to config");
    }

    // ===================================================================
    // probe_before_tick_with (wiring around the pure classifier)
    // ===================================================================

    struct FakeQueue {
        items: Vec<QueueItem>,
        err: Option<String>,
        calls: Vec<(PathBuf, String)>,
    }

    impl FakeQueue {
        fn ok(items: Vec<QueueItem>) -> Self {
            Self {
                items,
                err: None,
                calls: Vec::new(),
            }
        }
        fn failing() -> Self {
            Self {
                items: Vec::new(),
                err: Some("gh exploded".to_string()),
                calls: Vec::new(),
            }
        }
    }

    impl QueueSource for FakeQueue {
        fn list_queue(&mut self, root: &Path, label: &str) -> Result<Vec<QueueItem>, String> {
            self.calls.push((root.to_path_buf(), label.to_string()));
            match &self.err {
                Some(e) => Err(e.clone()),
                None => Ok(self.items.clone()),
            }
        }
    }

    #[test]
    #[serial(loom_config_env)]
    fn probe_queries_the_roles_own_label_queue() {
        let tmp = tempfile::tempdir().unwrap();
        std::env::remove_var(ROLE_COLLISION_WINDOW_ENV);
        let mut src = FakeQueue::ok(vec![]);
        with_isolated_config(|| {
            probe_before_tick_with(
                &mut src,
                tmp.path(),
                "champion",
                Duration::from_secs(600),
                Utc::now(),
            )
        });
        assert_eq!(src.calls.len(), 1);
        assert_eq!(src.calls[0].1, "loom:pr");
    }

    #[test]
    #[serial(loom_config_env)]
    fn probe_failure_is_unknown_and_never_counted() {
        let tmp = tempfile::tempdir().unwrap();
        std::env::remove_var(ROLE_COLLISION_WINDOW_ENV);
        let before = collision_count();
        let mut src = FakeQueue::failing();
        let class = with_isolated_config(|| {
            probe_before_tick_with(
                &mut src,
                tmp.path(),
                "judge",
                Duration::from_secs(300),
                Utc::now(),
            )
        });
        assert!(matches!(class, RoleCollisionClass::Unknown(_)), "got {class:?}");
        assert_eq!(collision_count(), before, "an unverifiable probe must not count");
    }

    #[test]
    #[serial(loom_config_env)]
    fn probe_for_a_queueless_role_makes_no_forge_call() {
        let tmp = tempfile::tempdir().unwrap();
        let mut src = FakeQueue::ok(vec![pr(1, "2026-07-30T12:00:00Z")]);
        let class = with_isolated_config(|| {
            probe_before_tick_with(
                &mut src,
                tmp.path(),
                "builder",
                Duration::from_secs(300),
                Utc::now(),
            )
        });
        assert!(matches!(class, RoleCollisionClass::Unknown(_)));
        assert!(src.calls.is_empty(), "a queueless role must not hit the forge");
    }

    #[test]
    #[serial(loom_config_env)]
    fn probe_counts_a_collision_against_the_recorded_self_run() {
        let tmp = tempfile::tempdir().unwrap();
        std::env::remove_var(ROLE_COLLISION_WINDOW_ENV);
        let root = tmp.path();
        let now = ts("2026-07-30T12:06:00Z");
        // Seed this process's own last pass through the public global API.
        record_run_started(root, "guide", ts("2026-07-30T11:58:00Z"));
        record_run_finished(root, "guide", ts("2026-07-30T12:00:00Z"));
        let before = collision_count();
        let mut src = FakeQueue::ok(vec![issue(4321, "2026-07-30T12:05:00Z")]);
        let class = with_isolated_config(|| {
            probe_before_tick_with(&mut src, root, "guide", Duration::from_secs(900), now)
        });
        assert_eq!(
            class,
            RoleCollisionClass::Collision {
                number: 4321,
                updated_at: ts("2026-07-30T12:05:00Z"),
                touched: 1,
            }
        );
        assert_eq!(collision_count(), before + 1, "a detected collision increments the total");
    }

    // ===================================================================
    // RestQueueSource (production I/O) against a fake `gh`
    // ===================================================================

    /// A fake `gh` answering the ETag-cached REST listing shape
    /// (`gh api --include` → status line, headers, blank line, JSON body).
    fn write_fake_gh(dir: &Path) -> PathBuf {
        let path = dir.join("fake-gh.sh");
        let body = r#"#!/bin/sh
printf 'HTTP/2.0 200 OK\r\nEtag: W/"role-collision"\r\n\r\n'
cat <<'JSON'
[
  {"number": 4540, "state": "open", "updated_at": "2026-07-30T12:05:00Z",
   "labels": [{"name": "loom:pr"}], "pull_request": {"url": "x"}},
  {"number": 99, "state": "open", "updated_at": "2026-07-30T12:04:00Z",
   "labels": [{"name": "loom:pr"}]}
]
JSON
"#;
        std::fs::write(&path, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).unwrap();
        }
        path
    }

    #[test]
    #[serial(loom_config_env)]
    fn rest_queue_source_parses_the_live_listing_shape() {
        // Exercises the real production path (gh api --include -> forge_listing
        // -> QueueItem), not just the pure classifier: an `updated_at` that
        // does not parse, or a missed `pull_request` marker, would silently
        // disable detection for every PR-queue role.
        let tmp = tempfile::tempdir().unwrap();
        let gh = write_fake_gh(tmp.path());
        std::env::set_var("LOOM_GH_BIN", &gh);
        std::env::set_var("LOOM_REPO", format!("test/role-collision-{}", std::process::id()));
        let mut source = RestQueueSource::new();
        let items = source.list_queue(tmp.path(), "loom:pr");
        std::env::remove_var("LOOM_GH_BIN");
        std::env::remove_var("LOOM_REPO");
        let items = items.unwrap();
        assert_eq!(
            items,
            vec![
                QueueItem {
                    number: 4540,
                    updated_at: Some(ts("2026-07-30T12:05:00Z")),
                    is_pull_request: true,
                },
                QueueItem {
                    number: 99,
                    updated_at: Some(ts("2026-07-30T12:04:00Z")),
                    is_pull_request: false,
                },
            ]
        );
    }

    #[test]
    #[serial(loom_config_env)]
    fn probe_is_clean_for_activity_inside_our_own_pass() {
        let tmp = tempfile::tempdir().unwrap();
        std::env::remove_var(ROLE_COLLISION_WINDOW_ENV);
        let root = tmp.path();
        record_run_started(root, "curator", ts("2026-07-30T11:58:00Z"));
        record_run_finished(root, "curator", ts("2026-07-30T12:00:00Z"));
        let before = collision_count();
        let mut src = FakeQueue::ok(vec![issue(11, "2026-07-30T11:59:30Z")]);
        let class = with_isolated_config(|| {
            probe_before_tick_with(
                &mut src,
                root,
                "curator",
                Duration::from_secs(300),
                ts("2026-07-30T12:06:00Z"),
            )
        });
        assert_eq!(class, RoleCollisionClass::Clean);
        assert_eq!(collision_count(), before);
    }
}
