//! `pick.decision` (#10212): what a role (or the work finder) looked at on one
//! tick, in the order it ranked it, what it acted on, and why it skipped the
//! rest.
//!
//! A queue-aware ETA needs each item's position **as the serving role sees
//! it**. Nothing else records that, and it cannot be reconstructed after the
//! fact. One record is emitted per role tick (including empty ticks, so the
//! service cadence per host is measurable) and per work-finder tick.
//!
//! **OTLP only.** Records are logs, and the body is this record's JSON so
//! ClickHouse can `JSONExtract` the candidate list. No forge call is made to
//! build one: the builders only read what the tick had already computed.
//!
//! **Bounded.** [`MAX_PICK_CANDIDATES`] caps every list; `candidates_total`
//! carries the uncapped count.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::types::QueueDisposition;

/// Payload version carried in the record's own `schema_version` field
/// (distinct from the envelope's gate, which is `NEW_KIND_SCHEMA_VERSION`).
pub const PICK_DECISION_SCHEMA_VERSION: u32 = 2;

/// Candidate / acted / skipped lists are capped at this many entries.
pub const MAX_PICK_CANDIDATES: usize = 50;

/// `role` value for work-finder dispatch decisions.
pub const WORK_FINDER_ROLE: &str = "work_finder";

/// The closed set of `acted[].action` values a role tick can carry (#10432).
/// Each is a forge write the role agent issued through the agent `gh` front.
pub const ROLE_ACTIONS: [&str; 9] = [
    "claimed",
    "approved",
    "changes_requested",
    "merged",
    "curated",
    "promoted",
    "blocked",
    "escalated",
    "labeled",
];

/// Where a record's `candidates` came from (`candidate_source`).
pub mod source {
    /// The work finder's ready queue, in dispatch order.
    pub const READY_QUEUE: &str = "ready_queue";
    /// The queue the role agent consumed through `loom-daemon pr-queue`.
    pub const SERVING_QUEUE: &str = "serving_queue";
    /// The issue/PR listings the role agent read through the agent `gh` front.
    pub const LISTING: &str = "listing";
    /// The daemon's own admission-gate listing (the agent's queue was not
    /// observed).
    pub const GATE_LISTING: &str = "gate_listing";
    /// Nothing observed.
    pub const NONE: &str = "none";
}

/// Why a candidate was not acted on. A **closed** set: a new reason is a new
/// variant here and a row in `telemetry-kind-pick-decision.md`, never a free
/// string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PickSkipReason {
    /// Overlaps an in-flight chain of stacked work.
    OverlapChain,
    /// The item already has an open PR (the open-PR guard).
    PrOpenSkip,
    /// Held for a human: a park / hold label, hard exclusion or decline.
    OperatorHold,
    /// The token pool / quota would not admit the work.
    Quota,
    /// A concurrency, ramp, per-repo, slice or saturation cap held it.
    Cap,
    /// A live sweep or claim already covers it.
    InFlight,
    /// Inside a back-off, cooldown, recheck or retry window.
    Backoff,
    /// The whole repo's dispatch was halted (red main, gate, drain, breaker).
    Halted,
    /// Not for this host (host affinity, host class, or a peer's claim).
    HostConstraint,
    /// Blocked by something specific to the item (quarantine, missing command).
    Blocked,
    /// The dispatch attempt failed.
    Error,
    /// The tick itself did not run the role (load, pool, queue gate, ...).
    TickSkipped,
    /// The role ran, its actions were observed, and it did not act on this
    /// candidate (and no label explains why).
    NotSelected,
}

impl PickSkipReason {
    /// Every reason, in declaration order (pinned by a test against the docs).
    pub const ALL: [Self; 13] = [
        Self::OverlapChain,
        Self::PrOpenSkip,
        Self::OperatorHold,
        Self::Quota,
        Self::Cap,
        Self::InFlight,
        Self::Backoff,
        Self::Halted,
        Self::HostConstraint,
        Self::Blocked,
        Self::Error,
        Self::TickSkipped,
        Self::NotSelected,
    ];

    /// The snake_case wire name (identical to the serde form).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OverlapChain => "overlap_chain",
            Self::PrOpenSkip => "pr_open_skip",
            Self::OperatorHold => "operator_hold",
            Self::Quota => "quota",
            Self::Cap => "cap",
            Self::InFlight => "in_flight",
            Self::Backoff => "backoff",
            Self::Halted => "halted",
            Self::HostConstraint => "host_constraint",
            Self::Blocked => "blocked",
            Self::Error => "error",
            Self::TickSkipped => "tick_skipped",
            Self::NotSelected => "not_selected",
        }
    }

    /// The reason a work-finder [`QueueDisposition`] maps to, or `None` for
    /// [`QueueDisposition::Dispatched`] (that row is *acted*, not skipped).
    ///
    /// Total over the enum, no catch-all, so a new disposition must be
    /// classified here deliberately.
    #[must_use]
    pub fn from_disposition(disposition: QueueDisposition) -> Option<Self> {
        use QueueDisposition as Qd;
        Some(match disposition {
            Qd::Dispatched => return None,
            Qd::InFlight => Self::InFlight,
            Qd::DeferredCapacity
            | Qd::DeferredRampCap
            | Qd::DeferredSaturation
            | Qd::DeferredOutOfSlice
            | Qd::DeferredRepoCap => Self::Cap,
            Qd::DeferredFileOverlap => Self::OverlapChain,
            Qd::DeferredBuildBackoff
            | Qd::RecheckInterval
            | Qd::DispatchBackoff
            | Qd::NoopCooldown
            | Qd::PrlessRetry => Self::Backoff,
            Qd::WorkspaceHalted => Self::Halted,
            Qd::WorkspaceCommandsMissing | Qd::Quarantined | Qd::LabelledBlocked | Qd::Unknown => {
                Self::Blocked
            }
            Qd::HostConstraint | Qd::HostClassRefused | Qd::PeerClaim => Self::HostConstraint,
            Qd::Parked | Qd::HardExclusion | Qd::Declined => Self::OperatorHold,
            Qd::OpenPrBackoff | Qd::OpenPr => Self::PrOpenSkip,
            Qd::DispatchError => Self::Error,
        })
    }
}

/// The value a candidate list was sorted by.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PickSortKey {
    /// What the key is (`dispatch_rank`, `listing_order`, ...).
    pub name: String,
    /// Its value, rendered as text.
    pub value: String,
}

/// One considered item, at its position in the ranking.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PickCandidate {
    /// 1-based position in the ranking.
    pub rank: u32,
    /// Forge `owner/repo`, or `repo_unresolved` (never a local path, #9442).
    pub repo: String,
    /// Issue or PR number.
    pub number: u32,
    /// The stage the item was in when considered (a label such as
    /// `loom:review-requested`).
    pub stage: String,
    /// The sort key's value, when the ranker exposes one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sort_key: Option<PickSortKey>,
}

/// An item the tick acted on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PickAction {
    pub repo: String,
    pub number: u32,
    /// What was done: `dispatched` (work finder) or one of the role actions in
    /// [`ROLE_ACTIONS`].
    pub action: String,
}

/// An item the tick looked at and did not act on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PickSkip {
    pub repo: String,
    pub number: u32,
    pub reason: PickSkipReason,
    /// Daemon-templated specifics of the skip, when the reason has them.
    /// Today only a work-finder file-overlap deferral (#9781) sets it: the
    /// shared `## Affected Files` paths (`file overlap: a, b`, bounded to one
    /// short line). Additive and optional; absent everywhere else.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// One workspace in a [`PickDraw`], with its draw weight.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PickDrawCandidate {
    /// Forge `owner/repo`, or `repo_unresolved`.
    pub repo: String,
    /// Its draw weight (from the workspace's `fleet_priority`).
    pub weight: u64,
}

/// One weighted workspace draw of the work finder (#11103).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PickDraw {
    /// `very-important` when only workspaces with `loom:very-important` work
    /// next were drawn over, `all` otherwise.
    pub pool: String,
    /// The workspaces drawn over, with weights.
    pub candidates: Vec<PickDrawCandidate>,
    /// Sum of the weights.
    pub total_weight: u64,
    /// The RNG value in `0..total_weight` that chose the pick.
    pub roll: u64,
    /// The drawn workspace.
    pub picked: String,
    /// The issue the draw placed next in the dispatch order.
    pub number: u32,
}

/// The work finder's workspace draws for one tick (#11103): enough to replay
/// the dispatch order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PickWorkspaceDraw {
    /// The tick's RNG seed, as decimal text (a `u64` does not survive a
    /// JSON number in every reader).
    pub seed: String,
    /// How many draws the tick made, before the cap.
    pub draws_total: usize,
    /// The first draws, in order (at most [`MAX_PICK_CANDIDATES`]).
    pub draws: Vec<PickDraw>,
}

/// What the tick concluded about one ranked candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickVerdict {
    /// Acted on, with the action name.
    Acted(&'static str),
    /// Skipped, with the closed-set reason.
    Skipped(PickSkipReason),
    /// Skipped, with the reason and its [`PickSkip::detail`].
    SkippedWithDetail(PickSkipReason, String),
    /// Considered; the daemon cannot see what the role did with it (a role
    /// agent chooses among its queue itself).
    Undecided,
}

/// One `pick.decision` record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PickDecisionRecord {
    /// [`PICK_DECISION_SCHEMA_VERSION`].
    pub schema_version: u32,
    /// `judge`, `doctor`, `champion`, `curator`, `work_finder`, ...
    pub role: String,
    /// The deciding host.
    pub host: String,
    /// Correlates with `role_tick.outcome` / the role's trace for role ticks.
    pub tick_id: String,
    pub started_at: DateTime<Utc>,
    pub ended_at: DateTime<Utc>,
    /// The tick's own result (`success`, `skipped_queue_empty`, `dispatched`,
    /// `idle`, ...).
    pub outcome: String,
    /// Candidates considered, before the cap.
    pub candidates_total: usize,
    /// Candidates in the order the ranker put them, at most
    /// [`MAX_PICK_CANDIDATES`].
    pub candidates: Vec<PickCandidate>,
    /// Items acted on (at most [`MAX_PICK_CANDIDATES`]).
    pub acted: Vec<PickAction>,
    /// Skipped candidates among [`Self::candidates`], with their reasons.
    pub skipped: Vec<PickSkip>,
    /// Where `candidates` came from: one of the [`source`] constants.
    #[serde(default)]
    pub candidate_source: String,
    /// `true` when what the tick acted on was observed, so a candidate with no
    /// action is a real skip; `false` when only the ranking was seen (every
    /// unexplained candidate is then neither acted nor skipped).
    #[serde(default)]
    pub decisions_observed: bool,
    /// The work finder's workspace draws (#11103); `None` for role ticks and
    /// for a work-finder tick with nothing to draw.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_draw: Option<PickWorkspaceDraw>,
}

/// Identity of the tick a record describes.
#[derive(Debug, Clone)]
pub struct PickTick {
    pub role: String,
    pub host: String,
    pub tick_id: String,
    pub started_at: DateTime<Utc>,
    pub ended_at: DateTime<Utc>,
    pub outcome: String,
}

impl PickDecisionRecord {
    /// Build a record from `ranked`, **already in the ranker's order**
    /// (`candidate.rank` is taken as given). Applies the cap and derives
    /// `acted` / `skipped` from the verdicts. An empty `ranked` is a valid
    /// (empty-tick) record.
    #[must_use]
    pub fn build(tick: PickTick, ranked: Vec<(PickCandidate, PickVerdict)>) -> Self {
        let candidates_total = ranked.len();
        let mut candidates = Vec::new();
        let mut acted = Vec::new();
        let mut skipped = Vec::new();
        for (candidate, verdict) in ranked {
            let listed = candidates.len() < MAX_PICK_CANDIDATES;
            match verdict {
                PickVerdict::Acted(action) if acted.len() < MAX_PICK_CANDIDATES => {
                    acted.push(PickAction {
                        repo: candidate.repo.clone(),
                        number: candidate.number,
                        action: action.to_string(),
                    });
                }
                PickVerdict::Skipped(reason) if listed => skipped.push(PickSkip {
                    repo: candidate.repo.clone(),
                    number: candidate.number,
                    reason,
                    detail: None,
                }),
                PickVerdict::SkippedWithDetail(reason, detail) if listed => {
                    skipped.push(PickSkip {
                        repo: candidate.repo.clone(),
                        number: candidate.number,
                        reason,
                        detail: Some(detail),
                    });
                }
                _ => {}
            }
            if listed {
                candidates.push(candidate);
            }
        }
        PickDecisionRecord {
            schema_version: PICK_DECISION_SCHEMA_VERSION,
            role: tick.role,
            host: tick.host,
            tick_id: tick.tick_id,
            started_at: tick.started_at,
            ended_at: tick.ended_at,
            outcome: tick.outcome,
            candidates_total,
            candidates,
            acted,
            skipped,
            candidate_source: source::NONE.to_string(),
            decisions_observed: false,
            workspace_draw: None,
        }
    }

    /// Raise `candidates_total` to `total` when the source held more rows than
    /// it handed over (a capped serving queue); never lowers it.
    pub fn raise_candidates_total(&mut self, total: usize) {
        self.candidates_total = self.candidates_total.max(total);
    }

    /// Stamp where the candidates came from and whether decisions were seen.
    #[must_use]
    pub fn with_source(mut self, candidate_source: &str, decisions_observed: bool) -> Self {
        self.candidate_source = candidate_source.to_string();
        self.decisions_observed = decisions_observed;
        self
    }

    /// Attach the work finder's workspace draws (#11103), capping the list at
    /// [`MAX_PICK_CANDIDATES`].
    #[must_use]
    pub fn with_workspace_draw(mut self, draw: Option<PickWorkspaceDraw>) -> Self {
        self.workspace_draw = draw.map(|mut d| {
            d.draws.truncate(MAX_PICK_CANDIDATES);
            d
        });
        self
    }

    /// Append observed actions not already in [`Self::acted`] (an action on an
    /// item outside the candidate list, or a second action on one), up to
    /// [`MAX_PICK_CANDIDATES`].
    pub fn add_actions(&mut self, actions: impl IntoIterator<Item = PickAction>) {
        for action in actions {
            if self.acted.len() >= MAX_PICK_CANDIDATES {
                break;
            }
            if !self.acted.contains(&action) {
                self.acted.push(action);
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "pick_decision_tests.rs"]
mod tests;
