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
//! **Complete and chunked.** Every list carries every entry the tick
//! produced: nothing is truncated, and `candidates_total` / `draws_total`
//! keep the tick's own totals. A record whose JSON would exceed
//! [`CHUNK_BYTES`] is split by [`split_into_chunks`] into several records
//! that share `tick_id` and carry `chunk_index` / `chunk_count`; the union
//! of the chunks in `chunk_index` order is exactly the original. The chunk
//! size exists only because one OTLP/gRPC message is limited to about 4 MB.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::step_dispatch::item_draw::Exclusion;
use crate::types::QueueDisposition;
use crate::work_finder::step_item::PassReason;

/// Payload version carried in the record's own `schema_version` field
/// (distinct from the envelope's gate, which is `NEW_KIND_SCHEMA_VERSION`).
pub const PICK_DECISION_SCHEMA_VERSION: u32 = 2;

/// A record whose JSON is larger than this is split into several records
/// (`chunk_index` / `chunk_count`), never truncated. One OTLP/gRPC message is
/// limited to about 4 MB; [`crate::telemetry::kinds::fleet_state::CHUNK_BYTES`]
/// uses the same value.
pub const CHUNK_BYTES: usize = 1_000_000;

/// The `chunk_count` default for `#[serde(default)]` (1 = unsplit).
fn one() -> u32 {
    1
}

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
    /// The item's work already landed (Issue #11429).
    AlreadyLanded,
    /// The issue is closed as not planned (Issue #11429).
    AlreadyClosed,
    /// The item's state calls for no step: an epic, an unpromoted proposal,
    /// or a PR with no workflow label (the step engine's item draw, #11896).
    NoStep,
}

impl PickSkipReason {
    /// Every reason, in declaration order (pinned by a test against the docs).
    pub const ALL: [Self; 16] = [
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
        Self::AlreadyLanded,
        Self::AlreadyClosed,
        Self::NoStep,
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
            Self::AlreadyLanded => "already_landed",
            Self::AlreadyClosed => "already_closed",
            Self::NoStep => "no_step",
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
            Qd::HostConstraint | Qd::PeerClaim => Self::HostConstraint,
            Qd::Parked | Qd::HardExclusion | Qd::Declined => Self::OperatorHold,
            Qd::OpenPrBackoff | Qd::OpenPr => Self::PrOpenSkip,
            Qd::DispatchError => Self::Error,
            Qd::AlreadyLanded => Self::AlreadyLanded,
            Qd::AlreadyClosed => Self::AlreadyClosed,
        })
    }

    /// The reason a step-engine weight-0 [`Exclusion`] maps to (#11896).
    ///
    /// Total over the exclusions and the classifier's pass reasons, no catch-all, so a new pass
    /// reason must be classified here deliberately, as a new disposition must
    /// be in [`Self::from_disposition`].
    #[must_use]
    pub fn from_exclusion(exclusion: Exclusion) -> Self {
        use PassReason as P;
        match exclusion {
            Exclusion::PoolExhausted => Self::Quota,
            Exclusion::BuildBackoff => Self::Backoff,
            Exclusion::Pass(reason) => match reason {
                P::Frozen | P::Operator | P::HardExclusion => Self::OperatorHold,
                P::Claimed | P::Queued => Self::InFlight,
                P::SharedPr => Self::PrOpenSkip,
                P::Sequenced => Self::OverlapChain,
                P::Blocked => Self::Blocked,
                P::CapabilityMissing => Self::HostConstraint,
                P::Epic | P::Proposal | P::NoStage => Self::NoStep,
            },
        }
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
    /// Only work-finder rows set it: a file-overlap deferral (#9781) carries
    /// the shared `## Affected Files` paths (`file overlap: a, b`, bounded to
    /// one short line), and a `workspace_halted` row (#11811) its closed-
    /// vocabulary halt cause (`main_red`, `drain`, ...). Additive and
    /// optional; absent everywhere else.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// The work finder's own un-collapsed [`QueueDisposition`] (#11811), so a
    /// reader can tell apart what `reason` lumps together (which cap, which
    /// back-off). Work-finder rows only; absent on role ticks and older
    /// records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disposition: Option<QueueDisposition>,
    /// When the row's time-boxed hold clears (#11811, the `queue.snapshot`
    /// `held_until` of #9311): set for the recheck, dispatch / open-PR
    /// back-off, cooldown and retry windows the work finder tracks; absent
    /// otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub held_until: Option<DateTime<Utc>>,
}

/// A work-finder skip (#11811): the closed-set reason plus the row's own
/// disposition, its exportable detail and its hold expiry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickQueueSkip {
    pub reason: PickSkipReason,
    pub disposition: QueueDisposition,
    /// Only daemon-templated, closed or path-derived text (see
    /// [`PickSkip::detail`]); never free-form dispatch-error text.
    pub detail: Option<String>,
    pub held_until: Option<DateTime<Utc>>,
}

/// One workspace in a [`PickDraw`], with its draw weight.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PickDrawCandidate {
    /// Forge `owner/repo`, or `repo_unresolved`.
    pub repo: String,
    /// Its effective draw weight: `base_weight + bonus` (#11742). The draw
    /// picked it with probability `weight / total_weight`. (Records from
    /// before #11742 carry a softmax weight here, on a 1,000,000 scale.)
    pub weight: u64,
    /// The workspace's roster `weight` (#11742). Absent on older records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_weight: Option<u32>,
    /// The bonus points it earned for a verified red-main fix,
    /// `loom:very-important` or `loom:important` work (#11742). Absent on
    /// older records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bonus: Option<u64>,
}

/// One weighted workspace draw of the work finder (#11103).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PickDraw {
    /// The workspaces drawn over, with weights. (Records from before #11742
    /// also carry a `pool` tier and a `p_min`; both are retired and ignored
    /// when read.)
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
    /// How many draws the tick made.
    pub draws_total: usize,
    /// Every draw, in order.
    pub draws: Vec<PickDraw>,
    /// How `seed` was derived (#11683): `loom-draw/v1` when it is
    /// [`minute_seed`](crate::work_finder::workspace_draw::minute_seed) of
    /// `seed_minute` and `seed_host`. Absent when the seed was pinned (tests)
    /// or the record predates #11683.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed_scheme: Option<String>,
    /// The wall-clock minute (`floor(unix_utc_secs / 60)`) the seed was
    /// derived from, as decimal text like `seed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed_minute: Option<String>,
    /// The host id the seed was derived from (the same value as the record's
    /// `host`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed_host: Option<String>,
}

/// The step engine's item draws for one tick (#11896): the seed inputs, the
/// weight-0 exclusion counts and every draw, enough to replay the sequence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PickItemDraw {
    /// `loom-draw/v2`.
    pub seed_scheme: String,
    /// The wall-clock minute the seeds were derived from, as decimal text.
    pub seed_minute: String,
    /// The host id the seeds were derived from.
    pub seed_host: String,
    /// The process tick counter the seeds were derived from, as decimal text.
    pub tick_counter: String,
    /// How many item draws the tick made, across workspaces.
    pub draws_total: usize,
    /// Weight-0 items per workspace and reason. Header: repeats on every
    /// chunk, so read it from one chunk only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub excluded: Vec<PickItemExclusion>,
    /// Every draw, in order (the chunk's in-order slice).
    pub draws: Vec<PickItemDrawStep>,
}

/// How many items of one workspace a reason kept out of the item draw.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PickItemExclusion {
    /// Forge `owner/repo`, or `repo_unresolved`.
    pub repo: String,
    /// The classifier's pass reason (`frozen`, `claimed`, `no_stage`, ...)
    /// or `pool_exhausted` / `build_backoff`.
    pub reason: String,
    /// The closed-set skip reason it maps to
    /// ([`PickSkipReason::from_exclusion`]).
    pub skip_reason: PickSkipReason,
    pub count: usize,
}

/// One item draw: the seed, the pool, the roll and the drawn item's weight
/// terms (`weight = actionable × priority × age`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PickItemDrawStep {
    /// Forge `owner/repo`: also the seed's `workspace` input.
    pub repo: String,
    /// 0-based draw index within the workspace (the seed's last input).
    pub draw_index: u32,
    /// The draw's seed, as decimal text.
    pub seed: String,
    /// Items in the pool when drawn.
    pub pool_size: usize,
    /// Their total weight.
    pub total_weight: u64,
    /// The roll in `0..total_weight`.
    pub roll: u64,
    /// The number the step acts on (the PR for a PR step).
    pub number: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issue: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr: Option<u32>,
    /// The step (`builder`, `judge`, `champion_merge`, ...).
    pub step: String,
    /// The drawn item's weight, `priority × (divisor_min + min(age_min,
    /// cap_min))`.
    pub weight: u64,
    /// Always 1 for a drawn item.
    pub actionable: u8,
    /// The priority factor.
    pub priority: u32,
    /// The level it was read from, after the very-important cap.
    pub level: String,
    /// `red_main`, `normal` or `capability`.
    pub lane: String,
    /// The age term × 1000.
    pub age_permille: u64,
    /// The stage age in whole minutes, uncapped.
    pub age_minutes: u64,
    /// `mirror`, `events`, `created` or `none`.
    pub age_source: String,
}

/// A `loom:very-important` issue the flood cap ranked as `loom:important`
/// this tick (#11509): its workspace already has an older open one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PickViDemotion {
    /// Forge `owner/repo`, or `repo_unresolved`.
    pub repo: String,
    /// The demoted issue.
    pub number: u32,
    /// The issue holding the workspace's very-important slot.
    pub holder: u32,
}

/// What the tick concluded about one ranked candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickVerdict {
    /// Acted on, with the action name.
    Acted(&'static str),
    /// Skipped, with the closed-set reason.
    Skipped(PickSkipReason),
    /// Skipped by the work finder, with its disposition (#11811).
    SkippedQueue(PickQueueSkip),
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
    /// 0-based position of this chunk among the record's chunks. A record
    /// that fits one chunk is `0`.
    #[serde(default)]
    pub chunk_index: u32,
    /// How many chunks the record was split into (1 = unsplit). The chunks of
    /// one tick share `tick_id`; concatenating their lists in `chunk_index`
    /// order reproduces the unsplit record.
    #[serde(default = "one")]
    pub chunk_count: u32,
    /// Candidates considered.
    pub candidates_total: usize,
    /// Candidates in the order the ranker put them, every one of them.
    pub candidates: Vec<PickCandidate>,
    /// Items acted on.
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
    /// for a work-finder tick with nothing to draw. `seed` / `draws_total`
    /// repeat on every chunk; `draws` is the chunk's in-order slice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_draw: Option<PickWorkspaceDraw>,
    /// The work finder's `loom:very-important` flood-cap demotions (#11509),
    /// every one of them; empty (and absent on the wire) when the cap demoted
    /// nothing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub very_important_demoted: Vec<PickViDemotion>,
    /// The per-repository lane computation behind a judge or doctor run
    /// (#10630): one entry per observed repository, so "why only one Doctor?"
    /// is one query. Empty for every other record.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lanes: Vec<PickLane>,
    /// The repository a role tick ran for (#11858): forge `owner/repo`, or
    /// `repo_unresolved`. Present on every role record, so a tick with an
    /// empty queue still names its repo; absent on work-finder records (their
    /// candidates name their own repos) and on an older daemon's records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// The step engine's item draws (#11896); `None` everywhere else. The
    /// header repeats on every chunk; `draws` is the chunk's in-order slice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_draw: Option<PickItemDraw>,
}

/// One repository's lane computation (#10630): `lanes = clamp(ceil(debt / k),
/// 1, cap)` before (`wanted`) and after (`lanes`) the host-capacity trim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PickLane {
    pub repo: String,
    /// The debt the formula read (last-known PRs on the role's axis).
    pub debt: usize,
    /// Lanes the formula wanted.
    pub wanted: usize,
    /// Lanes after the trim to the host capacity.
    pub lanes: usize,
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
    /// (`candidate.rank` is taken as given). Derives `acted` / `skipped` from
    /// the verdicts; every ranked entry is listed. An empty `ranked` is a
    /// valid (empty-tick) record.
    #[must_use]
    pub fn build(tick: PickTick, ranked: Vec<(PickCandidate, PickVerdict)>) -> Self {
        let candidates_total = ranked.len();
        let mut candidates = Vec::new();
        let mut acted = Vec::new();
        let mut skipped = Vec::new();
        for (candidate, verdict) in ranked {
            match verdict {
                PickVerdict::Acted(action) => {
                    acted.push(PickAction {
                        repo: candidate.repo.clone(),
                        number: candidate.number,
                        action: action.to_string(),
                    });
                }
                PickVerdict::Skipped(reason) => skipped.push(PickSkip {
                    repo: candidate.repo.clone(),
                    number: candidate.number,
                    reason,
                    detail: None,
                    disposition: None,
                    held_until: None,
                }),
                PickVerdict::SkippedQueue(skip) => skipped.push(PickSkip {
                    repo: candidate.repo.clone(),
                    number: candidate.number,
                    reason: skip.reason,
                    detail: skip.detail,
                    disposition: Some(skip.disposition),
                    held_until: skip.held_until,
                }),
                PickVerdict::Undecided => {}
            }
            candidates.push(candidate);
        }
        PickDecisionRecord {
            schema_version: PICK_DECISION_SCHEMA_VERSION,
            role: tick.role,
            host: tick.host,
            tick_id: tick.tick_id,
            started_at: tick.started_at,
            ended_at: tick.ended_at,
            outcome: tick.outcome,
            chunk_index: 0,
            chunk_count: 1,
            candidates_total,
            candidates,
            acted,
            skipped,
            candidate_source: source::NONE.to_string(),
            decisions_observed: false,
            workspace_draw: None,
            very_important_demoted: Vec::new(),
            lanes: Vec::new(),
            repo: None,
            item_draw: None,
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

    /// Attach the work finder's workspace draws (#11103) in full.
    #[must_use]
    pub fn with_workspace_draw(mut self, draw: Option<PickWorkspaceDraw>) -> Self {
        self.workspace_draw = draw;
        self
    }

    /// Attach the step engine's item draws (#11896) in full.
    #[must_use]
    pub fn with_item_draw(mut self, draw: Option<PickItemDraw>) -> Self {
        self.item_draw = draw;
        self
    }

    /// Attach the work finder's flood-cap demotions (#11509) in full.
    #[must_use]
    pub fn with_very_important_demoted(mut self, demoted: Vec<PickViDemotion>) -> Self {
        self.very_important_demoted = demoted;
        self
    }

    /// Append observed actions not already in [`Self::acted`] (an action on an
    /// item outside the candidate list, or a second action on one).
    pub fn add_actions(&mut self, actions: impl IntoIterator<Item = PickAction>) {
        for action in actions {
            if !self.acted.contains(&action) {
                self.acted.push(action);
            }
        }
    }
}

fn json_len<T: Serialize>(value: &T) -> usize {
    serde_json::to_vec(value).map_or(0, |v| v.len())
}

/// One list entry a chunk carries, in pack order.
#[derive(Debug)]
enum PickChunkEntry {
    Candidate(PickCandidate),
    Skip(PickSkip),
    Act(PickAction),
    Draw(PickDraw),
    Demoted(PickViDemotion),
    ItemDraw(PickItemDrawStep),
}

/// Split `record` into chunks whose JSON is at most `max_bytes` each (one
/// chunk, unchanged but for `chunk_index: 0, chunk_count: 1`, when it fits).
/// Every chunk repeats the header (`role`, `host`, `tick_id`, the tick
/// bounds, `outcome`, `candidates_total`, `candidate_source`,
/// `decisions_observed`, and `workspace_draw.seed` / `draws_total` /
/// `seed_scheme` / `seed_minute` / `seed_host`); the
/// `candidates`, `skipped`, `acted`, `workspace_draw.draws` and
/// `very_important_demoted` entries are packed in order, so concatenating the
/// chunks' lists in `chunk_index` order reproduces `record` exactly: nothing
/// is dropped. A cut may fall between a candidate and its `skipped` / `acted`
/// entry; a reader reassembles before reading per-candidate verdicts. Pure.
#[must_use]
pub fn split_into_chunks(record: PickDecisionRecord, max_bytes: usize) -> Vec<PickDecisionRecord> {
    let mut whole = record;
    whole.chunk_index = 0;
    whole.chunk_count = 1;
    if json_len(&whole) <= max_bytes {
        return vec![whole];
    }
    let mut header = whole;
    let units = std::mem::take(&mut header.candidates)
        .into_iter()
        .map(PickChunkEntry::Candidate)
        .chain(
            std::mem::take(&mut header.skipped)
                .into_iter()
                .map(PickChunkEntry::Skip),
        )
        .chain(
            std::mem::take(&mut header.acted)
                .into_iter()
                .map(PickChunkEntry::Act),
        )
        .chain(
            header
                .workspace_draw
                .as_mut()
                .map(|d| std::mem::take(&mut d.draws))
                .unwrap_or_default()
                .into_iter()
                .map(PickChunkEntry::Draw),
        )
        .chain(
            std::mem::take(&mut header.very_important_demoted)
                .into_iter()
                .map(PickChunkEntry::Demoted),
        )
        .chain(
            header
                .item_draw
                .as_mut()
                .map(|d| std::mem::take(&mut d.draws))
                .unwrap_or_default()
                .into_iter()
                .map(PickChunkEntry::ItemDraw),
        );

    // Header plus room for the multi-digit chunk numbers.
    let budget = max_bytes.saturating_sub(json_len(&header) + 32);
    let mut chunks: Vec<PickDecisionRecord> = Vec::new();
    let mut current: Option<PickDecisionRecord> = None;
    let mut used = 0usize;
    for unit in units {
        let len = match &unit {
            PickChunkEntry::Candidate(c) => json_len(c),
            PickChunkEntry::Skip(s) => json_len(s),
            PickChunkEntry::Act(a) => json_len(a),
            PickChunkEntry::Draw(d) => json_len(d),
            PickChunkEntry::Demoted(d) => json_len(d),
            PickChunkEntry::ItemDraw(d) => json_len(d),
        } + 1; // the separating comma
               // Cut between entries; a lone oversized entry still goes out whole.
        if used > 0 && used + len > budget {
            chunks.push(current.take().expect("current chunk"));
            used = 0;
        }
        let chunk = current.get_or_insert_with(|| header.clone());
        match unit {
            PickChunkEntry::Candidate(c) => chunk.candidates.push(c),
            PickChunkEntry::Skip(s) => chunk.skipped.push(s),
            PickChunkEntry::Act(a) => chunk.acted.push(a),
            PickChunkEntry::Draw(d) => chunk
                .workspace_draw
                .get_or_insert_with(|| PickWorkspaceDraw {
                    seed: String::new(),
                    draws_total: 0,
                    draws: Vec::new(),
                    seed_scheme: None,
                    seed_minute: None,
                    seed_host: None,
                })
                .draws
                .push(d),
            PickChunkEntry::Demoted(d) => chunk.very_important_demoted.push(d),
            // The header's `item_draw` (with emptied `draws`) is on every
            // chunk, so the draw lands in it.
            PickChunkEntry::ItemDraw(d) => {
                if let Some(item_draw) = chunk.item_draw.as_mut() {
                    item_draw.draws.push(d);
                }
            }
        }
        used += len;
    }
    chunks.push(current.unwrap_or(header));
    let count = u32::try_from(chunks.len()).unwrap_or(u32::MAX);
    chunks
        .into_iter()
        .enumerate()
        .map(|(i, mut chunk)| {
            chunk.chunk_index = u32::try_from(i).unwrap_or(u32::MAX);
            chunk.chunk_count = count;
            chunk
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "pick_decision_tests.rs"]
mod tests;
