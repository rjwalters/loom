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
//! | W4 | the files need a newer daemon | report only |
//! | repo-ahead | installed by a newer daemon, or a version that cannot be ordered | report only |
//!
//! An empty payload diff is W0 whatever the version stamp says: a resync that
//! changes no file writes nothing, so a stamp that is old or lacks
//! `requires_daemon` over matching files is never rewritten and must not be
//! waited for. W4 and repo-ahead are left to the host roll (#10719).
//!
//! W2 is the resync itself, under the per-repo claim
//! ([`crate::fleet_store::resync_claim`]): a throwaway detached worktree off
//! `origin/<default>`, the payload applied there, one commit, a push that is
//! never forced. At most one W2 per pass.
//!
//! # What a pass costs
//!
//! The verdict for a workspace is cached per (default-branch commit, running
//! version), in [`Memory`].
//!
//! * A host that may not write (`fleet.autoApply` off, or not in H0, which
//!   includes `paused`) makes **no network call at all**. It classifies from
//!   the clone's own `origin/<default>` ref and reports that.
//! * A settled workspace (W0, W4, repo-ahead, not installed) costs nothing
//!   until [`recheck_after`] has passed, then one `git ls-remote` of the
//!   default branch. If the head is the cached commit, that is all.
//! * A stale workspace (W1, W3) is probed with `git ls-remote` every tick,
//!   because another host may be resyncing it.
//! * `git fetch` runs only when the probed head is not already the clone's
//!   `origin/<default>`.
//!
//! The running version only changes with a restart, which empties the cache,
//! so startup and a version change both check every workspace.
//!
//! # A pass is bounded
//!
//! * It runs on its own task, one at a time ([`host`]), so it can never
//!   delay the fleet-store sync, the floor or `paused`/`stopped` enforcement.
//! * Classification stops asking the network once [`PASS_BUDGET`] is spent.
//!   The workspaces it did not reach keep their last verdict, and the next
//!   pass starts with them.
//! * Every git child has a timeout of at most a minute ([`git`]).
//! * Three remotes in a row that do not answer end the pass's network use.
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
//! running version, and at most one per [`cooldown`]. The record is the
//! default branch itself: resync commits carry `Loom-Resync-Version`. A
//! workspace that is stale again at a version it was already resynced to is
//! refused and alerted once as `resync-loop` (see `w2::bound`).
//!
//! # When it runs
//!
//! On the fleet-sync timer and once at startup (as soon as the startup pass
//! has finished and the drain state exists). It writes only when
//! `fleet.autoApply` is on, like every other timer write. Nothing here
//! schedules anything, and nothing waits for a roll window.

mod git;
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
use crate::fleet_store::resync_claim::{stale_after, ClaimForge};
use crate::init::payload::{gate_metadata, materialize_with, Payload, ResyncRefusal};
use crate::install_compat::{Compat, DaemonCompat, InstallMeta, Version, SUPPORTS_INSTALLED};

pub use host::{host_gate, HostGateInputs, NotCurrent};
pub(super) use host::{latest, mark_boot, mark_verified, registered_roots, spawn_pass};
use memory::FailureKind;
pub use memory::{recheck_after, Memory, OUTAGE_HOLD_CAP, RECHECK_TICKS};
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

/// Remotes in a row that do not answer before a pass stops asking.
pub const UNREACHABLE_IN_A_ROW: u32 = 3;

/// The least time between two daemon resync commits on one repo, whoever
/// made them and whatever version they installed: the claim's stale window,
/// `max(10 x syncIntervalSecs, 15 min)`.
#[must_use]
pub fn cooldown(interval: Duration) -> Duration {
    stale_after(interval)
}

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
    /// Needs a newer daemon than this one.
    W4,
    /// Installed by a newer daemon, or at a version that cannot be ordered.
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

    /// The repo is ahead of this daemon: the input to #10719's
    /// `repo_ahead_target` and its `daemon-too-old` hold.
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
}

/// A failure that needs a person: published on [`ALERT_TOPIC`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Alert {
    /// The registered root.
    pub root: PathBuf,
    /// `OWNER/REPO`, when known.
    pub repo: Option<String>,
    /// `branch-protection`, `failure`, `resync-loop`, or `network` (one for
    /// the host, with an empty `root`, however many remotes are down).
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
            lines.push(format!("  workspace {name}: {}{reason}{installed}", w.state.as_str()));
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
    /// The fleet-sync cadence: the unit of the backoff, the recheck window
    /// and the stale window.
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
    probes: u32,
    fetches: u32,
    reached: u32,
    unreachable: u32,
    in_a_row: u32,
    first_error: Option<String>,
    alerts: Vec<Alert>,
}

impl Scan {
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
        let (report, found) = if resume.is_some() {
            (carried(env, root, memory), None)
        } else {
            classify(env, root, gate, memory, &mut scan)
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
            if !scan.online || attempt(env, candidate, report, memory, &mut scan) {
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
    log::debug!(
        "workspace_resync: pass over {count} workspace(s): {} ls-remote, {} fetch",
        scan.probes,
        scan.fetches
    );
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
    }
}

fn stamp(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// The report for a workspace this pass did not get to: its last verdict.
/// No git, no network.
fn carried(env: &Env<'_>, root: &Path, memory: &Memory) -> WorkspaceReport {
    let mut report = report_for(root);
    report.repo = (env.nwo)(root);
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
    let Some(nwo) = (env.nwo)(root) else {
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
    let online = scan.online;
    match classify_head(env, root, gate, memory, scan, online, &mut report) {
        Ok(found) => (report, found.map(|(branch, commit)| (nwo, branch, commit))),
        Err(e) => {
            if let Some(down) = e.downcast_ref::<git::Unreachable>() {
                // The remote did not answer. Not this repo's failure to be
                // alerted on: the pass reports it once, for the host. The
                // workspace is still reported, from what the clone holds.
                scan.no_answer(root, &nwo, &down.0, memory);
                let mut offline = report_for(root);
                offline.repo = Some(nwo.clone());
                if classify_head(env, root, gate, memory, scan, false, &mut offline).is_ok() {
                    report = offline;
                }
                let detail = format!("remote did not answer: {}", down.0);
                record_failure(
                    env,
                    &mut report,
                    FailureKind::Unreachable,
                    &detail,
                    memory,
                    &mut scan.alerts,
                );
            } else {
                let detail = format!("{e:#}");
                record_failure(
                    env,
                    &mut report,
                    FailureKind::Other,
                    &detail,
                    memory,
                    &mut scan.alerts,
                );
            }
            (report, None)
        }
    }
}

/// The part of [`classify`] that can fail: find the default branch's head
/// (asking the remote only when `online` and the cached verdict is due),
/// and evaluate it unless it is the commit already evaluated. Returns the
/// branch and commit of a stale workspace.
fn classify_head(
    env: &Env<'_>,
    root: &Path,
    gate: std::result::Result<(), NotCurrent>,
    memory: &mut Memory,
    scan: &mut Scan,
    online: bool,
    report: &mut WorkspaceReport,
) -> Result<Option<(String, String)>> {
    let branch = git::default_branch(root)
        .ok_or_else(|| anyhow!("no usable default branch ref (origin/HEAD or origin/main)"))?;
    let version = env.running.to_string();
    let now = (env.clock)();
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
    let mut probed = false;
    let commit = if online {
        if let Some(verdict) = cached.as_ref().filter(|v| !v.probe_due(now, env.interval)) {
            return Ok(reuse(verdict, report));
        }
        scan.probes += 1;
        let head = git::remote_head(root, &branch)?;
        scan.answered(root, memory);
        probed = true;
        if let Some(verdict) = known(&head) {
            memory.confirm(root, now);
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
        memory.settle(root, &branch, &commit, &version, report, probed, now, env.interval);
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
        ResyncRefusal::PendingAheadOfDaemon { .. } | ResyncRefusal::UnrecognizedVersion { .. } => {
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
