//! The repo-ahead roll demand (#10719, tracker #10698 decision D1): a
//! registered workspace's installed Loom NEEDS a newer daemon than this one,
//! so this host is a roll candidate.
//!
//! The demand is [`crate::workspace_hold::repo_ahead_min`], rebuilt on every
//! workspace pass. It is raised only by a workspace whose `requires_daemon`
//! is above the running version (W4) or whose version cannot be ordered. A
//! repo that is merely ahead and still compatible raises nothing: that is the
//! ratchet guard (see [`crate::workspace_hold`]).
//!
//! It is a second floor and nothing more. [`RepoAheadState`] classifies it
//! with the floor's own [`floor_verdict`], and its target enters
//! [`super::select_target`] beside the floor target: no settle wait, the
//! exact tag, the same pause-and-roll, recorded as `target_source =
//! repo_ahead`.
//!
//! A demand no release satisfies (hand-edited metadata, a pre-release stamp)
//! arms nothing and pauses nothing. It is reported at ERROR, once per
//! distinct stall and then once per [`super::alert::REMINDER`]. The W4 hold
//! on that one workspace stands, because the pair really is incompatible;
//! every other workspace keeps dispatching.

use chrono::{DateTime, Utc};
use std::time::Duration;

use crate::fleet_sync::FloorKnowledge;

use super::{floor_verdict, FloorStallReport, FloorVerdict, Release, Target, TargetSource};

/// The demand in force: the version, and the workspace that asks for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Demand {
    /// The daemon version demanded, `X.Y.Z`.
    pub version: String,
    /// The workspace (`OWNER/REPO`, else its root) that needs it.
    pub workspace: String,
}

impl Demand {
    /// The demand the last workspace pass published, if any.
    #[must_use]
    pub fn live() -> Option<Self> {
        crate::fleet_sync::repo_ahead_min().map(|d| Self {
            version: d.version,
            workspace: d.repo.unwrap_or_else(|| d.root.display().to_string()),
        })
    }
}

/// The loop's repo-ahead bookkeeping: the basis set at the start of each
/// tick, and the last verdict. Inert while there is no demand.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepoAheadState {
    demand: Option<Demand>,
    running: String,
    verdict: FloorVerdict,
    /// The stall last logged at ERROR, and when.
    alerted: Option<(FloorStallReport, DateTime<Utc>)>,
}

impl RepoAheadState {
    /// Set this tick's basis. A changed basis drops the previous verdict, so
    /// a demand that goes away (the host rolled, or the repo was fixed)
    /// clears its stall at once.
    pub fn set_basis(&mut self, demand: Option<Demand>, running: &str) {
        if self.demand != demand || self.running != running {
            self.demand = demand;
            self.running = running.to_string();
            self.verdict = FloorVerdict::NoStore;
        }
    }

    /// Classify against this tick's newest release and return the target
    /// when a release satisfies the demand. A tick that cannot tell keeps a
    /// standing stall, as the floor does (#10866).
    pub fn observe(&mut self, newest: Option<&Release>) -> Option<Release> {
        // A demand is classified exactly as a floor set at its version; no
        // demand is no floor at all.
        let knowledge = self
            .demand
            .as_ref()
            .map_or(FloorKnowledge::NoStore, |d| FloorKnowledge::Set(d.version.clone()));
        let seen = floor_verdict(&knowledge, &self.running, newest);
        let keep_stall = matches!(seen, FloorVerdict::Unresolved { .. })
            && matches!(self.verdict, FloorVerdict::Unsatisfiable(_));
        if !keep_stall {
            self.verdict = seen;
        }
        match &self.verdict {
            FloorVerdict::Below { target, .. } => Some(target.clone()),
            _ => None,
        }
    }

    /// A release satisfies the demand: a roll decided now is repo-ahead
    /// driven unless the floor drives it too.
    #[must_use]
    pub fn driving(&self) -> bool {
        matches!(self.verdict, FloorVerdict::Below { .. })
    }

    fn workspace(&self) -> &str {
        self.demand.as_ref().map_or("", |d| d.workspace.as_str())
    }

    /// The standing unsatisfiable-demand stall, as its ERROR line.
    #[must_use]
    pub fn stall(&self) -> Option<String> {
        let FloorVerdict::Unsatisfiable(report) = &self.verdict else {
            return None;
        };
        let FloorStallReport {
            floor: demand,
            running,
            newest,
        } = report;
        Some(format!(
            "REPO-AHEAD DEMAND UNSATISFIABLE: workspace {} needs daemon {demand}, which is above \
             every published release (newest {newest}), so this host (running {running}) cannot \
             roll to it. Most likely hand-edited or pre-release install metadata. Nothing is \
             armed or paused for it. Dispatch into that workspace stays held (daemon-too-old); \
             every other workspace keeps dispatching. Fix the workspace's install metadata, or \
             publish a release at or above {demand}.",
            self.workspace()
        ))
    }

    /// Should this tick log the stall at ERROR? True when it starts or
    /// changes, then once per `reminder`.
    pub fn alert_due(&mut self, now: DateTime<Utc>, reminder: Duration) -> bool {
        let FloorVerdict::Unsatisfiable(report) = &self.verdict else {
            self.alerted = None;
            return false;
        };
        let quiet = self.alerted.as_ref().is_some_and(|(last, at)| {
            last == report && (now - *at).to_std().is_ok_and(|age| age < reminder)
        });
        if !quiet {
            self.alerted = Some((report.clone(), now));
        }
        !quiet
    }

    /// The suffix a repo-ahead fetch's reason carries, `""` otherwise.
    #[must_use]
    pub fn why_suffix(&self, target: &Target) -> String {
        match (&self.verdict, target.source) {
            (FloorVerdict::Below { floor: demand, .. }, TargetSource::RepoAhead) => format!(
                " [repo-ahead: workspace {} needs daemon {demand} and this host runs {}; rolling \
                 to the exact tag {} without waiting for settle]",
                self.workspace(),
                self.running,
                target.tag
            ),
            _ => String::new(),
        }
    }

    /// The suffix a tick's `status` note carries while a demand stands.
    #[must_use]
    pub fn note_suffix(&self) -> String {
        match &self.verdict {
            FloorVerdict::Below {
                floor: demand,
                target,
            } => format!(
                " [repo-ahead: workspace {} needs daemon {demand}, running {}; target {}]",
                self.workspace(),
                self.running,
                target.tag
            ),
            FloorVerdict::Unsatisfiable(_) => {
                format!(" [{}]", self.stall().unwrap_or_default())
            }
            FloorVerdict::Unresolved { floor: demand, .. } => format!(
                " [workspace {} needs daemon {demand} (running {}), but no comparable release \
                 resolved this tick to roll to]",
                self.workspace(),
                self.running
            ),
            _ => String::new(),
        }
    }
}
