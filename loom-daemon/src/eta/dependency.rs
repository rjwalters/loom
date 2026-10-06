//! Dependency-aware ETAs (#10510): a composition layer over any base `land`
//! heuristic that starts a blocked, stacked or sequenced item after its
//! parents.
//!
//! # The graph
//!
//! An [`Edge`] says a child waits on a parent, from one [`EdgeSource`], and
//! carries the instant it became knowable (`known_at`). An edge whose
//! `known_at` is not strictly before the estimate's `as_of` does not exist
//! for that estimate — the same point-in-time rule
//! [`super::history::StageSamples::select`] applies to samples, so a replay
//! cannot see an edge filed after its own instant.
//!
//! Two edge kinds ([`EdgeKind`]):
//!
//! - **start**: the child cannot start before the parent lands. Park records
//!   (`Blocked by: #N`), native "blocked by" dependencies, sub-issues of an
//!   epic, and `loom:epic-phase` order. Applies only to an item that has not
//!   started (a `blocked` or `no_dispatch_plan` refusal, or `ready_wait`).
//! - **merge**: the child cannot merge before the parent merges. Stacked PRs
//!   and merge sequencing (`<!-- loom:sequence after=N -->`, #9686). Bounds
//!   the merge only: the child's own path to approval is untouched.
//!
//! # The composition
//!
//! Per Monte Carlo draw `s`, for a node with own land draw `own[s]`, the
//! remaining path from dispatch `path[s]`, start parents `S` and merge
//! parents `M`:
//!
//! ```text
//! land[s] = max( own[s],                              (when the base answers)
//!                max_{p ∈ S} land_p[s] + path[s],     (when S is non-empty)
//!                max_{p ∈ M} land_p[s] )
//! ```
//!
//! which is `max(own_ready, max parent land) + path` with own and path drawn
//! from the same uniform. Each node draws **one** uniform per draw from its
//! own seed, so an ancestor shared by two parents (a diamond) contributes the
//! same land time to both: common random numbers, never a double count. A
//! node's distribution is the base heuristic's own four quantiles read as a
//! quantile function ([`quantile_at`]), so the layer is generic over any base
//! that reports p25/p50/p75/p90.
//!
//! # Refusals
//!
//! A parent with no estimate refuses the child `blocked_by`, with the
//! parent's reason recorded (`blocked_by:owner/repo#N (blocked)`); a parent
//! the graph does not know refuses it `blocked_by_unknown`; a node on a cycle
//! is refused `dependency_cycle`.
//!
//! # Explanation first
//!
//! The answer's [`DependencyRecord`] holds every node's four quantiles and
//! seed, so [`recompute`] reproduces the result from the explanation alone.

use super::explanation::{Combination, Explanation};
use super::history::StageSamples;
use super::simulate::{parse_seed, SplitMix64};
use super::{
    estimate_id, round3, AgeSource, CurrentStage, CurrentState, EstimateInput, Heuristic,
    NoEstimateReason, Stage, Subject, DRAWS,
};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

/// `combination.method` of a composed answer.
pub const METHOD: &str = "dependency_max_over_parents_crn";

/// `combination.draw_order` of a composed answer.
pub const DRAW_ORDER: &str = "per node (dependencies.nodes): one uniform per draw from the node's own seed, shared by every child that reads the node (common random numbers); own and path quantile functions read the same uniform";

/// Most nodes a composed answer may read. A larger closure is refused
/// `blocked_by` the first parent that would exceed it, never truncated
/// into a different number.
pub const MAX_NODES: usize = 64;

/// What an edge bounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    /// The child cannot start before the parent lands.
    Start,
    /// The child cannot merge before the parent merges.
    Merge,
}

/// Where an edge came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeSource {
    /// A `<!-- loom:park Blocked by: #N -->` record on a `loom:blocked` item.
    ParkRecord,
    /// A forge-native "blocked by" issue dependency.
    NativeDependency,
    /// A sub-issue of an epic.
    SubIssue,
    /// `loom:epic-phase` order: phase k+1 after phase k.
    EpicPhase,
    /// A PR based on another PR's head branch.
    StackedPr,
    /// A merge-sequencing marker (`<!-- loom:sequence after=N -->`, #9686).
    Sequence,
}

impl EdgeSource {
    /// What an edge from this source bounds.
    #[must_use]
    pub fn kind(self) -> EdgeKind {
        match self {
            EdgeSource::StackedPr | EdgeSource::Sequence => EdgeKind::Merge,
            _ => EdgeKind::Start,
        }
    }
}

/// One item in the graph: an issue in a repo.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct NodeKey {
    /// Lowercased `owner/repo`.
    pub repo: String,
    /// Issue.
    pub issue: u32,
}

impl NodeKey {
    /// The key for `repo#issue`.
    #[must_use]
    pub fn new(repo: &str, issue: u32) -> Self {
        NodeKey {
            repo: repo.to_ascii_lowercase(),
            issue,
        }
    }

    /// The key of an estimate's subject.
    #[must_use]
    pub fn of(subject: &Subject) -> Self {
        Self::new(&subject.repo, subject.issue)
    }
}

impl fmt::Display for NodeKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}#{}", self.repo, self.issue)
    }
}

/// `child` waits on `parent`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Edge {
    /// The waiting item.
    pub child: NodeKey,
    /// What it waits on.
    pub parent: NodeKey,
    /// Where the edge came from.
    pub source: EdgeSource,
    /// When it became knowable. Ignored at any `as_of` not strictly after it.
    pub known_at: DateTime<Utc>,
}

/// What the graph knows about a parent.
#[derive(Debug, Clone, PartialEq)]
pub enum Node {
    /// Open: estimated by the base from this input. `modeled` is the
    /// hold-aware view (#10284) a base that models the hold reads instead.
    Open {
        /// The described input.
        input: Box<EstimateInput>,
        /// The modeled input, when the item is in `merge_hold`/`merge_wait`.
        modeled: Option<Box<EstimateInput>>,
    },
    /// Landed (merged, or closed) at.
    Landed {
        /// When.
        at: DateTime<Utc>,
    },
}

/// The dependency graph one estimate pass reads. Built outside the
/// estimator (the tracker, or a backtest), handed in through
/// [`EstimateInput::dependencies`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DependencyGraph {
    /// Every edge observed, with its `known_at`.
    pub edges: Vec<Edge>,
    /// What is known of each item. A parent missing here is unknown.
    pub nodes: BTreeMap<NodeKey, Node>,
}

/// One parent of a node at `as_of`, all its edges folded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parent {
    /// The parent.
    pub key: NodeKey,
    /// Some edge to it bounds the start.
    pub start: bool,
    /// Some edge to it bounds the merge.
    pub merge: bool,
    /// Every source, deduplicated, in order.
    pub sources: Vec<EdgeSource>,
    /// The earliest `known_at`.
    pub known_at: DateTime<Utc>,
}

impl DependencyGraph {
    /// `child`'s parents at `as_of`: every edge known strictly before it,
    /// folded per parent, in parent order. A self-edge is a cycle, kept.
    #[must_use]
    pub fn parents(&self, child: &NodeKey, as_of: DateTime<Utc>) -> Vec<Parent> {
        let mut out: BTreeMap<&NodeKey, Parent> = BTreeMap::new();
        for edge in self
            .edges
            .iter()
            .filter(|e| &e.child == child && e.known_at < as_of)
        {
            let parent = out.entry(&edge.parent).or_insert_with(|| Parent {
                key: edge.parent.clone(),
                start: false,
                merge: false,
                sources: Vec::new(),
                known_at: edge.known_at,
            });
            match edge.source.kind() {
                EdgeKind::Start => parent.start = true,
                EdgeKind::Merge => parent.merge = true,
            }
            if !parent.sources.contains(&edge.source) {
                parent.sources.push(edge.source);
                parent.sources.sort();
            }
            parent.known_at = parent.known_at.min(edge.known_at);
        }
        out.into_values().collect()
    }

    /// Whether `node` reaches itself over the edges known at `as_of`.
    #[must_use]
    pub fn in_cycle(&self, node: &NodeKey, as_of: DateTime<Utc>) -> bool {
        let mut seen = BTreeSet::new();
        let mut stack: Vec<NodeKey> = self
            .parents(node, as_of)
            .into_iter()
            .map(|p| p.key)
            .collect();
        while let Some(next) = stack.pop() {
            if &next == node {
                return true;
            }
            if seen.insert(next.clone()) {
                stack.extend(self.parents(&next, as_of).into_iter().map(|p| p.key));
            }
        }
        false
    }
}

/// Whether `current` has not started: the item a start edge can still hold.
#[must_use]
pub fn unstarted(current: &CurrentState) -> bool {
    match current {
        CurrentState::Refused(reason) => rescuable(*reason),
        CurrentState::At(c) => c.stage == Stage::ReadyWait,
    }
}

/// The refusals a start edge explains, so a composition may answer them.
fn rescuable(reason: NoEstimateReason) -> bool {
    matches!(reason, NoEstimateReason::Blocked | NoEstimateReason::NoDispatchPlan)
}

/// What a parent's state was, in [`ParentRecord::status`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParentStatus {
    /// The base (or its own composition) answered.
    Estimated,
    /// Landed before `as_of`: the edge is satisfied.
    Landed,
    /// No estimate; `reason` says why.
    Refused,
    /// Not in the graph.
    Unknown,
}

/// One parent of the subject, as the explanation reports it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParentRecord {
    /// `owner/repo#N`.
    pub parent: String,
    /// What it bounds for this subject (`start` wins when both apply).
    pub edge: EdgeKind,
    /// Every source of the edge.
    pub sources: Vec<EdgeSource>,
    /// The earliest instant the edge was knowable.
    pub known_at: DateTime<Utc>,
    /// The parent's state.
    pub status: ParentStatus,
    /// Why it has no estimate, when refused (`blocked`, or a nested
    /// `blocked_by:…`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The parent's own land p50, seconds from `as_of`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p50_sec: Option<i64>,
    /// The parent's own land p90.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p90_sec: Option<i64>,
    /// Share of draws in which this parent set the max (three decimals).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binding_share: Option<f64>,
}

/// One node of a composed answer: everything [`recompute`] reads.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeRecord {
    /// `owner/repo#N`.
    pub node: String,
    /// `0x`-prefixed 16-hex seed of the node's uniforms.
    pub seed: String,
    /// Landed before `as_of`: every draw is zero.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub landed: bool,
    /// The base's own `[p25, p50, p75, p90]`, when it answered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub own_sec: Option<[i64; 4]>,
    /// The base's `[p25, p50, p75, p90]` from dispatch, when a start parent
    /// applies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path_sec: Option<[i64; 4]>,
    /// Start parents (`owner/repo#N`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub start: Vec<String>,
    /// Merge parents.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub merge: Vec<String>,
}

/// How a dependency composition produced (or refused) an estimate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DependencyRecord {
    /// [`METHOD`].
    pub method: String,
    /// The base heuristic id.
    pub base: String,
    /// Draws per node.
    pub draws: usize,
    /// The subject's parents.
    pub parents: Vec<ParentRecord>,
    /// The parent that most often set the max; absent when the item's own
    /// path did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binding_parent: Option<String>,
    /// Share of draws the binding parent set the max in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binding_share: Option<f64>,
    /// The propagated refusal, e.g. `blocked_by:owner/repo#N (blocked)`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_by: Option<String>,
    /// Every node the answer read, subject first. Empty on a refusal.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub nodes: Vec<NodeRecord>,
}

/// The `pct`th quantile of a distribution known by its four quantiles
/// `[p25, p50, p75, p90]`, at uniform `u ∈ [0, 1)`: linear between them,
/// the p25–p50 slope below p25 (floored at zero), and above p90 the
/// exponential tail whose p75→p90 rise matches the distribution's.
#[must_use]
pub fn quantile_at(q: [i64; 4], u: f64) -> f64 {
    let [p25, p50, p75, p90] = q.map(|x| x.max(0) as f64);
    let lerp = |u0: f64, x0: f64, u1: f64, x1: f64| x0 + (x1 - x0) * (u - u0) / (u1 - u0);
    if u < 0.25 {
        lerp(0.25, p25, 0.5, p50).max(0.0)
    } else if u < 0.5 {
        lerp(0.25, p25, 0.5, p50)
    } else if u < 0.75 {
        lerp(0.5, p50, 0.75, p75)
    } else if u < 0.9 {
        lerp(0.75, p75, 0.9, p90)
    } else {
        let theta = (p90 - p75).max(0.0) / 2.5_f64.ln();
        p90 + theta * (0.1 / (1.0 - u).max(f64::MIN_POSITIVE)).ln()
    }
}

/// The nearest-rank `pct`th percentile of sorted draws, in whole seconds —
/// the rule [`super::simulate`] uses for path totals.
fn nearest_rank(sorted: &[f64], pct: usize) -> i64 {
    let k = sorted.len();
    if k == 0 {
        return 0;
    }
    let rank = (pct * k).div_ceil(100).clamp(1, k);
    sorted[rank - 1].round() as i64
}

fn quantiles4(draws: &[f64]) -> [i64; 4] {
    let mut sorted = draws.to_vec();
    sorted.sort_by(f64::total_cmp);
    [25, 50, 75, 90].map(|pct| nearest_rank(&sorted, pct))
}

/// The seed of `node`'s uniforms for `heuristic` at `as_of`.
#[must_use]
pub fn node_seed(heuristic: &str, as_of: DateTime<Utc>, node: &str) -> u64 {
    let at = crate::telemetry::trace::instant(as_of);
    let hex =
        crate::telemetry::trace::derived_hex(&["loom.eta.dependency", heuristic, &at, node], 16);
    u64::from_str_radix(&hex, 16).unwrap_or(0)
}

/// What one subject's draws came to.
struct Composed {
    totals: Vec<f64>,
    /// Draws per binding term: `None` is the item's own path.
    wins: BTreeMap<Option<String>, usize>,
}

/// Draws of every node, memoised, from the records alone.
struct Sim<'a> {
    nodes: BTreeMap<&'a str, &'a NodeRecord>,
    draws: usize,
    memo: BTreeMap<&'a str, Vec<f64>>,
    visiting: BTreeSet<&'a str>,
}

impl<'a> Sim<'a> {
    fn new(nodes: &'a [NodeRecord], draws: usize) -> Self {
        Sim {
            nodes: nodes.iter().map(|n| (n.node.as_str(), n)).collect(),
            draws,
            memo: BTreeMap::new(),
            visiting: BTreeSet::new(),
        }
    }

    fn uniforms(record: &NodeRecord, draws: usize) -> Option<Vec<f64>> {
        let mut rng = SplitMix64::new(parse_seed(&record.seed)?);
        Some((0..draws).map(|_| rng.next_f64()).collect())
    }

    /// Per draw: the own term, each start parent's term, each merge
    /// parent's term (in record order), or `None` for a malformed record.
    #[allow(clippy::type_complexity)]
    fn terms(&mut self, name: &'a str) -> Option<(Option<Vec<f64>>, Vec<(&'a str, Vec<f64>)>)> {
        let record = *self.nodes.get(name)?;
        if !self.visiting.insert(name) {
            return None;
        }
        let u = Self::uniforms(record, self.draws)?;
        let own = record
            .own_sec
            .map(|q| u.iter().map(|&u| quantile_at(q, u)).collect::<Vec<f64>>());
        let mut terms = Vec::new();
        if !record.start.is_empty() {
            let path = record.path_sec?;
            let mut ready = vec![0.0_f64; self.draws];
            let mut starts = Vec::new();
            for parent in &record.start {
                let land = self.draws_of(parent)?;
                for (r, l) in ready.iter_mut().zip(&land) {
                    *r = r.max(*l);
                }
                starts.push((parent.as_str(), land));
            }
            for (parent, land) in starts {
                let term = land
                    .iter()
                    .zip(&u)
                    .map(|(l, &u)| l + quantile_at(path, u))
                    .collect();
                terms.push((parent, term));
            }
        }
        for parent in &record.merge {
            let land = self.draws_of(parent)?;
            terms.push((parent.as_str(), land));
        }
        if own.is_none() && record.start.is_empty() {
            return None;
        }
        self.visiting.remove(name);
        Some((own, terms))
    }

    fn draws_of(&mut self, name: &'a str) -> Option<Vec<f64>> {
        if let Some(d) = self.memo.get(name) {
            return Some(d.clone());
        }
        let record = *self.nodes.get(name)?;
        let draws = if record.landed {
            vec![0.0; self.draws]
        } else {
            let (own, terms) = self.terms(name)?;
            (0..self.draws)
                .map(|s| {
                    let own = own.as_ref().map_or(f64::NEG_INFINITY, |o| o[s]);
                    terms.iter().fold(own, |acc, (_, t)| acc.max(t[s]))
                })
                .collect()
        };
        self.memo.insert(name, draws.clone());
        Some(draws)
    }

    fn compose(&mut self, root: &'a str) -> Option<Composed> {
        let (own, terms) = self.terms(root)?;
        let mut wins: BTreeMap<Option<String>, usize> = BTreeMap::new();
        let mut totals = Vec::with_capacity(self.draws);
        for s in 0..self.draws {
            let mut best = own.as_ref().map_or(f64::NEG_INFINITY, |o| o[s]);
            let mut binding: Option<&str> = None;
            for (parent, term) in &terms {
                if term[s] > best {
                    best = term[s];
                    // A landed parent binds nothing: the item waits on no one.
                    let landed = self.nodes.get(parent).is_some_and(|n| n.landed);
                    binding = (!landed).then_some(*parent);
                }
            }
            *wins.entry(binding.map(str::to_string)).or_default() += 1;
            totals.push(best.max(0.0));
        }
        Some(Composed { totals, wins })
    }
}

/// Recompute a composed answer's `(p25, p50, p75, p90)` from its record.
#[must_use]
pub fn recompute(record: &DependencyRecord) -> Option<(i64, i64, i64, i64)> {
    let root = record.nodes.first()?;
    let composed = Sim::new(&record.nodes, record.draws).compose(&root.node)?;
    let [p25, p50, p75, p90] = quantiles4(&composed.totals);
    Some((p25, p50, p75, p90))
}

/// One node's state after resolution.
#[derive(Debug, Clone)]
enum Resolution {
    Landed,
    Unknown,
    Refused {
        reason: NoEstimateReason,
        detail: String,
    },
    Open(NodeRecord),
}

fn q4(explanation: &Explanation) -> Option<[i64; 4]> {
    let r = explanation.result.as_ref()?;
    Some([r.p25_sec, r.p50_sec, r.p75_sec, r.p90_sec?])
}

/// `input`, re-pointed at dispatch: the remaining path once its start
/// parents have landed.
fn path_input(input: &EstimateInput) -> EstimateInput {
    let mut path = input.clone();
    path.current = CurrentState::At(CurrentStage {
        stage: Stage::SweepCurator,
        entered_at: Some(input.as_of),
        age_sec: 0,
        age_source: AgeSource::TrackerObserved,
        rework_rounds: 0,
        episode_entered_at: None,
    });
    path.dispatch = None;
    path.held = None;
    path.stalls = Vec::new();
    path.dependencies = None;
    path
}

/// The base's explanation, re-identified as the wrapper's. Its result —
/// seed included, which `combination.seed` records — is untouched.
fn reidentify(
    mut explanation: Explanation,
    id: &'static str,
    input: &EstimateInput,
) -> Explanation {
    explanation.heuristic = id.to_string();
    explanation.estimate_id = estimate_id(&input.subject, explanation.kind, id, input.as_of);
    explanation
}

struct Composer<'a> {
    id: &'static str,
    base: &'a dyn Heuristic,
    graph: &'a DependencyGraph,
    history: &'a StageSamples,
    as_of: DateTime<Utc>,
    memo: BTreeMap<NodeKey, Resolution>,
}

/// A node's parents that apply to its current state, with the kind each
/// bounds: start for an unstarted node, else merge, else dropped.
fn applicable(parents: Vec<Parent>, current: &CurrentState) -> Vec<(Parent, EdgeKind)> {
    let unstarted = unstarted(current);
    parents
        .into_iter()
        .filter_map(|p| {
            let kind = if p.start && unstarted {
                EdgeKind::Start
            } else if p.merge {
                EdgeKind::Merge
            } else {
                return None;
            };
            Some((p, kind))
        })
        .collect()
}

impl Composer<'_> {
    fn resolve(&mut self, key: &NodeKey) -> Resolution {
        if let Some(r) = self.memo.get(key) {
            return r.clone();
        }
        let resolution = match self.graph.nodes.get(key) {
            None => Resolution::Unknown,
            Some(Node::Landed { at }) if *at < self.as_of => Resolution::Landed,
            Some(Node::Landed { .. }) => Resolution::Unknown,
            Some(Node::Open { input, modeled }) => {
                let input = match modeled {
                    Some(m) if self.base.models_hold() => m.as_ref(),
                    _ => input.as_ref(),
                };
                let own = self.base.estimate(input, self.history);
                self.resolve_open(key, input, &own)
            }
        };
        self.memo.insert(key.clone(), resolution.clone());
        resolution
    }

    fn resolve_open(
        &mut self,
        key: &NodeKey,
        input: &EstimateInput,
        own: &Explanation,
    ) -> Resolution {
        let parents = applicable(self.graph.parents(key, self.as_of), &input.current);
        let own_q = q4(own);
        let has_start = parents.iter().any(|(_, k)| *k == EdgeKind::Start);
        if own_q.is_none() {
            let reason = own
                .no_estimate_reason
                .unwrap_or(NoEstimateReason::UnknownStage);
            if !(has_start && rescuable(reason)) {
                return Resolution::Refused {
                    reason,
                    detail: reason.as_str().to_string(),
                };
            }
        }
        if parents.is_empty() {
            // The own answer exists: the refusal above returned otherwise.
            return Resolution::Open(self.record(key, own_q, None, &parents));
        }
        if self.graph.in_cycle(key, self.as_of) {
            return Resolution::Refused {
                reason: NoEstimateReason::DependencyCycle,
                detail: NoEstimateReason::DependencyCycle.as_str().to_string(),
            };
        }
        for (parent, _) in &parents {
            match self.resolve(&parent.key) {
                Resolution::Unknown => {
                    return Resolution::Refused {
                        reason: NoEstimateReason::BlockedByUnknown,
                        detail: format!("blocked_by_unknown:{}", parent.key),
                    }
                }
                Resolution::Refused { detail, .. } => {
                    return Resolution::Refused {
                        reason: NoEstimateReason::BlockedBy,
                        detail: format!("blocked_by:{} ({detail})", parent.key),
                    }
                }
                Resolution::Landed | Resolution::Open(_) => {}
            }
        }
        if self.memo.len() > MAX_NODES {
            return Resolution::Refused {
                reason: NoEstimateReason::BlockedBy,
                detail: format!("blocked_by:{} (graph_too_large)", parents[0].0.key),
            };
        }
        let path_q = if has_start {
            let path = self.base.estimate(&path_input(input), self.history);
            match q4(&path) {
                Some(q) => Some(q),
                None => {
                    let reason = path
                        .no_estimate_reason
                        .unwrap_or(NoEstimateReason::UnknownStage);
                    return Resolution::Refused {
                        reason,
                        detail: reason.as_str().to_string(),
                    };
                }
            }
        } else {
            None
        };
        Resolution::Open(self.record(key, own_q, path_q, &parents))
    }

    fn record(
        &self,
        key: &NodeKey,
        own_sec: Option<[i64; 4]>,
        path_sec: Option<[i64; 4]>,
        parents: &[(Parent, EdgeKind)],
    ) -> NodeRecord {
        let node = key.to_string();
        let names = |kind: EdgeKind| {
            parents
                .iter()
                .filter(|(_, k)| *k == kind)
                .map(|(p, _)| p.key.to_string())
                .collect()
        };
        NodeRecord {
            seed: format!("0x{:016x}", node_seed(self.id, self.as_of, &node)),
            node,
            landed: false,
            own_sec,
            path_sec,
            start: names(EdgeKind::Start),
            merge: names(EdgeKind::Merge),
        }
    }

    /// Every resolved node as a record (subject excluded), in key order.
    fn records(&self) -> Vec<NodeRecord> {
        self.memo
            .iter()
            .filter_map(|(key, r)| match r {
                Resolution::Open(record) => Some(record.clone()),
                Resolution::Landed => Some(NodeRecord {
                    node: key.to_string(),
                    seed: format!("0x{:016x}", node_seed(self.id, self.as_of, &key.to_string())),
                    landed: true,
                    own_sec: None,
                    path_sec: None,
                    start: Vec::new(),
                    merge: Vec::new(),
                }),
                _ => None,
            })
            .collect()
    }
}

fn refuse(
    mut explanation: Explanation,
    reason: NoEstimateReason,
    record: DependencyRecord,
) -> Explanation {
    explanation.no_estimate_reason = Some(reason);
    if let Some(stalled) = &mut explanation.stalled {
        stalled.applied = false;
    }
    explanation.result = None;
    explanation.contributions = None;
    explanation.combination = None;
    explanation.twin_otter = None;
    explanation.calibration = None;
    explanation.recalibration = None;
    explanation.dependencies = Some(record);
    explanation.enforce_cap();
    explanation
}

/// `base`'s estimate of `input`, composed over the dependency graph the
/// input carries, as heuristic `id`. Pure.
///
/// With no graph, or no parent that applies at `as_of`, the result is the
/// base's own explanation re-identified: the same numbers, bit for bit.
#[must_use]
pub fn compose(
    id: &'static str,
    base: &dyn Heuristic,
    input: &EstimateInput,
    history: &StageSamples,
) -> Explanation {
    let own = reidentify(base.estimate(input, history), id, input);
    let Some(graph) = input.dependencies.as_deref() else {
        return own;
    };
    let as_of = input.as_of;
    let root = NodeKey::of(&input.subject);
    let parents = applicable(graph.parents(&root, as_of), &input.current);
    if parents.is_empty() {
        return own;
    }
    let mut composer = Composer {
        id,
        base,
        graph,
        history,
        as_of,
        memo: BTreeMap::new(),
    };
    let resolution = composer.resolve_open(&root, input, &own);
    let mut nodes = vec![match &resolution {
        Resolution::Open(record) => record.clone(),
        _ => composer.record(&root, None, None, &[]),
    }];
    nodes.extend(composer.records());
    let mut sim = Sim::new(&nodes, DRAWS);
    let parent_records: Vec<ParentRecord> = parents
        .iter()
        .map(|(p, kind)| {
            let name = p.key.to_string();
            let (status, reason) = match composer.memo.get(&p.key) {
                Some(Resolution::Landed) => (ParentStatus::Landed, None),
                Some(Resolution::Open(_)) => (ParentStatus::Estimated, None),
                Some(Resolution::Refused { detail, .. }) => {
                    (ParentStatus::Refused, Some(detail.clone()))
                }
                Some(Resolution::Unknown) | None => (ParentStatus::Unknown, None),
            };
            let q = (status == ParentStatus::Estimated)
                .then(|| sim.draws_of(nodes.iter().find(|n| n.node == name)?.node.as_str()))
                .flatten()
                .map(|d| quantiles4(&d));
            ParentRecord {
                parent: name,
                edge: *kind,
                sources: p.sources.clone(),
                known_at: p.known_at,
                status,
                reason,
                p50_sec: q.map(|q| q[1]),
                p90_sec: q.map(|q| q[3]),
                binding_share: None,
            }
        })
        .collect();
    let mut record = DependencyRecord {
        method: METHOD.to_string(),
        base: base.id().to_string(),
        draws: DRAWS,
        parents: parent_records,
        binding_parent: None,
        binding_share: None,
        blocked_by: None,
        nodes: Vec::new(),
    };
    let root_record = match resolution {
        Resolution::Open(r) => r,
        Resolution::Refused { reason, detail } => {
            record.blocked_by = matches!(
                reason,
                NoEstimateReason::BlockedBy
                    | NoEstimateReason::BlockedByUnknown
                    | NoEstimateReason::DependencyCycle
            )
            .then_some(detail);
            return refuse(own, reason, record);
        }
        Resolution::Landed | Resolution::Unknown => {
            unreachable!("the subject resolves through resolve_open")
        }
    };
    let Some(composed) = Sim::new(&nodes, DRAWS).compose(&nodes[0].node) else {
        return refuse(own, NoEstimateReason::UnknownStage, record);
    };
    let share = |n: usize| round3(n as f64 / DRAWS as f64);
    for parent in &mut record.parents {
        let wins = composed.wins.get(&Some(parent.parent.clone())).copied();
        parent.binding_share = wins.map(share).or(Some(0.0)).filter(|_| {
            parent.status == ParentStatus::Estimated || parent.status == ParentStatus::Landed
        });
    }
    let binding = composed
        .wins
        .iter()
        .filter_map(|(k, n)| k.as_ref().map(|k| (k, *n)))
        .max_by(|a, b| a.1.cmp(&b.1).then_with(|| b.0.cmp(a.0)));
    let own_wins = composed.wins.get(&None).copied().unwrap_or(0);
    if let Some((parent, n)) = binding.filter(|(_, n)| *n > own_wins) {
        record.binding_parent = Some(parent.clone());
        record.binding_share = Some(share(n));
    }
    let [p25, p50, p75, p90] = quantiles4(&composed.totals);
    let path = (root_record.path_sec.is_some()).then(|| base.estimate(&path_input(input), history));
    record.nodes = nodes;
    finish(own, path, record, (p25, p50, p75, p90), as_of)
}

/// The composed explanation: the base's own frame (or, when the base
/// refused, the path from dispatch), with the composed result.
fn finish(
    own: Explanation,
    path: Option<Explanation>,
    record: DependencyRecord,
    (p25, p50, p75, p90): (i64, i64, i64, i64),
    as_of: DateTime<Utc>,
) -> Explanation {
    let own_result = own.result.clone();
    let mut explanation = own;
    let path_result = path.as_ref().and_then(|p| p.result.clone());
    if explanation.result.is_none() {
        if let Some(path) = path {
            explanation.history = path.history;
            explanation.history_window = path.history_window;
            explanation.path = path.path;
            explanation.stages = path.stages;
            explanation.branches = path.branches;
            explanation.contributions = path.contributions;
        }
    }
    let samples_min = [&own_result, &path_result]
        .into_iter()
        .flatten()
        .map(|r| r.samples_min)
        .min()
        .unwrap_or(0);
    let tail_extrapolated = [&own_result, &path_result]
        .into_iter()
        .flatten()
        .any(|r| r.tail_extrapolated);
    explanation.no_estimate_reason = None;
    explanation.combination = Some(Combination {
        method: METHOD.to_string(),
        draws: record.draws,
        seed: record.nodes[0].seed.clone(),
        rng: "splitmix64".to_string(),
        draw_order: DRAW_ORDER.to_string(),
        independence_assumed: false,
    });
    explanation.result = Some(super::explanation::EstimateResult {
        p25_sec: p25,
        p50_sec: p50,
        p75_sec: p75,
        p90_sec: Some(p90),
        eta_p50_at: as_of + Duration::seconds(p50),
        samples_min,
        // The base's marks describe its own path, not the composed one.
        stage_marks: Vec::new(),
        tail_extrapolated,
    });
    explanation.dependencies = Some(record);
    explanation.enforce_cap();
    explanation
}

/// One `(child, source)` read of the live edge pass.
pub type ReadKey = (NodeKey, EdgeSource);

/// The edges and parent landings the daemon has observed (#10510), kept
/// across passes by the tracker and filled only by the caller's forge reads
/// (`observability::eta_dependency`), never by the tracker itself. An
/// edge keeps the instant it was **first** observed as its `known_at`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DependencyBook {
    /// Every edge currently observed.
    pub edges: Vec<Edge>,
    /// Parents observed closed, and when.
    pub landed: BTreeMap<NodeKey, DateTime<Utc>>,
    /// When each `(child, source)` was last read.
    read_at: BTreeMap<(NodeKey, EdgeSource), DateTime<Utc>>,
}

impl DependencyBook {
    /// Replace `child`'s edges from `source` with edges to `parents`,
    /// observed at `observed_at`. An edge already present keeps its
    /// `known_at`; one no longer read is dropped.
    pub fn set(
        &mut self,
        child: &NodeKey,
        source: EdgeSource,
        parents: &[NodeKey],
        observed_at: DateTime<Utc>,
    ) {
        let mut kept = Vec::with_capacity(self.edges.len());
        let mut known: BTreeMap<NodeKey, DateTime<Utc>> = BTreeMap::new();
        for edge in std::mem::take(&mut self.edges) {
            if &edge.child == child && edge.source == source {
                known.insert(edge.parent, edge.known_at);
            } else {
                kept.push(edge);
            }
        }
        for parent in parents {
            kept.push(Edge {
                child: child.clone(),
                parent: parent.clone(),
                source,
                known_at: known.get(parent).copied().unwrap_or(observed_at),
            });
        }
        self.edges = kept;
        self.read_at.insert((child.clone(), source), observed_at);
    }

    /// Record that `parent` closed at `at`.
    pub fn set_landed(&mut self, parent: &NodeKey, at: DateTime<Utc>) {
        self.landed.insert(parent.clone(), at);
    }

    /// The `(child, source)` pairs due a read at `at`: never read first,
    /// then the oldest read past `refresh_secs`, at most `budget`.
    #[must_use]
    pub fn due(
        &self,
        candidates: &[ReadKey],
        at: DateTime<Utc>,
        refresh_secs: i64,
        budget: usize,
    ) -> Vec<ReadKey> {
        let mut due: Vec<(Option<DateTime<Utc>>, &ReadKey)> = candidates
            .iter()
            .map(|c| (self.read_at.get(c).copied(), c))
            .filter(|(read, _)| read.is_none_or(|r| (at - r).num_seconds() >= refresh_secs))
            .collect();
        due.sort();
        due.into_iter()
            .take(budget)
            .map(|(_, c)| c.clone())
            .collect()
    }

    /// Keep only the edges (and reads) of `children`, and the landings of
    /// parents an edge still names.
    pub fn retain(&mut self, children: &BTreeSet<NodeKey>) {
        self.edges.retain(|e| children.contains(&e.child));
        self.read_at
            .retain(|(child, _), _| children.contains(child));
        let named: BTreeSet<&NodeKey> = self.edges.iter().map(|e| &e.parent).collect();
        self.landed.retain(|parent, _| named.contains(parent));
    }
}
