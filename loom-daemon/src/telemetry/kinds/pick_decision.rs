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

use crate::types::QueueDisposition;

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
}

impl PickSkipReason {
    /// Every reason, in declaration order (pinned by a test against the docs).
    pub const ALL: [Self; 15] = [
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
            Qd::HostConstraint => Self::HostConstraint,
            Qd::Parked | Qd::HardExclusion | Qd::Declined => Self::OperatorHold,
            Qd::OpenPrBackoff | Qd::OpenPr => Self::PrOpenSkip,
            Qd::DispatchError => Self::Error,
            Qd::AlreadyLanded => Self::AlreadyLanded,
            Qd::AlreadyClosed => Self::AlreadyClosed,
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
    /// Its draw weight (from the workspace's `fleet_priority`). Relative:
    /// only `weight / total_weight` is meaningful. Since #11684 it is a
    /// softmax weight on a 1,000,000 scale, relative to the draw's `p_min`,
    /// so one workspace's raw weight varies between draws.
    pub weight: u64,
}

/// One weighted workspace draw of the work finder (#11103).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PickDraw {
    /// `red-main-fix` when only workspaces whose next candidate is a verified
    /// red-main fix were drawn over (#11547), else `very-important` when only
    /// workspaces with `loom:very-important` work next were, `all` otherwise.
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
    /// The smallest `fleet_priority` in the pool: the workspace weighing the
    /// full scale (#11684). Absent on records from before #11684.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p_min: Option<u32>,
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
    /// The softmax temperature every draw of the tick weighed at
    /// (`dispatch.drawTemperature`, #11684). Per tick, like `seed`, and
    /// repeated on every chunk like it. Absent on records from before #11684.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<u32>,
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
                }),
                PickVerdict::SkippedWithDetail(reason, detail) => {
                    skipped.push(PickSkip {
                        repo: candidate.repo.clone(),
                        number: candidate.number,
                        reason,
                        detail: Some(detail),
                    });
                }
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
}

/// Split `record` into chunks whose JSON is at most `max_bytes` each (one
/// chunk, unchanged but for `chunk_index: 0, chunk_count: 1`, when it fits).
/// Every chunk repeats the header (`role`, `host`, `tick_id`, the tick
/// bounds, `outcome`, `candidates_total`, `candidate_source`,
/// `decisions_observed`, and `workspace_draw.seed` / `draws_total`); the
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
                    temperature: None,
                })
                .draws
                .push(d),
            PickChunkEntry::Demoted(d) => chunk.very_important_demoted.push(d),
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
