//! The read-only host-update state projection (#12420, epic #10698).
//!
//! One pure classifier maps facts the update machinery already keeps (the
//! pause-and-roll H4/H5 state, the loop's backoff and terminal reason, the
//! running and on-disk versions, the roll target) to a host state. It changes
//! no target selection, pause/resume, `host_gate`, dispatch hold or resync
//! behavior, and starts no poll, timer or credential read: `loom-daemon
//! status` and the `auto_update.tick` record render what it returns.
//!
//! | State | Meaning |
//! |---|---|
//! | H0 current | on-disk binary is the running version and nothing newer is targeted |
//! | H1 settling | a newer target exists and the roll has not started |
//! | H2 fetching | a release artifact fetch is running now |
//! | H3 staged | a newer binary is on disk but not running |
//! | H4 pausing | the pause-and-roll is stopping agents at safe points |
//! | H5 verifying | the resume side of a pause-and-roll is in progress |
//! | H7 stalled | the loop is backing off or gave up terminally |
//! | unknown | the evidence needed to tell H0/H1/H3 apart is missing |
//!
//! There is no H6. Precedence when signals overlap: H4, H5, H2, H7, then the
//! on-disk comparison. An unreadable on-disk version is `unknown`, never
//! defaulted to the running version; the states that do not need it (H2, H4,
//! H5, H7) stand regardless.

use std::cmp::Ordering;

use serde::{Deserialize, Serialize};

use super::artifact_verdict::compare_versions;
use super::floor_roll::{repo_ahead::RepoAheadState, FloorState, TargetSource};
use super::pause_resume::PauseResumeStatus;
use super::{ArtifactResolution, AutoUpdateStatusSnapshot};

/// The host's place in the update lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HostState {
    /// H0: current.
    #[serde(rename = "H0")]
    Current,
    /// H1: a newer target exists, the roll has not started.
    #[serde(rename = "H1")]
    Settling,
    /// H2: fetching a release artifact.
    #[serde(rename = "H2")]
    Fetching,
    /// H3: a newer binary is staged on disk, not running.
    #[serde(rename = "H3")]
    Staged,
    /// H4: pausing agents for a roll.
    #[serde(rename = "H4")]
    Pausing,
    /// H5: verifying after a roll, before agents resume.
    #[serde(rename = "H5")]
    Verifying,
    /// H7: stalled or failed.
    #[serde(rename = "H7")]
    Stalled,
    /// Evidence is missing.
    #[serde(rename = "unknown")]
    Unknown,
}

impl HostState {
    /// The wire spelling (`H0`, …, `unknown`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Current => "H0",
            Self::Settling => "H1",
            Self::Fetching => "H2",
            Self::Staged => "H3",
            Self::Pausing => "H4",
            Self::Verifying => "H5",
            Self::Stalled => "H7",
            Self::Unknown => "unknown",
        }
    }

    /// The word shown next to the code in `status`.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::Settling => "settling",
            Self::Fetching => "fetching",
            Self::Staged => "staged",
            Self::Pausing => "pausing",
            Self::Verifying => "verifying",
            Self::Stalled => "stalled",
            Self::Unknown => "unknown",
        }
    }
}

/// Who would choose the next roll target.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ObservedTargetSource {
    /// The fleet floor.
    #[serde(rename = "floor")]
    Floor,
    /// A registered workspace needs a newer daemon (#10719).
    #[serde(rename = "repo-ahead")]
    RepoAhead,
    /// Ordinary opt-in autoUpdate on a host with no fleet store.
    #[serde(rename = "autoUpdate")]
    AutoUpdate,
    /// Nothing drives a roll.
    #[default]
    #[serde(rename = "none")]
    None,
}

impl ObservedTargetSource {
    /// The wire spelling (`floor`, `repo-ahead`, `autoUpdate`, `none`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Floor => "floor",
            Self::RepoAhead => "repo-ahead",
            Self::AutoUpdate => "autoUpdate",
            Self::None => "none",
        }
    }
}

/// What the update loop saw on its last tick, published with its status.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LoopFacts {
    /// The loop has completed a tick that read these facts.
    pub observed: bool,
    /// The installed binary's `--version`, when the probe read it.
    pub on_disk_version: Option<String>,
    /// The fleet floor in force, when one is set.
    pub floor: Option<String>,
    /// The version a roll would go to, when it is newer than the running one.
    pub target_version: Option<String>,
    /// Who would choose that target.
    pub target_source: ObservedTargetSource,
    /// A release artifact fetch is running now.
    pub fetching: bool,
}

impl LoopFacts {
    /// The facts of a finished tick. `fetching` is never set here: only the
    /// fetch itself raises it ([`super::AutoUpdateStatus::set_fetching`]).
    #[must_use]
    pub fn of_tick(
        artifact: &ArtifactResolution,
        floor: &FloorState,
        repo_ahead: &RepoAheadState,
        running: &str,
    ) -> Self {
        let (resolved, on_disk_version) = match artifact {
            ArtifactResolution::Resolved(info) => {
                (Some(info.version.clone()), info.on_disk_version.clone())
            }
            ArtifactResolution::Unresolved(_) => (None, None),
        };
        let target_source = if floor.roll_source() == TargetSource::Floor {
            ObservedTargetSource::Floor
        } else if repo_ahead.driving() {
            ObservedTargetSource::RepoAhead
        } else if !floor.fleet_host() {
            ObservedTargetSource::AutoUpdate
        } else {
            ObservedTargetSource::None
        };
        let target_version = resolved
            .filter(|_| target_source != ObservedTargetSource::None)
            .filter(|v| compare_versions(v, running) == Ordering::Greater);
        Self {
            observed: true,
            on_disk_version,
            floor: floor.floor().map(str::to_string),
            target_version,
            target_source,
            fetching: false,
        }
    }
}

/// Everything the classifier reads.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Facts {
    /// The version this process runs.
    pub running_version: String,
    /// What the loop last saw.
    pub loop_facts: LoopFacts,
    /// The loop is backing off a retryable failure.
    pub backing_off: bool,
    /// The loop gave up on a non-retryable failure.
    pub terminal: bool,
    /// The H4 pause is running.
    pub pausing: bool,
    /// The H5 resume is in progress (not a finished manifest).
    pub verifying: bool,
}

/// Whether `status` is an H5 run still in progress. A finished manifest stays
/// in the status after `done`/`unreadable`; it is history, not the host's
/// current state.
#[must_use]
pub fn is_verifying(status: &PauseResumeStatus) -> bool {
    status.finished_at.is_none()
        && matches!(
            status.step.as_str(),
            "hold" | "residue" | "lease" | "probation" | "resume" | "requeue"
        )
}

/// Map `facts` to a host state.
#[must_use]
pub fn classify(facts: &Facts) -> HostState {
    if facts.pausing {
        return HostState::Pausing;
    }
    if facts.verifying {
        return HostState::Verifying;
    }
    if facts.loop_facts.fetching {
        return HostState::Fetching;
    }
    if facts.terminal || facts.backing_off {
        return HostState::Stalled;
    }
    let Some(disk) = facts.loop_facts.on_disk_version.as_deref() else {
        return HostState::Unknown;
    };
    match compare_versions(disk, &facts.running_version) {
        Ordering::Greater => HostState::Staged,
        Ordering::Less => HostState::Unknown,
        Ordering::Equal if facts.loop_facts.target_version.is_some() => HostState::Settling,
        Ordering::Equal => HostState::Current,
    }
}

/// The host-update line's data: the state and the versions it was drawn from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostUpdateObservation {
    /// The classified state.
    pub state: HostState,
    /// The version this process runs.
    pub running_version: String,
    /// The installed binary's version; `None` is unreadable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_disk_version: Option<String>,
    /// The fleet floor, when one is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub floor: Option<String>,
    /// The version a roll would go to, when there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_version: Option<String>,
    /// Who would choose the target.
    pub target_source: ObservedTargetSource,
}

/// Observe the host from the loop's published status, the resume side's
/// status and whether the H4 pause is running.
#[must_use]
pub fn observe(
    snapshot: &AutoUpdateStatusSnapshot,
    resume: Option<&PauseResumeStatus>,
    pausing: bool,
    running_version: &str,
) -> HostUpdateObservation {
    let loop_facts = snapshot.observation.clone();
    let facts = Facts {
        running_version: running_version.to_string(),
        loop_facts: loop_facts.clone(),
        backing_off: snapshot.backoff_secs.is_some(),
        terminal: snapshot.terminal_reason.is_some(),
        pausing,
        verifying: resume.is_some_and(is_verifying),
    };
    HostUpdateObservation {
        state: classify(&facts),
        running_version: facts.running_version,
        on_disk_version: loop_facts.on_disk_version,
        floor: loop_facts.floor,
        target_version: loop_facts.target_version,
        target_source: loop_facts.target_source,
    }
}

#[cfg(test)]
#[path = "update_observation_tests.rs"]
mod tests;
