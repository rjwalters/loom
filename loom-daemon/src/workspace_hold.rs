//! Per-workspace dispatch holds for workspace states W3 and W4, and the
//! `repo_ahead_target` roll demand (#10719, tracker #10698 decision D1).
//!
//! Dispatch runs each repo's own installed files: agent worktrees start from
//! the default branch, and the spawn script comes from the host's checkout.
//! When either copy cannot work with this daemon, new dispatch into that one
//! workspace is held until the pair is compatible again.
//!
//! # Every way the daemon starts work in a workspace
//!
//! | Path | Where it is refused |
//! |---|---|
//! | Sweeps: work finder, IPC/MCP `DispatchSweep`, epic child sweeps, watchdog re-dispatch, crash resume | [`guard`], in the registry (issue and PR-set dispatch) |
//! | Work-finder selection, the red-main fix lane, the pre-flight recovery probe | the per-root pre-filter (`work_finder::pool_preflight`) |
//! | Role runner, interval ticks | [`filter_held`] |
//! | Role runner, idle-edge (`onIdle`) runs | [`refuse_role_start`] |
//! | Epic supervisor: its whole tick, and its singleton roles (Architect, Champion) | the tick skips a held root; [`guard`] again in its `dispatch_role` |
//!
//! Not refused, on purpose: the pause-and-roll resume of sweeps and role runs
//! (work that was already in flight when the host rolled), the resync itself,
//! and terminals a person opens over IPC.
//!
//! # What is held
//!
//! | Verdict for a copy | Hold | Roll demand |
//! |---|---|---|
//! | its `requires_daemon` is above this daemon (W4) | `daemon-too-old` | yes |
//! | a contract field that cannot be ordered (`0.20.0-rc1`) | `daemon-too-old` | yes |
//! | too old for this daemon or the floor, files differ (W3) | `install-incompatible` | no |
//! | a resync to a release above this daemon was interrupted there | `install-incompatible` | no |
//! | too old by its stamp, files equal the payload | none (W0) | no |
//! | installed by a newer daemon, `requires_daemon` at or below this one | none | no |
//! | compatible, or a resync owed | none | no |
//! | could not be read this pass | the previous verdict | as before |
//!
//! # The ratchet guard
//!
//! A repo that is merely *ahead* of this daemon is not a reason to roll and
//! not a reason to hold. Hosts roll at different moments, so a host that has
//! just rolled resyncs repos to a release the others do not run yet. If that
//! alone made the others roll, one straggler would pull the whole fleet
//! forward a release at a time. The compatibility contract
//! ([`crate::install_compat`]) says whether the newer files still work here:
//! while their `requires_daemon` is at or below this daemon, this host keeps
//! working the repo, never resyncs it downward, and only reports the state.
//! Only a copy that *needs* a newer daemon (or whose version cannot be
//! ordered, so it may) holds dispatch and raises [`repo_ahead_min`].
//!
//! An interrupted resync to a newer release is held too, and asks for no
//! roll. `requires_daemon` is stamped last, so the half-applied files are a
//! mix of two releases whose compatibility nobody recorded. The hold clears
//! when a host on that release (or this one, once it has rolled for another
//! reason) finishes the resync. Raising a roll demand from it would be the
//! ratchet again: it names a release, not a need.
//!
//! # Scope of a hold
//!
//! A hold stops **new** dispatch into **one** workspace. It never pauses the
//! host, never touches in-flight sweeps or role runs, and does not stop the
//! workspace resync. IPC `force` does not bypass it: `force` overrides timing
//! gates, and this is a structural refusal like the workspace-commands guard.
//!
//! The holds are rebuilt on every workspace pass (the fleet-sync timer), so
//! one clears on the first pass whose verdict for both copies is clean.
//! Every set and clear is logged and published on [`TOPIC`]. A hold that has
//! stood for [`ALERT_AFTER`] is logged at ERROR and published as `standing`,
//! and again every [`ALERT_AFTER`] for as long as it stands.
//!
//! A copy that cannot be read keeps its last verdict, so a hold can outlive
//! the evidence for it. Each hold carries when a pass last judged its copy
//! ([`WorkspaceHold::verdict_at`]); the repeated ERROR and the status row say
//! so when that is no longer the latest pass.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, RwLock};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::init::payload::ResyncRefusal;
use crate::install_compat::{Compat, Version};
use crate::work_finder::halt_cause::HaltCause;

/// Event-bus topic hold transitions (`set`, `cleared`, `standing`) are
/// published on. Transitions only, never once per pass.
pub const TOPIC: &str = "fleet_sync.workspace_hold";

/// How long a hold stands before it alerts, and how often the alert repeats
/// while it still stands. A starting value.
pub const ALERT_AFTER: Duration = Duration::from_secs(30 * 60);

/// Why a workspace's dispatch is held.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HoldKind {
    /// W3: the installed files are too old for this daemon or the fleet
    /// floor, and differ from this daemon's payload. Also a resync to a newer
    /// release that was interrupted, leaving files from two releases. Either
    /// way a resync clears it, and it asks for no roll.
    InstallIncompatible,
    /// W4: the installed files need a newer daemon than this one, or record a
    /// version that cannot be ordered against it.
    DaemonTooOld,
}

impl HoldKind {
    /// The typed outcome a refused dispatch carries.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InstallIncompatible => "install-incompatible",
            Self::DaemonTooOld => "daemon-too-old",
        }
    }

    /// The cause a `workspace_halted` queue row names.
    #[must_use]
    pub fn halt_cause(self) -> HaltCause {
        match self {
            Self::InstallIncompatible => HaltCause::InstallIncompatible,
            Self::DaemonTooOld => HaltCause::DaemonTooOld,
        }
    }
}

/// Which copy of the installed files a hold is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HeldCopy {
    /// The default branch: what a new agent worktree starts from.
    DefaultBranch,
    /// The host's checkout: where the spawn script and role runs come from.
    Checkout,
}

impl HeldCopy {
    /// The label `loom-daemon status` prints.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DefaultBranch => "default-branch",
            Self::Checkout => "checkout",
        }
    }
}

/// A standing hold on one workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceHold {
    /// Why.
    pub kind: HoldKind,
    /// Which copy. When both are incompatible this is the one that decided
    /// the kind (W4 before W3, then the default branch).
    pub copy: HeldCopy,
    /// Since when the workspace has been held for this kind.
    pub since: DateTime<Utc>,
    /// The versions involved, for a person.
    pub detail: String,
    /// When a pass last judged the deciding copy held. Equal to the latest
    /// pass unless the copy has been unreadable since, or passes stopped.
    pub verdict_at: DateTime<Utc>,
}

impl WorkspaceHold {
    /// `hold verdict is N minutes old`, once no pass has judged the copy for
    /// longer than `fresh` (a few pass intervals). `None` while it is fresh.
    #[must_use]
    pub fn stale_note(&self, now: DateTime<Utc>, fresh: Duration) -> Option<String> {
        let age = now.signed_duration_since(self.verdict_at);
        let fresh = chrono::Duration::from_std(fresh).ok()?;
        (age > fresh).then(|| format!("hold verdict is {} minutes old", age.num_minutes()))
    }

    /// `held: <kind> (<copy>) since <time>`, for a status line.
    #[must_use]
    pub fn note(&self) -> String {
        format!(
            "held: {} ({}) since {}",
            self.kind.as_str(),
            self.copy.as_str(),
            self.since.format("%Y-%m-%dT%H:%M:%SZ")
        )
    }
}

/// What one pass concluded about one copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The copy could not be read or judged this pass. Never a hold by
    /// itself: the previous verdict stands.
    Unknown,
    /// Not held.
    Clear,
    /// Held.
    Hold(HoldKind),
}

/// The hold decision for one copy. Pure.
///
/// `gate` is [`crate::init::payload::gate_metadata`]'s answer for the copy's
/// install metadata. `diff_stale` says whether this daemon's payload differs
/// from the copy's installed files; it is only consulted for
/// [`Compat::InstalledTooOld`], and `None` means it is not known.
///
/// The W3 hold keys on the payload diff, not on the stamp. A resync that
/// changes no file writes no stamp, so a copy whose files already equal the
/// payload keeps an old `loom_version` forever. Its files are current, so it
/// is W0, and a hold keyed on the stamp alone would never clear.
#[must_use]
pub fn decide_hold(gate: &Result<Compat, ResyncRefusal>, diff_stale: Option<bool>) -> Verdict {
    match gate {
        // Needs a newer daemon, or cannot be ordered so it may.
        Err(ResyncRefusal::NeedsNewerDaemon { .. } | ResyncRefusal::UnrecognizedVersion { .. }) => {
            Verdict::Hold(HoldKind::DaemonTooOld)
        }
        // The ratchet guard: ahead, and its `requires_daemon` is at or below
        // this daemon (or it would be `NeedsNewerDaemon`). Compatible.
        Err(ResyncRefusal::RepoAheadOfDaemon { .. }) => Verdict::Clear,
        // A newer release is half applied: files from two releases, and the
        // `requires_daemon` on record is the old one. Held, with no demand
        // ([`Finding::install_incompatible`] never carries one).
        Err(ResyncRefusal::PendingAheadOfDaemon { .. }) => {
            Verdict::Hold(HoldKind::InstallIncompatible)
        }
        Err(ResyncRefusal::UnreadableMetadata(_)) => Verdict::Unknown,
        Err(
            ResyncRefusal::NotInstalled
            | ResyncRefusal::LoomSourceRepo
            | ResyncRefusal::NotAReleaseBuild,
        ) => Verdict::Clear,
        Ok(Compat::InstalledTooOld) => match diff_stale {
            Some(true) => Verdict::Hold(HoldKind::InstallIncompatible),
            Some(false) => Verdict::Clear,
            None => Verdict::Unknown,
        },
        // `NeedsNewerDaemon` never arrives as `Ok`: the gate refuses it.
        Ok(Compat::NeedsNewerDaemon) => Verdict::Hold(HoldKind::DaemonTooOld),
        Ok(Compat::Compatible | Compat::ResyncOwed) => Verdict::Clear,
    }
}

/// The daemon version a `daemon-too-old` copy asks for: its `requires_daemon`
/// when that is a version above `running`, else "any release after this
/// one" (a version that cannot be ordered names no release to wait for).
#[must_use]
pub fn demanded(requires_daemon: Option<&str>, running: Version) -> Version {
    requires_daemon
        .and_then(Version::parse)
        .filter(|r| *r > running)
        .unwrap_or_else(|| running.next_patch())
}

/// One copy's verdict, with what a person and the roll need to know.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// The verdict.
    pub verdict: Verdict,
    /// The versions involved. Empty unless held.
    pub detail: String,
    /// The daemon version a `daemon-too-old` copy asks for.
    pub demand: Option<Version>,
}

impl Finding {
    /// Could not be judged this pass.
    #[must_use]
    pub fn unknown() -> Self {
        Self::of(Verdict::Unknown)
    }

    /// Not held.
    #[must_use]
    pub fn clear() -> Self {
        Self::of(Verdict::Clear)
    }

    fn of(verdict: Verdict) -> Self {
        Self {
            verdict,
            detail: String::new(),
            demand: None,
        }
    }

    /// A W3 hold, or an interrupted newer resync. Never a roll demand.
    #[must_use]
    pub fn install_incompatible(detail: String) -> Self {
        Self {
            verdict: Verdict::Hold(HoldKind::InstallIncompatible),
            detail,
            demand: None,
        }
    }

    /// A W4 hold asking for a daemon at or above [`demanded`].
    #[must_use]
    pub fn daemon_too_old(detail: String, requires_daemon: Option<&str>, running: Version) -> Self {
        Self {
            verdict: Verdict::Hold(HoldKind::DaemonTooOld),
            detail,
            demand: Some(demanded(requires_daemon, running)),
        }
    }
}

/// What one pass found for one workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    /// The registered root.
    pub root: PathBuf,
    /// `OWNER/REPO`, when known.
    pub repo: Option<String>,
    /// The default-branch copy.
    pub default_branch: Finding,
    /// The checkout copy.
    pub checkout: Finding,
}

/// The highest daemon version any registered workspace needs and this host
/// does not run: the `repo_ahead_target` roll demand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoAheadDemand {
    /// The version demanded, `X.Y.Z`.
    pub version: String,
    /// The workspace that demands it.
    pub root: PathBuf,
    /// `OWNER/REPO`, when known.
    pub repo: Option<String>,
    /// Which copy.
    pub copy: HeldCopy,
}

/// A hold was set, cleared, or has stood for another [`ALERT_AFTER`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Transition {
    /// `set`, `cleared` or `standing`.
    pub event: &'static str,
    /// The registered root.
    pub root: PathBuf,
    /// `OWNER/REPO`, when known.
    pub repo: Option<String>,
    /// The hold (for `cleared`, the one that ended).
    #[serde(flatten)]
    pub hold: WorkspaceHold,
    /// The daemon version asked for, for a `daemon-too-old` hold.
    pub demand: Option<String>,
    /// The version this daemon runs.
    pub running: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Standing {
    kind: HoldKind,
    detail: String,
    demand: Option<Version>,
    /// The pass that last judged the copy held.
    seen: DateTime<Utc>,
}

#[derive(Debug, Clone)]
struct Entry {
    repo: Option<String>,
    /// The last known verdict per copy: default branch, then checkout.
    copies: [Option<Standing>; 2],
    since: DateTime<Utc>,
    /// When the standing alert last fired for this hold.
    alerted: Option<DateTime<Utc>>,
}

impl Entry {
    /// The copy that decides: W4 before W3, then the default branch.
    fn deciding(&self) -> Option<(HeldCopy, &Standing)> {
        let [default_branch, checkout] = &self.copies;
        let found = [
            (HeldCopy::DefaultBranch, default_branch.as_ref()),
            (HeldCopy::Checkout, checkout.as_ref()),
        ];
        let pick = |kind| {
            found
                .iter()
                .find_map(|(copy, s)| s.filter(|s| s.kind == kind).map(|s| (*copy, s)))
        };
        pick(HoldKind::DaemonTooOld).or_else(|| pick(HoldKind::InstallIncompatible))
    }

    fn hold(&self) -> Option<WorkspaceHold> {
        self.deciding().map(|(copy, standing)| WorkspaceHold {
            kind: standing.kind,
            copy,
            since: self.since,
            detail: standing.detail.clone(),
            verdict_at: standing.seen,
        })
    }

    /// The highest demand over both copies.
    fn demand(&self) -> Option<(Version, HeldCopy)> {
        let [default_branch, checkout] = &self.copies;
        [(default_branch, HeldCopy::DefaultBranch), (checkout, HeldCopy::Checkout)]
            .into_iter()
            .filter_map(|(s, copy)| Some((s.as_ref()?.demand?, copy)))
            // `max_by_key` keeps the last of equals; the default branch is first.
            .rev()
            .max_by_key(|(version, _)| *version)
    }
}

/// The hold state one pass leaves for the next. Pure: [`Holds::step`] is the
/// whole state machine, and the process-wide copy lives behind [`apply`].
#[derive(Debug, Default)]
pub struct Holds {
    entries: HashMap<PathBuf, Entry>,
}

impl Holds {
    /// Fold one pass's observations in. A copy whose verdict is
    /// [`Verdict::Unknown`] keeps its previous one; a workspace that is not
    /// observed at all (it left the registry) is dropped.
    pub fn step(
        &mut self,
        observations: &[Observation],
        running: Version,
        now: DateTime<Utc>,
        alert_after: Duration,
    ) -> Vec<Transition> {
        let mut previous = std::mem::take(&mut self.entries);
        let mut transitions = Vec::new();
        let event =
            |event, root: &Path, repo: &Option<String>, hold, demand: Option<Version>| Transition {
                event,
                root: root.to_path_buf(),
                repo: repo.clone(),
                hold,
                demand: demand.map(|v| v.to_string()),
                running: running.to_string(),
            };
        for seen in observations {
            let root = normalize(&seen.root);
            let before = previous.remove(&root);
            let was = before.as_ref().and_then(Entry::hold);
            let was_demand = before.as_ref().and_then(Entry::demand).map(|(v, _)| v);
            let [old_default, old_checkout] = before
                .as_ref()
                .map_or([None, None], |entry| entry.copies.clone());
            let keep = |finding: &Finding, old: Option<Standing>| match finding.verdict {
                Verdict::Unknown => old,
                Verdict::Clear => None,
                Verdict::Hold(kind) => Some(Standing {
                    kind,
                    detail: finding.detail.clone(),
                    demand: finding.demand,
                    seen: now,
                }),
            };
            let mut entry = Entry {
                repo: seen.repo.clone(),
                copies: [
                    keep(&seen.default_branch, old_default),
                    keep(&seen.checkout, old_checkout),
                ],
                since: now,
                alerted: None,
            };
            let Some((copy, standing)) = entry.deciding().map(|(c, s)| (c, s.clone())) else {
                if let Some(was) = was {
                    transitions.push(event("cleared", &root, &seen.repo, was, was_demand));
                }
                continue;
            };
            // The clock runs for as long as the workspace is held for the
            // same reason; a W3 hold that becomes W4 starts a new one.
            if let Some(was) = was.as_ref().filter(|was| was.kind == standing.kind) {
                entry.since = was.since;
                entry.alerted = before.as_ref().and_then(|b| b.alerted);
            }
            let demand = entry.demand().map(|(v, _)| v);
            if let Some(hold) = entry.hold() {
                let changed = was
                    .as_ref()
                    .is_none_or(|was| (was.kind, was.copy) != (standing.kind, copy));
                // Due once it has stood for `alert_after`, and then again
                // every `alert_after` since the last alert: a hold nothing
                // clears must not go quiet.
                let last = entry.alerted.unwrap_or(hold.since);
                let due = chrono::Duration::from_std(alert_after)
                    .is_ok_and(|after| now.signed_duration_since(last) >= after);
                if changed {
                    transitions.push(event("set", &root, &seen.repo, hold.clone(), demand));
                }
                if due {
                    entry.alerted = Some(now);
                    transitions.push(event("standing", &root, &seen.repo, hold, demand));
                }
            }
            self.entries.insert(root, entry);
        }
        for (root, gone) in previous {
            if let Some(was) = gone.hold() {
                let demand = gone.demand().map(|(v, _)| v);
                transitions.push(event("cleared", &root, &gone.repo, was, demand));
            }
        }
        transitions
    }

    /// Every standing hold, by normalized root.
    #[must_use]
    pub fn holds(&self) -> HashMap<PathBuf, WorkspaceHold> {
        self.entries
            .iter()
            .filter_map(|(root, entry)| Some((root.clone(), entry.hold()?)))
            .collect()
    }

    /// The roll demand: the highest version any copy of any workspace asks
    /// for. `None` when no workspace needs a newer daemon.
    #[must_use]
    pub fn demand(&self) -> Option<RepoAheadDemand> {
        self.entries
            .iter()
            .filter_map(|(root, entry)| {
                let (version, copy) = entry.demand()?;
                Some((version, root, entry, copy))
            })
            // Ties go to the lowest root, so the answer is stable.
            .max_by(|a, b| a.0.cmp(&b.0).then_with(|| b.1.cmp(a.1)))
            .map(|(version, root, entry, copy)| RepoAheadDemand {
                version: version.to_string(),
                root: root.clone(),
                repo: entry.repo.clone(),
                copy,
            })
    }
}

// ============================================================================
// The process-wide holds dispatch reads
// ============================================================================

/// The key a root is held under: its canonical path when it has one.
fn normalize(root: &Path) -> PathBuf {
    std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf())
}

fn state() -> &'static Mutex<Holds> {
    static STATE: OnceLock<Mutex<Holds>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(Holds::default()))
}

fn published() -> &'static RwLock<HashMap<PathBuf, WorkspaceHold>> {
    static PUBLISHED: OnceLock<RwLock<HashMap<PathBuf, WorkspaceHold>>> = OnceLock::new();
    PUBLISHED.get_or_init(|| RwLock::new(HashMap::new()))
}

fn demand_cell() -> &'static Mutex<Option<RepoAheadDemand>> {
    static DEMAND: OnceLock<Mutex<Option<RepoAheadDemand>>> = OnceLock::new();
    DEMAND.get_or_init(|| Mutex::new(None))
}

/// The hold standing on `root`, if any. Cheap: no hold anywhere costs one
/// read lock and no syscall.
#[must_use]
pub fn hold_for(root: &Path) -> Option<WorkspaceHold> {
    let holds = published()
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if holds.is_empty() {
        return None;
    }
    holds
        .get(root)
        .or_else(|| holds.get(&normalize(root)))
        .cloned()
}

/// The `repo_ahead_target` roll demand as of the last workspace pass: the
/// highest daemon version a registered workspace needs and this host does
/// not run. `None` when there is none, and until a pass has run.
///
/// Only a workspace that NEEDS a newer daemon contributes (see the module
/// docs, "The ratchet guard"). The self-update loop reads it every tick and
/// treats it like the fleet floor ([`crate::auto_update::floor_roll`]).
#[must_use]
pub fn repo_ahead_min() -> Option<RepoAheadDemand> {
    demand_cell().lock().ok().and_then(|g| g.clone())
}

/// Fold one workspace pass into the process-wide holds, publish them for
/// dispatch, and report every transition (log, and the event bus when there
/// is one). Returns the holds now standing, by observed root.
pub fn apply(
    observations: &[Observation],
    running: Version,
    now: DateTime<Utc>,
    bus: Option<&crate::event_bus::EventBus>,
) -> HashMap<PathBuf, WorkspaceHold> {
    let mut holds = state()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let transitions = holds.step(observations, running, now, ALERT_AFTER);
    let standing = holds.holds();
    let demand = holds.demand();
    drop(holds);
    if let Ok(mut cell) = published().write() {
        cell.clone_from(&standing);
    }
    if let Ok(mut cell) = demand_cell().lock() {
        *cell = demand;
    }
    for t in &transitions {
        let name = t
            .repo
            .clone()
            .unwrap_or_else(|| t.root.display().to_string());
        let (kind, copy, detail) = (t.hold.kind.as_str(), t.hold.copy.as_str(), &t.hold.detail);
        match t.event {
            "set" => log::warn!(
                "workspace_hold: {name}: new dispatch held ({kind}, {copy} copy): {detail}. \
                 In-flight work is not touched and no other workspace is affected"
            ),
            "cleared" => log::info!(
                "workspace_hold: {name}: hold cleared ({kind}, {copy} copy); dispatch resumes"
            ),
            _ => log::error!(
                "workspace_hold: {name}: dispatch has been held for {} minutes ({kind}, {copy} \
                 copy): {detail}.{} Nothing has cleared it: check for a protected branch that \
                 blocks the resync, a checkout that cannot fast-forward, an interrupted resync a \
                 newer host has to finish, or a requires_daemon no release satisfies. This \
                 repeats every {} minutes while the hold stands",
                now.signed_duration_since(t.hold.since).num_minutes(),
                unread_note(&t.hold, now),
                ALERT_AFTER.as_secs() / 60
            ),
        }
        if let Some(bus) = bus {
            let payload = serde_json::to_value(t).unwrap_or(serde_json::Value::Null);
            let _ = bus.publish_generic(TOPIC, payload);
        }
    }
    observations
        .iter()
        .filter_map(|seen| Some((seen.root.clone(), standing.get(&normalize(&seen.root))?.clone())))
        .collect()
}

/// For the standing alert: says when this pass could not read the copy, so
/// the hold stands on an older verdict. Empty when this pass judged it.
fn unread_note(hold: &WorkspaceHold, now: DateTime<Utc>) -> String {
    if hold.verdict_at >= now {
        return String::new();
    }
    format!(
        " This pass could not read the {} copy: the hold stands on the verdict of {} ({} minutes \
         ago).",
        hold.copy.as_str(),
        hold.verdict_at.format("%Y-%m-%dT%H:%M:%SZ"),
        now.signed_duration_since(hold.verdict_at).num_minutes()
    )
}

// ============================================================================
// The dispatch side
// ============================================================================

/// Typed, matchable error [`crate::sweep_registry::SweepRegistry`] returns
/// when a dispatch is refused because its workspace is held. A structural,
/// workspace-level refusal like
/// [`crate::sweep_registry::WorkspaceCommandsMissingDispatchError`]: every
/// dispatch into the workspace is refused identically until the hold clears,
/// and IPC `force` does not bypass it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceHeldDispatchError {
    /// The workspace root refusing every dispatch.
    pub workspace: PathBuf,
    /// Why: `install-incompatible` or `daemon-too-old`.
    pub kind: HoldKind,
    /// The versions involved.
    pub detail: String,
}

impl std::fmt::Display for WorkspaceHeldDispatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let remedy = match self.kind {
            HoldKind::InstallIncompatible => {
                "it clears when the workspace's installed Loom is resynced"
            }
            HoldKind::DaemonTooOld => "it clears when this host has rolled to a newer daemon",
        };
        write!(
            f,
            "refusing to dispatch: workspace {} is held ({}): {}. New dispatch only; {remedy}. \
             `force` does not override this (#10719)",
            self.workspace.display(),
            self.kind.as_str(),
            self.detail
        )
    }
}

impl std::error::Error for WorkspaceHeldDispatchError {}

/// The registry guard: refuse a dispatch into a held workspace. Runs before
/// any lock, label flip or forge call.
///
/// # Errors
/// The typed refusal, when `root` is held.
pub fn guard(root: &Path) -> Result<(), WorkspaceHeldDispatchError> {
    match hold_for(root) {
        Some(hold) => Err(WorkspaceHeldDispatchError {
            workspace: root.to_path_buf(),
            kind: hold.kind,
            detail: hold.detail,
        }),
        None => Ok(()),
    }
}

/// Drop held roots from a role runner's tick, logging each hold once when it
/// starts and once when it ends. `logged` carries the roots currently known
/// held across ticks.
pub fn filter_held(
    roots: Vec<PathBuf>,
    logged: &mut std::collections::HashSet<PathBuf>,
) -> Vec<PathBuf> {
    let mut free = Vec::with_capacity(roots.len());
    let mut held = std::collections::HashSet::new();
    for root in roots {
        let Some(hold) = hold_for(&root) else {
            if logged.contains(&root) {
                log::info!(
                    "role_runner: workspace {} is no longer held; role ticks resume",
                    root.display()
                );
            }
            free.push(root);
            continue;
        };
        if !logged.contains(&root) {
            log::warn!(
                "role_runner: workspace {} is held ({}, {} copy): {}; no role tick starts there \
                 until it clears (running roles are not touched)",
                root.display(),
                hold.kind.as_str(),
                hold.copy.as_str(),
                hold.detail
            );
        }
        held.insert(root);
    }
    *logged = held;
    free
}

/// Whether a role run that is about to start in `root` must not, because the
/// workspace is held. For the starts [`filter_held`] does not see: the
/// idle-edge (`onIdle`) runs, which the work finder fires per root. A hold
/// stops new sweeps, so the workspace drains and goes idle; without this the
/// hold itself would start every `onIdle` role in the held checkout.
///
/// Logged each time it refuses. Idle edges are rare, and the edge is spent:
/// the role fires on the next idle edge after the hold clears.
#[must_use]
pub fn refuse_role_start(root: &Path, what: &str) -> bool {
    let Some(hold) = hold_for(root) else {
        return false;
    };
    log::info!(
        "role_runner: {what} for {} suppressed: the workspace is held ({}, {} copy): {} (#10719)",
        root.display(),
        hold.kind.as_str(),
        hold.copy.as_str(),
        hold.detail
    );
    true
}

/// Put a hold on `root` (or lift it) directly, for tests of the dispatch
/// side. Keyed by root, so tests in one process do not disturb each other.
#[cfg(test)]
pub(crate) fn set_for_test(root: &Path, hold: Option<WorkspaceHold>) {
    let mut holds = published()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match hold {
        Some(hold) => holds.insert(normalize(root), hold),
        None => holds.remove(&normalize(root)),
    };
}

#[cfg(test)]
mod tests;
