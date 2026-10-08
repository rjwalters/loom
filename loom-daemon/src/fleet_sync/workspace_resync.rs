//! The workspace half of fleet-sync: keep each registered repo's installed
//! Loom at the version this daemon is RUNNING (#10718, tracker #10698
//! Phase 4, workspace states W0-W2).
//!
//! # What a pass does
//!
//! For every registered workspace it fetches the default branch, reads the
//! install metadata from that branch (never the working tree) and classifies:
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
//! never forced. At most one W2 per pass, so a tick's wall time is bounded.
//!
//! # Only from H0
//!
//! [`host_gate`] decides whether this host may claim or write at all. A host
//! that fails it still classifies and reports. The gate is asked twice: at
//! the start of the pass and again immediately before the push.
//!
//! # When it runs
//!
//! On the fleet-sync timer and once at startup (as soon as the startup pass
//! has finished and the drain state exists), from the daemon's own loop. It
//! writes only when `fleet.autoApply` is on, like every other timer write.
//! Nothing here schedules anything, and nothing waits for a roll window.

mod git;
mod host;

use std::cell::OnceCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};

use super::Mode;
use crate::fleet_store::resync_claim::{stale_after, Acquire, ClaimForge, Claimant, Held};
use crate::init::payload::{
    gate_metadata, materialize_with, resync_workspace_with, Payload, ResyncOutcome, ResyncRefusal,
};
use crate::install_compat::{Compat, DaemonCompat, InstallMeta, Version, SUPPORTS_INSTALLED};

pub use host::{host_gate, HostGateInputs, NotCurrent};
pub(super) use host::{mark_boot, mark_verified, pass};

/// Event-bus topic a resync failure that needs a person is published on.
pub const ALERT_TOPIC: &str = "fleet_sync.workspace_resync";

/// Ceiling of the per-repo backoff.
pub const BACKOFF_CAP: Duration = Duration::from_secs(6 * 60 * 60);

/// Consecutive failures (other than a protection rule) before an alert.
pub const ALERT_AFTER: u32 = 3;

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
    /// `branch-protection` or `failure`.
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
    /// Why this host only reported, when it did: it is not in H0, or its
    /// payload is not a release. A host-level fact, said once.
    pub host: Option<String>,
    /// Every registered workspace.
    pub workspaces: Vec<WorkspaceReport>,
    /// Failures to publish. Not part of the snapshot.
    #[serde(skip)]
    pub alerts: Vec<Alert>,
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
// What a host remembers between passes
// ============================================================================

#[derive(Debug, Clone)]
struct Backoff {
    failures: u32,
    next_attempt: DateTime<Utc>,
    state: WState,
    last_error: String,
}

#[derive(Debug, Clone)]
struct Verdict {
    commit: String,
    version: String,
    stale: bool,
}

/// Per-repo state kept in process memory. It resets on restart: a restarted
/// host re-evaluates every workspace from scratch on its first H0 tick.
#[derive(Debug, Default)]
pub struct Memory {
    backoff: HashMap<PathBuf, Backoff>,
    /// The diff verdict for a default-branch commit and a payload version, so
    /// an unchanged repo costs a fetch and no diff.
    verdicts: HashMap<PathBuf, Verdict>,
    /// Things said once per process.
    noted: HashSet<String>,
}

/// How a failure counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FailureKind {
    /// A rule on the remote refused the push. Retrying sooner cannot help.
    Protected,
    /// Anything else.
    Other,
}

impl Memory {
    /// The backoff delay after `failures` consecutive failures: one sync
    /// interval, doubling, capped at [`BACKOFF_CAP`].
    #[must_use]
    pub fn delay(interval: Duration, failures: u32) -> Duration {
        let doublings = failures.saturating_sub(1).min(20);
        interval.saturating_mul(1u32 << doublings).min(BACKOFF_CAP)
    }

    fn fail(
        &mut self,
        report: &WorkspaceReport,
        kind: FailureKind,
        detail: &str,
        interval: Duration,
        now: DateTime<Utc>,
    ) -> (DateTime<Utc>, Option<Alert>) {
        let failures = self
            .backoff
            .get(&report.root)
            .map_or(0, |b| b.failures)
            .saturating_add(1);
        let delay = match kind {
            FailureKind::Protected => BACKOFF_CAP,
            FailureKind::Other => Self::delay(interval, failures),
        };
        let next_attempt = now + chrono::Duration::from_std(delay).unwrap_or(chrono::Duration::MAX);
        self.backoff.insert(
            report.root.clone(),
            Backoff {
                failures,
                next_attempt,
                state: report.state,
                last_error: detail.to_string(),
            },
        );
        let alert = (kind == FailureKind::Protected || failures >= ALERT_AFTER).then(|| Alert {
            root: report.root.clone(),
            repo: report.repo.clone(),
            kind: match kind {
                FailureKind::Protected => "branch-protection",
                FailureKind::Other => "failure",
            },
            failures,
            detail: detail.to_string(),
            next_attempt,
        });
        (next_attempt, alert)
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
}

/// A stale workspace this pass may resync.
struct Candidate {
    index: usize,
    nwo: String,
    branch: String,
    commit: String,
}

/// Run one workspace pass over `roots`. Never fails: every failure lands in a
/// report, the backoff and (at the thresholds) an alert.
pub fn run(env: &Env<'_>, roots: &[PathBuf], mode: Mode, memory: &mut Memory) -> WorkspacePass {
    let gate = (env.gate)();
    let mut pass = WorkspacePass {
        running: env.running.to_string(),
        host: gate.err().map(|why| why.host_note()),
        ..WorkspacePass::default()
    };
    if gate == Err(NotCurrent::NotAReleaseBuild) && memory.noted.insert("release".into()) {
        log::warn!(
            "workspace_resync: this daemon is not a release build, so it will never resync a \
             workspace (reported once)"
        );
    }
    let mut candidates = Vec::new();
    for root in roots {
        let (report, found) = classify(env, root, gate, memory, &mut pass.alerts);
        if let Some((nwo, branch, commit)) = found {
            candidates.push(Candidate {
                index: pass.workspaces.len(),
                nwo,
                branch,
                commit,
            });
        }
        pass.workspaces.push(report);
    }
    if mode != Mode::Write || gate.is_err() {
        return pass;
    }
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
        if attempt(env, candidate, report, memory, &mut pass.alerts) {
            break;
        }
    }
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
    alerts: &mut Vec<Alert>,
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
    if let Some(b) = memory.backoff.get(root) {
        if b.next_attempt > (env.clock)() {
            report.state = b.state;
            report.reason =
                Some(format!("backoff until {}: {}", stamp(b.next_attempt), b.last_error));
            return (report, None);
        }
    }
    match classify_fetched(env, root, gate, memory, &mut report) {
        Ok(found) => (report, found.map(|(branch, commit)| (nwo, branch, commit))),
        Err(e) => {
            record_failure(env, &mut report, FailureKind::Other, &format!("{e:#}"), memory, alerts);
            (report, None)
        }
    }
}

/// The part of [`classify`] that can fail: fetch, read the default branch,
/// gate, diff. Returns the branch and commit of a stale workspace.
fn classify_fetched(
    env: &Env<'_>,
    root: &Path,
    gate: std::result::Result<(), NotCurrent>,
    memory: &mut Memory,
    report: &mut WorkspaceReport,
) -> Result<Option<(String, String)>> {
    let branch = git::default_branch(root)
        .ok_or_else(|| anyhow!("no default branch ref (origin/HEAD or origin/main)"))?;
    let commit = git::fetch(root, &branch)?;
    let Some(raw) = git::metadata_at(root, &commit)? else {
        report.state = WState::Skipped;
        report.reason = Some("Loom is not installed on the default branch".to_string());
        return Ok(None);
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
            return Ok(None);
        }
    };
    if gate == Err(NotCurrent::NotAReleaseBuild) {
        // Without a release payload there is nothing to diff against. The
        // reason is the host's, already said once on the pass.
        return Ok(None);
    }
    if !diff_is_stale(env, root, &commit, memory)? {
        report.state = WState::W0;
        if compat != Compat::Compatible {
            // The stamp is old or incomplete, the files are not. No resync
            // will ever rewrite it, so this is current, not owed.
            report.reason = Some("files match the payload; the stamp is left as is".to_string());
        }
        // A repo that reached W0 has nothing left to back off from.
        memory.backoff.remove(root);
        return Ok(None);
    }
    report.state = if compat == Compat::InstalledTooOld {
        WState::W3
    } else {
        WState::W1
    };
    Ok(Some((branch, commit)))
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
/// a resync could close? Cached per default-branch commit and payload version.
fn diff_is_stale(env: &Env<'_>, root: &Path, commit: &str, memory: &mut Memory) -> Result<bool> {
    let version = env.running.to_string();
    if let Some(v) = memory.verdicts.get(root) {
        if v.commit == commit && v.version == version {
            return Ok(v.stale);
        }
    }
    let payload = env.payload.get()?;
    let tree = tempfile::Builder::new()
        .prefix("loom-resync-tree-")
        .tempdir()?;
    git::export_surfaces(root, commit, tree.path())?;
    let diff = materialize_with(payload, tree.path())?;
    let stale = diff.stamp_pending() || {
        let paths: Vec<String> = diff
            .added
            .iter()
            .chain(&diff.changed)
            .chain(&diff.removed)
            .cloned()
            .collect();
        let ignored = git::ignored(root, root, &paths);
        paths.iter().any(|p| !ignored.contains(p))
    };
    remember(memory, root, commit, &version, stale);
    Ok(stale)
}

fn remember(memory: &mut Memory, root: &Path, commit: &str, version: &str, stale: bool) {
    memory.verdicts.insert(
        root.to_path_buf(),
        Verdict {
            commit: commit.to_string(),
            version: version.to_string(),
            stale,
        },
    );
}

/// How a resync under the claim ended.
enum Done {
    /// One commit is on the default branch.
    Pushed(String),
    /// Nothing left to do at this commit: another host or a person did it.
    AlreadyCurrent(String),
    /// The re-read found a state this host never writes to.
    NoWrite(WState, Option<String>),
    /// Stopped before the push, on purpose. Not a failure.
    Aborted(String),
    /// The branch moved on both tries. Next tick.
    Moved,
    /// A rule on the remote refused the push.
    Protected(String),
}

/// Try the claim for `candidate` and, if won, resync. Returns whether this
/// pass has used its one W2 (so the remaining candidates wait a tick).
fn attempt(
    env: &Env<'_>,
    candidate: &Candidate,
    report: &mut WorkspaceReport,
    memory: &mut Memory,
    alerts: &mut Vec<Alert>,
) -> bool {
    let root = report.root.clone();
    // Loom writes only to repositories it manages and can write (#9548). A
    // refusal is this host's standing, not a failure: another host may hold
    // a credential that can, so there is no backoff and no alert.
    if let Err(why) = (env.may_write)(&root, &candidate.nwo) {
        if memory
            .noted
            .insert(format!("scope:{}:{why}", root.display()))
        {
            log::warn!(
                "workspace_resync: {}: not resynced from this host: {why} (reported once)",
                candidate.nwo
            );
        }
        report.reason = Some(format!("not written from this host: {why}"));
        return false;
    }
    let forge = (env.forge)(&root, &candidate.nwo);
    let version = env.running.to_string();
    let claimant = Claimant {
        read: forge.as_read(),
        write: forge.as_write(),
        repo: &candidate.nwo,
        host: env.host,
        version: &version,
        stale_after: stale_after(env.interval),
    };
    let acquired = git::tree_of(&root, &candidate.commit)
        .and_then(|tree| claimant.acquire(&tree, (env.clock)()));
    let held = match acquired {
        Ok(Acquire::Won(held)) => held,
        Ok(Acquire::HeldBy { host, since }) => {
            log::debug!("workspace_resync: {}: claim held by {host}", candidate.nwo);
            report.reason = Some(format!("claim held by {host} since {}", stamp(since)));
            return false;
        }
        Ok(Acquire::Lost(why)) => {
            log::debug!("workspace_resync: {}: {why}", candidate.nwo);
            report.reason = Some(format!("claim not taken this tick: {why}"));
            return false;
        }
        Err(e) => {
            let detail = format!("claim: {e:#}");
            record_failure(env, report, FailureKind::Other, &detail, memory, alerts);
            return true;
        }
    };
    let outcome = resync_under_claim(env, candidate, &root, &claimant, &held);
    match claimant.release(&held) {
        Ok(_) => {}
        Err(e) => log::warn!(
            "workspace_resync: {}: could not release the claim ({e:#}); it expires on its own",
            candidate.nwo
        ),
    }
    match outcome {
        Ok(Done::Pushed(commit)) => {
            memory.backoff.remove(&root);
            log::info!(
                "workspace_resync: {}: resynced installed Loom to v{version} ({commit})",
                candidate.nwo
            );
            report.state = WState::W0;
            report.installed = Some(version.clone());
            report.reason = Some(format!("resynced to v{version} in {}", short(&commit)));
        }
        Ok(Done::AlreadyCurrent(commit)) => {
            memory.backoff.remove(&root);
            remember(memory, &root, &commit, &version, false);
            report.state = WState::W0;
            report.reason = Some("already current once the claim was held; no commit".to_string());
        }
        Ok(Done::NoWrite(state, reason)) => (report.state, report.reason) = (state, reason),
        Ok(Done::Aborted(why)) => {
            log::info!("workspace_resync: {}: resync abandoned: {why}", candidate.nwo);
            report.reason = Some(why);
        }
        Ok(Done::Moved) => {
            report.reason = Some("the default branch moved twice; retrying next tick".to_string());
        }
        Ok(Done::Protected(rule)) => {
            let detail = format!("push refused by a branch rule: {rule}");
            record_failure(env, report, FailureKind::Protected, &detail, memory, alerts);
        }
        Err(e) => {
            record_failure(env, report, FailureKind::Other, &format!("{e:#}"), memory, alerts);
        }
    }
    true
}

fn short(commit: &str) -> &str {
    &commit[..commit.len().min(12)]
}

/// W2, with the claim held: re-read, apply in a throwaway worktree, commit,
/// push. A moved branch is retried once.
fn resync_under_claim(
    env: &Env<'_>,
    candidate: &Candidate,
    root: &Path,
    claimant: &Claimant<'_>,
    held: &Held,
) -> Result<Done> {
    for _ in 0..2 {
        // Everything is read again now that the claim is ours: another host
        // or a person may have resynced it, or a newer host moved it ahead.
        let commit = git::fetch(root, &candidate.branch)?;
        let raw = git::metadata_at(root, &commit)?.ok_or_else(|| {
            anyhow!("the install metadata is gone from origin/{}", candidate.branch)
        })?;
        if let Err(refusal) = gate_metadata(&raw, &daemon(env)) {
            let (state, reason) = refused(&refusal);
            return Ok(Done::NoWrite(state, reason));
        }
        let payload = env.payload.get()?;
        git::clean_stale_worktrees(root);
        let worktree = git::add_worktree(root, &commit)?;
        let done = in_worktree(env, candidate, root, &worktree, &commit, payload, claimant, held);
        git::remove_worktree(root, &worktree);
        match done? {
            Done::Moved => {}
            other => return Ok(other),
        }
    }
    Ok(Done::Moved)
}

#[allow(clippy::too_many_arguments)] // one step of W2; each argument is a distinct input
fn in_worktree(
    env: &Env<'_>,
    candidate: &Candidate,
    root: &Path,
    worktree: &Path,
    commit: &str,
    payload: &Payload,
    claimant: &Claimant<'_>,
    held: &Held,
) -> Result<Done> {
    let written = match resync_workspace_with(payload, worktree)? {
        ResyncOutcome::Refused(refusal) => {
            let (state, reason) = refused(&refusal);
            return Ok(Done::NoWrite(state, reason));
        }
        ResyncOutcome::Unchanged => return Ok(Done::AlreadyCurrent(commit.to_string())),
        ResyncOutcome::Applied { written } => written,
    };
    let version = env.running;
    let message = format!(
        "chore(loom): resync installed Loom to v{version}\n\nLoom-Resync-Host: {}\n\
         Loom-Resync-Version: {version}\n",
        env.host
    );
    let Some(new) = git::commit(worktree, root, &written, &message)? else {
        return Ok(Done::AlreadyCurrent(commit.to_string()));
    };
    // The second gate check and the fence, immediately before the push. A
    // roll that started since the claim sets the drain flag; a claim that was
    // taken over is no longer ours.
    if let Err(why) = (env.gate)() {
        return Ok(Done::Aborted(format!("{} before the push", why.host_note())));
    }
    if let Err(why) = claimant.fence(held, (env.clock)())? {
        return Ok(Done::Aborted(format!("{why}; not pushing")));
    }
    Ok(match git::push(worktree, root, &candidate.branch)? {
        git::Push::Accepted => Done::Pushed(new),
        git::Push::NonFastForward => Done::Moved,
        git::Push::Protected(rule) => Done::Protected(rule),
    })
}

#[cfg(test)]
#[path = "tests/workspace_resync.rs"]
mod tests;
