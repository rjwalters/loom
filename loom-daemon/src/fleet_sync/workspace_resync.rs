//! The workspace half of fleet-sync: keep each registered repo's installed
//! Loom at the version this daemon is RUNNING (#10718, tracker #10698
//! Phase 4, workspace states W0-W2).
//!
//! # What a pass does
//!
//! For every registered workspace it reads the install metadata from the
//! default branch (never the working tree) and classifies:
//!
//! | State | Meaning | This pass |
//! |---|---|---|
//! | W0 | the default branch's installed files equal this daemon's payload | nothing |
//! | W1 | compatible, files differ | try the claim |
//! | W3 | too old for this daemon or the floor, files differ | as W1, first |
//! | W4 | the files need a newer daemon, or record a version that cannot be ordered | report only |
//! | repo-ahead | installed by a newer daemon, and still compatible with this one | report only |
//!
//! An empty payload diff is W0 whatever the version stamp says: a resync that
//! changes no file writes nothing, so a stamp that is old or lacks
//! `requires_daemon` over matching files is never rewritten and must not be
//! waited for.
//!
//! W3 and W4 hold new dispatch into the workspace, and W4 makes this host a
//! roll candidate ([`crate::workspace_hold`], #10719). A repo that is only
//! repo-ahead is neither held nor a reason to roll: this host keeps working
//! it and never resyncs it downward.
//!
//! W2 is the resync itself, under the per-repo claim
//! ([`crate::fleet_store::resync_claim`]): a throwaway detached worktree off
//! `origin/<default>`, the payload applied there, one commit, a push that is
//! never forced. At most one W2 per pass.
//!
//! # What a pass costs
//!
//! Every pass checks every workspace's default-branch head (#10987), so a
//! change made by anyone (a person, a fresh install, a newer host) is
//! classified on the next tick. The verdict for a workspace is cached per
//! (default-branch commit, running version), in [`Memory`].
//!
//! * A host that may not write (`fleet.autoApply` off, or not in H0, which
//!   includes `paused`) makes **no network call at all**. It classifies from
//!   the clone's own `origin/<default>` ref and reports that.
//! * Otherwise the pass asks the forge for every head at once: one GraphQL
//!   query per owner ([`heads`]). A workspace whose head is the cached commit
//!   costs nothing more: no git child at all, so no fetch.
//! * `git fetch` runs only when the head is not the cached commit and is not
//!   already the clone's `origin/<default>`.
//! * When that query fails, or does not cover a repo, the repo's remote is
//!   asked with `git ls-remote` instead, [`heads::PROBE_PARALLEL`] at a time,
//!   inside [`PASS_BUDGET`].
//! * While the rate-limit breaker is open nothing is asked at all: the pass
//!   reports from the cache and says so.
//!
//! The running version only changes with a restart, which empties the cache,
//! so startup and a version change both evaluate every workspace.
//!
//! # A pass is bounded
//!
//! * It runs on its own task, one at a time ([`host`]), so it can never
//!   delay the fleet-store sync, the floor or `paused`/`stopped` enforcement.
//! * Classification stops once [`PASS_BUDGET`] is spent. The workspaces it
//!   did not reach keep their last verdict, and the next pass starts with
//!   them.
//! * No resync starts after [`PASS_DEADLINE`].
//! * Every git child has a timeout of at most a minute ([`git`]), and the
//!   head query one of fifteen seconds.
//! * Three remotes in a row that do not answer end the pass's network use.
//! * A pass that is still running several ticks later is an alert, and one
//!   that never ends gives the slot up ([`host`]).
//!
//! # One repo's failure is not the host's
//!
//! A remote that does not answer counts toward the host's one `network`
//! alert and, when no remote answers at all, a hold off the network. A repo
//! the forge or the remote *refuses* (it was deleted or renamed, or the
//! credential may not read it) is that repo's failure: it backs off and
//! alerts on its own as `repo-access`, and never counts as an outage.
//!
//! # Only from H0, only an official release build
//!
//! [`host_gate`] decides whether this host may claim or write at all. It is
//! read at the start of the pass, again immediately before the claim, and
//! again immediately before the push. A binary that is not a verified
//! official release build never passes it ([`crate::release_provenance`]).
//!
//! # A repo cannot be resynced in a loop
//!
//! Whatever the cause, a repo gets at most one daemon resync commit per
//! running version. The record is the default branch itself: resync commits
//! carry `Loom-Resync-Version`. A workspace that is stale again at a version
//! it was already resynced to is refused and alerted once as `resync-loop`
//! (see `w2::bound`). There is no wait between two resyncs to different
//! versions (#10987): a host never installs a version older than the one a
//! repo has, so each version can land at most once.
//!
//! # When it runs
//!
//! On the fleet-sync timer and once at startup (as soon as the startup pass
//! has finished and the drain state exists). It writes only when
//! `fleet.autoApply` is on, like every other timer write. Nothing here
//! schedules anything, and nothing waits for a roll window.

mod git;
pub mod heads;
mod hold;
mod host;
mod memory;
mod w2;

use std::cell::OnceCell;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};

use super::Mode;
use crate::fleet_store::resync_claim::ClaimForge;
use crate::init::payload::{gate_metadata, materialize_with, Payload, ResyncRefusal};
use crate::install_compat::{Compat, DaemonCompat, InstallMeta, Version, SUPPORTS_INSTALLED};

use heads::{Asked, HeadAsk, Heads};
pub use host::{host_gate, HostGateInputs, NotCurrent, ABANDON_AFTER, STUCK_AFTER_TICKS};
pub(super) use host::{latest, mark_boot, mark_verified, spawn_pass};
use memory::FailureKind;
pub use memory::{Memory, OUTAGE_HOLD_CAP};
use w2::attempt;

/// Event-bus topic a resync failure that needs a person is published on.
pub const ALERT_TOPIC: &str = "fleet_sync.workspace_resync";

/// Ceiling of the per-repo backoff.
pub const BACKOFF_CAP: Duration = Duration::from_secs(6 * 60 * 60);

/// Consecutive failures (other than a protection rule) before an alert.
pub const ALERT_AFTER: u32 = 3;

/// How long one pass may spend asking remotes for their heads. Workspaces
/// it did not reach are first in the next pass.
pub const PASS_BUDGET: Duration = Duration::from_secs(45);

/// How long after it started a pass may still begin a resync. A pass that
/// has run this long leaves its stale workspaces to the next one.
pub const PASS_DEADLINE: Duration = Duration::from_secs(120);

/// Remotes in a row that do not answer before a pass stops asking.
pub const UNREACHABLE_IN_A_ROW: u32 = 3;

// ============================================================================
// What a pass reports
// ============================================================================

/// A workspace's state this pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WState {
    /// Current.
    W0,
    /// Stale: compatible, but the installed files differ.
    W1,
    /// Resyncing. Lives only inside a pass, so a snapshot never shows it.
    W2,
    /// Too old for this daemon or the floor.
    W3,
    /// Needs a newer daemon than this one, or records a version that cannot
    /// be ordered against it (so it may).
    W4,
    /// Installed by a newer daemon whose files still work with this one.
    #[serde(rename = "repo-ahead")]
    RepoAhead,
    /// Not a workspace this pass manages (see the reason).
    #[serde(rename = "skipped")]
    Skipped,
    /// Could not be classified this pass (see the reason).
    #[serde(rename = "unknown")]
    Unknown,
}

impl WState {
    /// The label `loom-daemon status` prints.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::W0 => "W0",
            Self::W1 => "W1",
            Self::W2 => "W2",
            Self::W3 => "W3",
            Self::W4 => "W4",
            Self::RepoAhead => "repo-ahead",
            Self::Skipped => "skipped",
            Self::Unknown => "unknown",
        }
    }

    /// The repo is ahead of this daemon, so this host never resyncs it.
    /// Only [`Self::W4`] holds dispatch and asks for a roll (#10719).
    #[must_use]
    pub fn repo_ahead(self) -> bool {
        matches!(self, Self::W4 | Self::RepoAhead)
    }
}

/// One workspace, as the last pass found it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceReport {
    /// The registered root.
    pub root: PathBuf,
    /// `OWNER/REPO`, when the origin is a GitHub remote.
    pub repo: Option<String>,
    /// Its state.
    pub state: WState,
    /// The default branch's recorded `loom_version`.
    pub installed: Option<String>,
    /// The default branch's recorded `requires_daemon`.
    pub requires_daemon: Option<String>,
    /// Why it is in that state, or what this pass did about it.
    pub reason: Option<String>,
    /// The dispatch hold standing on it, if any (#10719).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hold: Option<crate::workspace_hold::WorkspaceHold>,
}

/// A failure that needs a person: published on [`ALERT_TOPIC`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Alert {
    /// The registered root.
    pub root: PathBuf,
    /// `OWNER/REPO`, when known.
    pub repo: Option<String>,
    /// For one repo: `branch-protection`, `failure`, `resync-loop`, or
    /// `repo-access` (the forge or the remote refuses this repo). For the
    /// host, with an empty `root`: `network` (one, however many remotes are
    /// down), `head-query` (the batched head query keeps failing) or
    /// `pass-stuck` (a pass is still running several ticks later).
    pub kind: &'static str,
    /// Consecutive failures so far.
    pub failures: u32,
    /// What failed (for a protection failure, the rule the remote named).
    pub detail: String,
    /// When the repo is tried again.
    pub next_attempt: DateTime<Utc>,
}

/// What one workspace pass found and did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspacePass {
    /// The version this daemon runs and would install.
    pub running: String,
    /// Why this host only reported, when it did: it is not in H0, or it is
    /// not a verified official release build. A host-level fact, said once.
    pub host: Option<String>,
    /// Every registered workspace.
    pub workspaces: Vec<WorkspaceReport>,
    /// Failures to publish. Not part of the snapshot.
    #[serde(skip)]
    pub alerts: Vec<Alert>,
    /// Forge requests the head check made (one per owner). Not part of the
    /// snapshot.
    #[serde(skip)]
    pub head_queries: u32,
    /// `git ls-remote` calls this pass made. Not part of the snapshot.
    #[serde(skip)]
    pub probes: u32,
    /// `git fetch` calls this pass made while classifying. Not part of the
    /// snapshot.
    #[serde(skip)]
    pub fetches: u32,
}

impl WorkspacePass {
    /// Nothing to show: no pass has run. A snapshot in this state omits the
    /// field, so a host without the feature writes the snapshot it always did.
    #[must_use]
    pub fn is_unset(&self) -> bool {
        self.workspaces.is_empty() && self.host.is_none()
    }

    /// The workspace lines of the `Fleet store:` block.
    #[must_use]
    pub fn lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        if let Some(why) = &self.host {
            lines.push(format!("  workspaces: {why}; reporting only"));
        }
        for w in &self.workspaces {
            let name = w
                .repo
                .clone()
                .unwrap_or_else(|| w.root.display().to_string());
            let installed = w
                .installed
                .as_deref()
                .map_or_else(String::new, |v| format!(", installed {v}"));
            let reason = w
                .reason
                .as_deref()
                .map_or_else(String::new, |r| format!(" ({r})"));
            let hold = w
                .hold
                .as_ref()
                .map_or_else(String::new, |h| format!("; {}: {}", h.note(), h.detail));
            lines
                .push(format!("  workspace {name}: {}{reason}{installed}{hold}", w.state.as_str()));
        }
        lines
    }
}

// ============================================================================
// The pass
// ============================================================================

/// The running release's payload, unpacked the first time a pass needs it.
/// A pass over unchanged repos never does.
pub struct LazyPayload {
    make: Box<dyn Fn() -> Result<Payload>>,
    cell: OnceCell<std::result::Result<Payload, String>>,
}

impl LazyPayload {
    /// A payload built by `make` on first use.
    #[must_use]
    pub fn new(make: impl Fn() -> Result<Payload> + 'static) -> Self {
        Self {
            make: Box::new(make),
            cell: OnceCell::new(),
        }
    }

    fn get(&self) -> Result<&Payload> {
        self.cell
            .get_or_init(|| (self.make)().map_err(|e| format!("{e:#}")))
            .as_ref()
            .map_err(|e| anyhow!("the resync payload is unavailable: {e}"))
    }
}

/// Everything a pass reads from outside itself. Production builds it in
/// [`host`]; tests inject each piece.
pub struct Env<'a> {
    /// This host's id.
    pub host: &'a str,
    /// The RUNNING daemon version: what a resync installs.
    pub running: Version,
    /// The fleet floor in force, if any.
    pub floor: Option<Version>,
    /// The fleet-sync cadence: the unit of the backoff and the stale window.
    pub interval: Duration,
    /// The running release's payload.
    pub payload: &'a LazyPayload,
    /// `OWNER/REPO` of a workspace's origin; `None` when it is not GitHub.
    pub nwo: &'a dyn Fn(&Path) -> Option<String>,
    /// May this host write to a workspace's repo at all (#9548: a managed
    /// repo, and a credential with WRITE)? `Err` carries why not.
    pub may_write: &'a dyn Fn(&Path, &str) -> std::result::Result<(), String>,
    /// The forge for a workspace's repo, under that repo's credentials.
    pub forge: &'a dyn Fn(&Path, &str) -> Box<dyn ClaimForge + 'a>,
    /// The host gate, read live each time it is called.
    pub gate: &'a dyn Fn() -> std::result::Result<(), NotCurrent>,
    /// The clock.
    pub clock: &'a dyn Fn() -> DateTime<Utc>,
    /// Has this pass used its [`PASS_BUDGET`]? Read before each workspace.
    pub spent: &'a dyn Fn() -> bool,
    /// Is this pass past its [`PASS_DEADLINE`]? Read before each resync.
    pub overdue: &'a dyn Fn() -> bool,
    /// The default-branch head of every repo asked about, in as few forge
    /// requests as that takes ([`heads::live`]). Called at most once per
    /// pass, and never by a host that may not write.
    pub heads: &'a dyn Fn(&[HeadAsk]) -> Heads,
}

/// A stale workspace this pass may resync.
struct Candidate {
    index: usize,
    nwo: String,
    branch: String,
    commit: String,
}

/// What one pass has done on the network so far, and what it will publish.
#[derive(Default)]
struct Scan {
    /// May this pass ask remotes and the forge at all?
    online: bool,
    /// The rate-limit breaker is open: this pass asked nothing.
    breaker_open: bool,
    head_queries: u32,
    /// Why the head query got no usable answer, when it did not.
    head_failure: Option<String>,
    probes: u32,
    fetches: u32,
    reached: u32,
    unreachable: u32,
    in_a_row: u32,
    first_error: Option<String>,
    alerts: Vec<Alert>,
}

impl Scan {
    /// The remote (or the forge, about this repo) answered: with a head, or
    /// with a refusal. Either way the network is there.
    fn answered(&mut self, root: &Path, memory: &mut Memory) {
        self.reached += 1;
        self.in_a_row = 0;
        memory.down.remove(root);
    }

    fn no_answer(&mut self, root: &Path, name: &str, detail: &str, memory: &mut Memory) {
        self.unreachable += 1;
        self.in_a_row += 1;
        self.first_error
            .get_or_insert_with(|| format!("{name}: {detail}"));
        memory.down.insert(root.to_path_buf());
        if self.in_a_row >= UNREACHABLE_IN_A_ROW && self.online {
            log::warn!(
                "workspace_resync: {UNREACHABLE_IN_A_ROW} remotes in a row did not answer; not \
                 asking any more this pass"
            );
            self.online = false;
        }
    }
}

/// Run one workspace pass over `roots`. Never fails: every failure lands in a
/// report, the backoff and (at the thresholds) an alert.
pub fn run(env: &Env<'_>, roots: &[PathBuf], mode: Mode, memory: &mut Memory) -> WorkspacePass {
    let gate = (env.gate)();
    let started = (env.clock)();
    let mut pass = WorkspacePass {
        running: env.running.to_string(),
        host: gate.err().map(|why| why.host_note()),
        ..WorkspacePass::default()
    };
    if let Err(why) = gate {
        if why.is_about_the_build() && memory.noted.insert(format!("release:{why}")) {
            log::warn!(
                "workspace_resync: {why}, so this host does not resync any workspace (reported \
                 once)"
            );
        }
    }
    // The network is for a host that may write. One that may not (autoApply
    // off, paused, rolling, not a release build) reports from what its clones
    // already hold and asks nobody.
    let mut scan = Scan {
        online: mode == Mode::Write && gate.is_ok() && memory.outage_hold(started).is_none(),
        ..Scan::default()
    };
    let count = roots.len();
    let names: Vec<Option<String>> = roots.iter().map(|root| (env.nwo)(root)).collect();
    // Every head, asked once (#10987). A host that is offline asks nothing.
    let mut asked = if scan.online {
        heads::check(env, roots, &names, memory, &mut scan)
    } else {
        std::collections::HashMap::new()
    };
    if scan.breaker_open {
        pass.host.get_or_insert_with(|| {
            "the forge rate-limit breaker is open, so no default-branch head was checked this \
             tick"
                .to_string()
        });
    }
    let first = if count == 0 { 0 } else { memory.cursor % count };
    let mut reports: Vec<Option<WorkspaceReport>> = vec![None; count];
    let mut candidates = Vec::new();
    let mut resume = None;
    for step in 0..count {
        let index = (first + step) % count;
        let Some(root) = roots.get(index) else {
            continue;
        };
        if resume.is_none() && (env.spent)() {
            resume = Some(index);
        }
        let nwo = names.get(index).cloned().flatten();
        let (report, found) = if resume.is_some() {
            (carried(root, nwo, memory), None)
        } else {
            let head = asked.remove(root).unwrap_or(Asked::No);
            classify(env, root, nwo, head, gate, memory, &mut scan)
        };
        if let Some((nwo, branch, commit)) = found {
            candidates.push(Candidate {
                index,
                nwo,
                branch,
                commit,
            });
        }
        if let Some(slot) = reports.get_mut(index) {
            *slot = Some(report);
        }
    }
    if let Some(index) = resume {
        memory.cursor = index;
        log::info!(
            "workspace_resync: the pass's {}s budget was spent; {} workspace(s) keep their last \
             verdict and are first next pass",
            PASS_BUDGET.as_secs(),
            (first + count - index) % count
        );
    }
    pass.workspaces = reports.into_iter().flatten().collect();
    if mode == Mode::Write && gate.is_ok() {
        // W3 first: dispatch into it is (or will be) held until it is resynced.
        candidates.sort_by_key(|c| {
            pass.workspaces
                .get(c.index)
                .is_none_or(|w| w.state != WState::W3)
        });
        for candidate in &candidates {
            let Some(report) = pass.workspaces.get_mut(candidate.index) else {
                continue;
            };
            if !scan.online {
                break;
            }
            if (env.overdue)() {
                log::info!(
                    "workspace_resync: the pass is past its {}s deadline; no resync is started \
                     and the stale workspaces wait for the next pass",
                    PASS_DEADLINE.as_secs()
                );
                break;
            }
            if attempt(env, candidate, report, memory, &mut scan) {
                break;
            }
        }
    }
    let outage = memory.note_network(
        scan.reached,
        scan.unreachable,
        scan.first_error.as_deref(),
        env.interval,
        (env.clock)(),
    );
    if let Some(alert) = outage {
        log::error!("workspace_resync: {}", alert.detail);
        scan.alerts.push(alert);
    }
    if scan.head_queries > 0 {
        // A query that failed while no remote answered either is the outage
        // above, not a second thing to report.
        let failure = scan
            .head_failure
            .as_deref()
            .filter(|_| scan.reached > 0 || scan.unreachable == 0);
        let failing = scan.head_failure.is_some();
        match memory.note_head_query(failure, failing, env.interval, (env.clock)()) {
            (true, _) => log::warn!(
                "workspace_resync: the batched head query failed ({}); each remote is asked \
                 with git ls-remote instead",
                failure.unwrap_or("no detail")
            ),
            (false, Some(alert)) => {
                log::error!("workspace_resync: {}", alert.detail);
                scan.alerts.push(alert);
            }
            (false, None) => {}
        }
    }
    log::debug!(
        "workspace_resync: pass over {count} workspace(s): {} head query, {} ls-remote, {} fetch",
        scan.head_queries,
        scan.probes,
        scan.fetches
    );
    pass.head_queries = scan.head_queries;
    pass.probes = scan.probes;
    pass.fetches = scan.fetches;
    pass.alerts = scan.alerts;
    pass
}

fn report_for(root: &Path) -> WorkspaceReport {
    WorkspaceReport {
        root: root.to_path_buf(),
        repo: None,
        state: WState::Unknown,
        installed: None,
        requires_daemon: None,
        reason: None,
        hold: None,
    }
}

/// What a finished pass tells the dispatch hold (#10719): both copies of
/// every workspace in `pass`. Reads each checkout's install metadata; no
/// network.
pub fn observations(
    env: &Env<'_>,
    pass: &WorkspacePass,
    memory: &mut Memory,
) -> Vec<crate::workspace_hold::Observation> {
    hold::observe(env, &pass.workspaces, (env.gate)(), memory)
}

fn stamp(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// The report for a workspace this pass did not get to: its last verdict.
/// No git, no network.
fn carried(root: &Path, nwo: Option<String>, memory: &Memory) -> WorkspaceReport {
    let mut report = report_for(root);
    report.repo = nwo;
    match memory.verdicts.get(root) {
        Some(verdict) => verdict.fill(&mut report),
        None => {
            report.reason = Some("not checked yet: the pass ran out of time".to_string());
        }
    }
    report
}

/// Record a failure against `report`'s repo and say so in the report.
fn record_failure(
    env: &Env<'_>,
    report: &mut WorkspaceReport,
    kind: FailureKind,
    detail: &str,
    memory: &mut Memory,
    alerts: &mut Vec<Alert>,
) {
    let (until, alert) = memory.fail(report, kind, detail, env.interval, (env.clock)());
    report.reason = Some(format!("backoff until {}: {detail}", stamp(until)));
    if let Some(alert) = alert {
        log::error!(
            "workspace_resync: {} ({}): {} failure(s): {detail}; next attempt {}",
            report.repo.as_deref().unwrap_or("unknown repo"),
            report.root.display(),
            alert.failures,
            stamp(until)
        );
        alerts.push(alert);
    } else {
        log::warn!("workspace_resync: {}: {detail}", report.root.display());
    }
}

/// Classify one workspace. The second value is set for a stale workspace that
/// could be resynced: its repo, default branch and the commit that was read.
fn classify(
    env: &Env<'_>,
    root: &Path,
    nwo: Option<String>,
    asked: Asked,
    gate: std::result::Result<(), NotCurrent>,
    memory: &mut Memory,
    scan: &mut Scan,
) -> (WorkspaceReport, Option<(String, String, String)>) {
    let mut report = report_for(root);
    let skip = |mut report: WorkspaceReport, why: &str| {
        report.state = WState::Skipped;
        report.reason = Some(why.to_string());
        (report, None)
    };
    if crate::init::is_loom_source_repo(root) {
        return skip(report, "the Loom source repo installs from its own tree");
    }
    let Some(nwo) = nwo else {
        if memory.noted.insert(format!("forge:{}", root.display())) {
            log::info!(
                "workspace_resync: {}: forge-unsupported (origin is not a GitHub remote); not \
                 resynced (reported once)",
                root.display()
            );
        }
        return skip(report, "forge-unsupported: origin is not a GitHub remote");
    };
    report.repo = Some(nwo.clone());
    // A resync that was killed leaves its worktree behind. One pass runs at a
    // time and one daemon per host, so none is in use now.
    git::clean_stale_worktrees(root);
    if let Some(b) = memory.backoff.get(root) {
        if b.next_attempt > (env.clock)() {
            report.state = b.state;
            report.reason =
                Some(format!("backoff until {}: {}", stamp(b.next_attempt), b.last_error));
            return (report, None);
        }
    }
    // A pass that has stopped asking (three remotes in a row did not answer)
    // classifies the rest from what their clones hold.
    let asked = if scan.online { asked } else { Asked::No };
    match classify_head(env, root, gate, memory, scan, asked, &mut report) {
        Ok(found) => (report, found.map(|(branch, commit)| (nwo, branch, commit))),
        Err(e) => {
            if let Some(low) = e.downcast_ref::<git::LowDisk>() {
                // Below the disk floor the fetch was skipped (#10995). The
                // remote answered, so this is neither a network outage nor a
                // repo failure. It returns here, before the failure
                // accounting below: no `no_answer`, no `record_failure`, no
                // backoff, no alert. The workspace is reported from what the
                // clone holds, and the next pass retries as soon as the disk
                // has room.
                log::info!("workspace_resync: {}: low-disk: {}", root.display(), low.0);
                let mut offline = report_for(root);
                offline.repo = Some(nwo.clone());
                if classify_head(env, root, gate, memory, scan, Asked::No, &mut offline).is_ok() {
                    report = offline;
                }
                report.reason = Some(format!("low-disk: {}", low.0));
                return (report, None);
            }
            // Which kind of failure: the remote did not answer (the host's,
            // reported once for the pass), the remote or the forge refused
            // this repo (the repo's own), or anything else.
            let (kind, detail) = if let Some(down) = e.downcast_ref::<git::Unreachable>() {
                scan.no_answer(root, &nwo, &down.0, memory);
                (FailureKind::Unreachable, format!("remote did not answer: {}", down.0))
            } else if let Some(refused) = e.downcast_ref::<git::Refused>() {
                scan.answered(root, memory);
                (FailureKind::Refused, refused.0.clone())
            } else {
                (FailureKind::Other, format!("{e:#}"))
            };
            if kind != FailureKind::Other {
                // The workspace is still reported, from what the clone holds.
                let mut offline = report_for(root);
                offline.repo = Some(nwo.clone());
                if classify_head(env, root, gate, memory, scan, Asked::No, &mut offline).is_ok() {
                    report = offline;
                }
            }
            record_failure(env, &mut report, kind, &detail, memory, &mut scan.alerts);
            (report, None)
        }
    }
}

/// The part of [`classify`] that can fail: take the default branch's head
/// from what the head check learned (`asked`; [`Asked::No`] reads the clone's
/// own remote-tracking ref and asks nobody), and evaluate it unless it is the
/// commit already evaluated. Returns the branch and commit of a stale
/// workspace.
fn classify_head(
    env: &Env<'_>,
    root: &Path,
    gate: std::result::Result<(), NotCurrent>,
    memory: &mut Memory,
    scan: &mut Scan,
    asked: Asked,
    report: &mut WorkspaceReport,
) -> Result<Option<(String, String)>> {
    let version = env.running.to_string();
    if let Asked::Forge { branch, commit } = &asked {
        // The settled case: the forge names the branch and commit already
        // evaluated for this version. Nothing to run.
        let settled = memory
            .verdicts
            .get(root)
            .filter(|v| v.version == version && v.branch == *branch && v.commit == *commit)
            .cloned();
        if let Some(verdict) = settled {
            scan.answered(root, memory);
            verdict.fill(report);
            return Ok(verdict.stale().then(|| (branch.clone(), commit.clone())));
        }
    }
    let branch = git::default_branch(root)
        .ok_or_else(|| anyhow!("no usable default branch ref (origin/HEAD or origin/main)"))?;
    let cached = memory
        .verdicts
        .get(root)
        .filter(|v| v.version == version && v.branch == branch)
        .cloned();
    let known = |commit: &str| cached.as_ref().filter(|v| v.commit == commit);
    let reuse = |verdict: &memory::Verdict, report: &mut WorkspaceReport| {
        verdict.fill(report);
        verdict
            .stale()
            .then(|| (branch.clone(), verdict.commit.clone()))
    };
    let head = match asked {
        Asked::No => None,
        Asked::Forge {
            branch: theirs,
            commit,
        } if theirs == branch => Some(commit),
        Asked::Forge { .. } => {
            // The forge's default branch is not the one this clone follows
            // (it was renamed since the clone was made). The pass works on
            // the clone's, so that is the branch the remote is asked about.
            scan.probes += 1;
            Some(git::remote_head(root, &branch)?)
        }
        Asked::Remote(found) => Some(found?),
    };
    let commit = if let Some(head) = head {
        scan.answered(root, memory);
        if let Some(verdict) = known(&head) {
            return Ok(reuse(verdict, report));
        }
        if git::tracking_head(root, &branch).as_deref() == Some(head.as_str()) {
            // The clone already has this commit: nothing to fetch.
            head
        } else {
            scan.fetches += 1;
            git::fetch(root, &branch)?
        }
    } else {
        let Some(head) = git::tracking_head(root, &branch) else {
            // Nothing to read, and this pass asks no remote. Not a failure.
            report.reason = Some(format!("origin/{branch} has not been fetched in this clone"));
            return Ok(None);
        };
        if let Some(verdict) = known(&head) {
            return Ok(reuse(verdict, report));
        }
        head
    };
    if evaluate(env, root, &branch, &commit, gate, report)? {
        memory.settle(root, &branch, &commit, &version, report);
    }
    if report.state == WState::W0 {
        // A repo that reached W0 has nothing left to back off from.
        memory.backoff.remove(root);
    }
    let stale = matches!(report.state, WState::W1 | WState::W3);
    Ok(stale.then_some((branch, commit)))
}

/// Read `commit`'s install metadata, gate it and diff the payload against it,
/// into `report`. Returns whether the result is worth remembering.
fn evaluate(
    env: &Env<'_>,
    root: &Path,
    branch: &str,
    commit: &str,
    gate: std::result::Result<(), NotCurrent>,
    report: &mut WorkspaceReport,
) -> Result<bool> {
    let Some(raw) = git::metadata_at(root, commit)? else {
        report.state = WState::Skipped;
        report.reason = Some("Loom is not installed on the default branch".to_string());
        return Ok(true);
    };
    if let Ok(meta) = InstallMeta::parse(&raw) {
        report.installed = meta.loom_version;
        report.requires_daemon = meta.requires_daemon;
    }
    let compat = match gate_metadata(&raw, &daemon(env)) {
        Ok(compat) => compat,
        Err(ResyncRefusal::UnreadableMetadata(e)) => {
            return Err(anyhow!("install metadata on origin/{branch} is unreadable: {e}"));
        }
        Err(refusal) => {
            (report.state, report.reason) = refused(&refusal);
            return Ok(true);
        }
    };
    if gate == Err(NotCurrent::NotAReleaseBuild) {
        // Without a release payload there is nothing to diff against. The
        // reason is the host's, already said once on the pass.
        return Ok(false);
    }
    if diff_is_stale(env, root, commit)? {
        report.state = if compat == Compat::InstalledTooOld {
            WState::W3
        } else {
            WState::W1
        };
    } else {
        report.state = WState::W0;
        if compat != Compat::Compatible {
            // The stamp is old or incomplete, the files are not. No resync
            // will ever rewrite it, so this is current, not owed.
            report.reason = Some("files match the payload; the stamp is left as is".to_string());
        }
    }
    Ok(true)
}

fn daemon(env: &Env<'_>) -> DaemonCompat {
    DaemonCompat {
        running: env.running,
        supports_installed: Version::parse(SUPPORTS_INSTALLED).unwrap_or(env.running),
        floor: env.floor,
    }
}

/// The state and reason for a workspace the gate refuses.
fn refused(refusal: &ResyncRefusal) -> (WState, Option<String>) {
    match refusal {
        ResyncRefusal::NeedsNewerDaemon { requires, running } => {
            (WState::W4, Some(format!("requires daemon {requires} > running {running}")))
        }
        ResyncRefusal::RepoAheadOfDaemon { installed, running } => {
            (WState::RepoAhead, Some(format!("installed {installed} > running {running}")))
        }
        // Cannot be ordered, so it may need a newer daemon: W4 (#10719).
        ResyncRefusal::UnrecognizedVersion { .. } => (WState::W4, Some(refusal.to_string())),
        ResyncRefusal::PendingAheadOfDaemon { .. } => {
            (WState::RepoAhead, Some(refusal.to_string()))
        }
        other => (WState::Unknown, Some(other.to_string())),
    }
}

/// Does this daemon's payload differ from `commit`'s installed files in a way
/// a resync could close? The caller caches the answer per commit.
fn diff_is_stale(env: &Env<'_>, root: &Path, commit: &str) -> Result<bool> {
    let payload = env.payload.get()?;
    let tree = tempfile::Builder::new()
        .prefix("loom-resync-tree-")
        .tempdir()?;
    git::export_surfaces(root, commit, tree.path())?;
    let diff = materialize_with(payload, tree.path())?;
    Ok(diff.stamp_pending() || {
        let paths: Vec<String> = diff
            .added
            .iter()
            .chain(&diff.changed)
            .chain(&diff.removed)
            .cloned()
            .collect();
        let ignored = git::ignored(root, root, &paths);
        paths.iter().any(|p| !ignored.contains(p))
    })
}

#[cfg(test)]
#[path = "tests/workspace_resync.rs"]
mod tests;
