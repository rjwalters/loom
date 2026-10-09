//! `loom-daemon eta explain` core (#10930): why an estimate says what it
//! says, from its logged explanation alone.
//!
//! Pure. Three views over an [`Explanation`]:
//!
//! - [`inputs`] names the perturbable inputs the replay
//!   ([`run_explanation`]) reads, one adapter per [`ReplayEngine`] plus the
//!   wrappers every engine shares (the regime factor);
//! - [`marginal`] perturbs them one at a time through [`run_explanation`] and
//!   reports each one's `Δp50` / `Δp90`. The seed is the explanation's own
//!   (`combination.seed`, `queue.seed`), so the result is deterministic;
//! - [`diff`] swaps each input that changed from estimate `a` to estimate `b`
//!   into `a`, one at a time, ranks the swaps by `|Δp50|`, and reports the
//!   **residual**: the part of `p50(b) − p50(a)` no single swap explains
//!   (interactions, a different history, a different seed). The swaps are
//!   not additive, so the residual is printed, never hidden.
//!
//! The recorded context [`Features::INPUT_VECTOR`] is listed by [`context`]
//! and compared by [`diff`], but it is not perturbed: no replay reads it
//! (a fitted heuristic reads its own copy in `twin_otter.input`).
//!
//! A dependency composition's node inputs are not perturbable yet; only its
//! wrapper inputs are.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::explanation::{
    Explanation, Features, ReplayEngine, CONDITIONING_TRUNCATE, TRUNCATED_FEATURES,
};
use super::grid;
use super::heuristics::LITTLE_V0_MAX_SHAPE;
use super::simulate::run_explanation;
use super::Stage;

/// Schema tag of an [`ExplainReport`] and a [`Diff`].
pub const EXPLAIN_SCHEMA: &str = "eta-explain/v1";

/// How an input is perturbed for [`marginal`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputKind {
    /// Whole seconds: `+max(10%, 60 s)`.
    Seconds,
    /// A count: `+1`.
    Count,
    /// A flag: flipped.
    Flag,
    /// A probability: `+0.1` (`−0.1` at 1).
    Probability,
    /// A positive real (rate, factor, hours): `×1.1` (`+0.1` at 0).
    Real,
}

impl InputKind {
    /// The perturbed value of `value`.
    #[must_use]
    pub fn perturb(self, value: f64) -> f64 {
        match self {
            InputKind::Seconds => value + (value * 0.1).round().max(60.0),
            InputKind::Count => value + 1.0,
            InputKind::Flag => {
                if value == 0.0 {
                    1.0
                } else {
                    0.0
                }
            }
            InputKind::Probability => {
                if value >= 1.0 {
                    round6(value - 0.1)
                } else {
                    round6((value + 0.1).min(1.0))
                }
            }
            InputKind::Real => {
                if value == 0.0 {
                    0.1
                } else {
                    round6(value * 1.1)
                }
            }
        }
    }
}

/// One input the replay reads.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NamedInput {
    /// Dotted path into the explanation, e.g. `queue.items_ahead`.
    pub name: String,
    /// The recorded value (a flag reads `0` / `1`).
    pub value: f64,
    /// How [`marginal`] perturbs it.
    pub kind: InputKind,
    /// The value [`marginal`] replays with.
    pub perturbed: f64,
}

/// One input's marginal contribution.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Marginal {
    /// The input.
    #[serde(flatten)]
    pub input: NamedInput,
    /// `p50(perturbed) − p50`, seconds. `None` when the perturbed record did
    /// not replay (e.g. an age beyond the stage's history).
    pub dp50_sec: Option<i64>,
    /// `p90(perturbed) − p90`, seconds.
    pub dp90_sec: Option<i64>,
}

/// The JSON pointer of a dotted input name.
fn pointer(name: &str) -> String {
    format!("/{}", name.replace('.', "/"))
}

/// The value at `name` as a number (a flag as `0`/`1`).
fn number(root: &Value, name: &str) -> Option<f64> {
    match root.pointer(&pointer(name))? {
        Value::Bool(b) => Some(f64::from(u8::from(*b))),
        Value::Number(n) => n.as_f64(),
        _ => None,
    }
}

fn round6(x: f64) -> f64 {
    (x * 1_000_000.0).round() / 1_000_000.0
}

/// Twin-otter input fields that are 0/1 flags.
const TWIN_OTTER_FLAGS: [&str; 6] = [
    "op_hold",
    "sequenced",
    "starred",
    "conflict",
    "ci_fail",
    "blocked",
];

/// Twin-otter input fields measured in hours.
const TWIN_OTTER_HOURS: [&str; 2] = ["age_h", "since_merge_h"];

/// The perturbable inputs `explanation`'s replay reads, in a fixed order.
/// Empty for a refusal or an explanation the cap left unreplayable.
#[must_use]
pub fn inputs(explanation: &Explanation) -> Vec<NamedInput> {
    if explanation.result.is_none() || explanation.replay_lost() {
        return Vec::new();
    }
    let Ok(root) = serde_json::to_value(explanation) else {
        return Vec::new();
    };
    let mut slots: Vec<(String, InputKind)> = Vec::new();
    let mut push = |name: &str, kind| slots.push((name.to_string(), kind));
    match explanation.replay_engine() {
        ReplayEngine::Path => {
            let start = explanation.path.as_ref().map(|p| p.start);
            if start.is_some_and(|s| s != Stage::ReadyWait) {
                push("current_stage.age_sec", InputKind::Seconds);
            }
            push("current_stage.rework_rounds", InputKind::Count);
            push("path.include_merge", InputKind::Flag);
            if explanation
                .path
                .as_ref()
                .is_some_and(|p| p.dispatch.is_some())
            {
                push("path.dispatch.ahead", InputKind::Count);
                push("path.dispatch.free_slots", InputKind::Count);
            }
            if explanation.stalled.as_ref().is_some_and(|s| s.applied) {
                push("stalled.term_sec", InputKind::Seconds);
            }
            if let Some(b) = &explanation.branches {
                for i in 0..b.changes_requested.p_by_attempt.len() {
                    push(
                        &format!("branches.changes_requested.p_by_attempt.{i}"),
                        InputKind::Probability,
                    );
                }
            }
        }
        ReplayEngine::Queue => {
            push("queue.items_ahead", InputKind::Count);
            push("queue.drain_rate_per_hr", InputKind::Real);
            push("queue.exits", InputKind::Count);
            push("queue.service_total_sec", InputKind::Seconds);
        }
        ReplayEngine::TwinOtter => {
            if let Some(Value::Object(fields)) = root.pointer("/twin_otter/input") {
                for (field, value) in fields {
                    let kind = if TWIN_OTTER_FLAGS.contains(&field.as_str()) {
                        InputKind::Flag
                    } else if TWIN_OTTER_HOURS.contains(&field.as_str()) {
                        InputKind::Real
                    } else if value.is_u64() {
                        InputKind::Count
                    } else {
                        continue;
                    };
                    push(&format!("twin_otter.input.{field}"), kind);
                }
            }
        }
        ReplayEngine::HeldHeron => {
            push("held_heron.spell_age_sec", InputKind::Seconds);
        }
        ReplayEngine::Dependency => {}
    }
    push("regime_adjustment.factor", InputKind::Real);
    slots
        .into_iter()
        .filter_map(|(name, kind)| {
            let value = number(&root, &name)?;
            Some(NamedInput {
                perturbed: kind.perturb(value),
                name,
                value,
                kind,
            })
        })
        .collect()
}

/// `explanation` with input `name` set to `value`, and every field the
/// recorder derives from it re-derived the way the heuristic did
/// (the current stage's conditioning, the dispatch turnovers, the queue's
/// Gamma shape). `None` when `name` is not a number in the record.
#[must_use]
pub fn with_input(explanation: &Explanation, name: &str, value: f64) -> Option<Explanation> {
    let mut root = serde_json::to_value(explanation).ok()?;
    let slot = root.pointer_mut(&pointer(name))?;
    *slot = match slot {
        Value::Bool(_) => Value::Bool(value != 0.0),
        Value::Number(n) if n.is_u64() => Value::from(value.round().max(0.0) as u64),
        Value::Number(n) if n.is_i64() => Value::from(value.round() as i64),
        Value::Number(_) => Value::from(value),
        _ => return None,
    };
    let mut out: Explanation = serde_json::from_value(root).ok()?;
    rederive(&mut out, name);
    Some(out)
}

/// Re-derive what the recorder computed from input `name`.
fn rederive(e: &mut Explanation, name: &str) {
    match name {
        "current_stage.age_sec" => {
            let Some(age) = e.current_stage.as_ref().map(|c| c.age_sec) else {
                return;
            };
            let start = e.path.as_ref().map(|p| p.start);
            let Some(first) = e.stages.first_mut().filter(|s| Some(s.stage) == start) else {
                return;
            };
            if age <= 0 {
                first.conditioning = None;
                return;
            }
            if first.distribution.grid_sec.len() != grid::GRID_POINTS {
                return;
            }
            let f_age = round6(grid::cdf(&first.distribution.grid_sec, age));
            match &mut first.conditioning {
                Some(c) => {
                    c.age_sec = age;
                    c.f_age = f_age;
                }
                None => {
                    first.conditioning = Some(super::explanation::Conditioning {
                        age_sec: age,
                        f_age,
                        n_above: 0,
                        method: CONDITIONING_TRUNCATE.to_string(),
                    });
                }
            }
        }
        "path.dispatch.ahead" | "path.dispatch.free_slots" => {
            if let Some(d) = e.path.as_mut().and_then(|p| p.dispatch.as_mut()) {
                d.turnovers = d.input.turnovers();
                d.admission_delay_sec = d.input.admission_delay_sec();
            }
        }
        "queue.items_ahead" | "queue.drain_rate_per_hr" | "queue.exits" => {
            if let Some(q) = &mut e.queue {
                q.gamma_shape = q.exits.clamp(1, LITTLE_V0_MAX_SHAPE);
                q.wait_sec = if q.items_ahead == 0 {
                    0
                } else {
                    super::heuristics::wait_sec(q.items_ahead, q.drain_rate_per_hr)
                };
            }
        }
        _ => {}
    }
}

/// `p50` and `p90` of a replay.
fn p50_p90(q: (i64, i64, i64, i64)) -> (i64, i64) {
    (q.1, q.3)
}

/// Each input's marginal contribution, ranked by `|Δp50|` (largest first,
/// then by input order). Empty when the explanation does not replay.
#[must_use]
pub fn marginal(explanation: &Explanation) -> Vec<Marginal> {
    let Some(base) = run_explanation(explanation).map(p50_p90) else {
        return Vec::new();
    };
    let mut out: Vec<Marginal> = inputs(explanation)
        .into_iter()
        .map(|input| {
            let moved = with_input(explanation, &input.name, input.perturbed)
                .and_then(|e| run_explanation(&e))
                .map(p50_p90);
            Marginal {
                dp50_sec: moved.map(|(p50, _)| p50 - base.0),
                dp90_sec: moved.map(|(_, p90)| p90 - base.1),
                input,
            }
        })
        .collect();
    out.sort_by_key(|m| std::cmp::Reverse(m.dp50_sec.map_or(-1, i64::abs)));
    out
}

/// The recorded context ([`Features::INPUT_VECTOR`]) that has a value, in
/// that order. Not read by any replay.
#[must_use]
pub fn context(explanation: &Explanation) -> Vec<(String, Value)> {
    let Some(features) = &explanation.features else {
        return Vec::new();
    };
    let Ok(Value::Object(map)) = serde_json::to_value(features) else {
        return Vec::new();
    };
    Features::INPUT_VECTOR
        .iter()
        .filter_map(|name| {
            map.get(*name)
                .filter(|v| !v.is_null())
                .map(|v| ((*name).to_string(), v.clone()))
        })
        .collect()
}

/// One changed input's swap in a [`Diff`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Swap {
    /// The input.
    pub name: String,
    /// Its value in `a`.
    pub from: f64,
    /// Its value in `b`.
    pub to: f64,
    /// `p50(a with b's value) − p50(a)`, seconds; `None` when that record did
    /// not replay.
    pub dp50_sec: Option<i64>,
    /// The same for p90.
    pub dp90_sec: Option<i64>,
}

/// A recorded-context field that differs between `a` and `b`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextChange {
    /// The feature.
    pub name: String,
    /// In `a` (`null` when absent).
    pub from: Value,
    /// In `b`.
    pub to: Value,
}

/// What moved estimate `a`'s p50 to estimate `b`'s.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Diff {
    /// [`EXPLAIN_SCHEMA`].
    pub schema: String,
    /// `a`'s estimate id.
    pub a: String,
    /// `b`'s estimate id.
    pub b: String,
    /// Whether both replay through the same engine; when not, only the
    /// shared wrapper inputs are swapped and the residual carries the rest.
    pub same_engine: bool,
    /// `a`'s replayed p50 and p90.
    pub a_p50_sec: i64,
    /// `a`'s replayed p90.
    pub a_p90_sec: i64,
    /// `b`'s replayed p50.
    pub b_p50_sec: i64,
    /// `b`'s replayed p90.
    pub b_p90_sec: i64,
    /// Every changed input, ranked by `|Δp50|`.
    pub swaps: Vec<Swap>,
    /// `(p50(b) − p50(a)) − Σ swaps' Δp50`: interactions and everything that
    /// is not a named input (history, seed, model).
    pub residual_p50_sec: i64,
    /// The same for p90.
    pub residual_p90_sec: i64,
    /// Inputs only `a` has.
    pub only_in_a: Vec<String>,
    /// Inputs only `b` has.
    pub only_in_b: Vec<String>,
    /// Recorded context that changed (not read by the replay).
    pub context_changed: Vec<ContextChange>,
}

/// Explain the move from `a` to `b`. `None` when either does not replay.
#[must_use]
pub fn diff(a: &Explanation, b: &Explanation) -> Option<Diff> {
    let (a_p50, a_p90) = run_explanation(a).map(p50_p90)?;
    let (b_p50, b_p90) = run_explanation(b).map(p50_p90)?;
    let ia = inputs(a);
    let ib = inputs(b);
    let mut swaps: Vec<Swap> = ia
        .iter()
        .filter_map(|x| {
            let y = ib.iter().find(|y| y.name == x.name)?;
            (x.value != y.value).then(|| {
                let moved = with_input(a, &x.name, y.value)
                    .and_then(|e| run_explanation(&e))
                    .map(p50_p90);
                Swap {
                    name: x.name.clone(),
                    from: x.value,
                    to: y.value,
                    dp50_sec: moved.map(|(p50, _)| p50 - a_p50),
                    dp90_sec: moved.map(|(_, p90)| p90 - a_p90),
                }
            })
        })
        .collect();
    swaps.sort_by_key(|s| std::cmp::Reverse(s.dp50_sec.map_or(-1, i64::abs)));
    let explained50: i64 = swaps.iter().filter_map(|s| s.dp50_sec).sum();
    let explained90: i64 = swaps.iter().filter_map(|s| s.dp90_sec).sum();
    let only = |xs: &[NamedInput], ys: &[NamedInput]| -> Vec<String> {
        xs.iter()
            .filter(|x| !ys.iter().any(|y| y.name == x.name))
            .map(|x| x.name.clone())
            .collect()
    };
    let ca = context(a);
    let cb = context(b);
    let context_changed = Features::INPUT_VECTOR
        .iter()
        .filter_map(|name| {
            let get = |c: &[(String, Value)]| {
                c.iter()
                    .find(|(n, _)| n == name)
                    .map_or(Value::Null, |(_, v)| v.clone())
            };
            let (from, to) = (get(&ca), get(&cb));
            (from != to).then(|| ContextChange {
                name: (*name).to_string(),
                from,
                to,
            })
        })
        .collect();
    Some(Diff {
        schema: EXPLAIN_SCHEMA.to_string(),
        a: a.estimate_id.clone(),
        b: b.estimate_id.clone(),
        same_engine: a.replay_engine() == b.replay_engine(),
        a_p50_sec: a_p50,
        a_p90_sec: a_p90,
        b_p50_sec: b_p50,
        b_p90_sec: b_p90,
        residual_p50_sec: (b_p50 - a_p50) - explained50,
        residual_p90_sec: (b_p90 - a_p90) - explained90,
        only_in_a: only(&ia, &ib),
        only_in_b: only(&ib, &ia),
        swaps,
        context_changed,
    })
}

/// Whether the replay reproduced the recorded quantiles.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Parity {
    /// All four quantiles equal.
    Exact,
    /// The replay answered a different number: a bug, always.
    Mismatch,
    /// The replay could not run; `reason` says why.
    NotReplayable {
        /// `replayable_reason`, or what is missing.
        reason: String,
    },
    /// A refusal: there is nothing to replay.
    NoEstimate,
}

/// One stage of the per-stage breakdown.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StageRow {
    /// The stage.
    pub stage: Stage,
    /// First entry, p50, seconds from `as_of`.
    pub entry_p50_sec: Option<i64>,
    /// Time in the stage, p50, seconds (`stage_predictions` only).
    pub dwell_p50_sec: Option<i64>,
    /// Share of the p50 total: `alloc / p50` from `stage_predictions`, else
    /// `contributions.p50_share`.
    pub p50_share: Option<f64>,
    /// Percentage of paths that visit it (`stage_predictions` only).
    pub reach_pct: Option<u8>,
}

/// The whole explain report for one explanation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExplainReport {
    /// [`EXPLAIN_SCHEMA`].
    pub schema: String,
    /// The estimate id.
    pub estimate_id: String,
    /// The heuristic.
    pub heuristic: String,
    /// `start`, `finish` or `land`.
    pub kind: String,
    /// The instant described.
    pub as_of: chrono::DateTime<chrono::Utc>,
    /// `owner/repo#N`.
    pub subject: String,
    /// The replay engine.
    pub engine: String,
    /// The recorded p25/p50/p75/p90, seconds.
    pub recorded: Option<[i64; 4]>,
    /// The replayed ones.
    pub replayed: Option<[i64; 4]>,
    /// Whether they agree.
    pub parity: Parity,
    /// What the size cap dropped.
    pub truncated: Vec<String>,
    /// `stage_predictions` (#10929) when present, else `stage_marks` and
    /// `contributions`.
    pub breakdown_source: Option<String>,
    /// The per-stage breakdown.
    pub stages: Vec<StageRow>,
    /// Each replay input's marginal contribution ([`marginal`]).
    pub inputs: Vec<Marginal>,
    /// The recorded context ([`context`]).
    pub context: serde_json::Map<String, Value>,
}

/// The per-stage breakdown and where it came from.
fn breakdown(e: &Explanation) -> (Option<String>, Vec<StageRow>) {
    if !e.stage_predictions.is_empty() {
        let p50 = e.result.as_ref().map_or(0, |r| r.p50_sec);
        let rows = e
            .stage_predictions
            .iter()
            .map(|(stage, p)| StageRow {
                stage: *stage,
                entry_p50_sec: Some(p.entry_p50),
                dwell_p50_sec: Some(p.dwell_p50),
                p50_share: (p50 > 0).then(|| super::round3(p.alloc as f64 / p50 as f64)),
                reach_pct: Some(p.reach_pct),
            })
            .collect();
        return (Some("stage_predictions".to_string()), rows);
    }
    let Some(result) = e.result.as_ref().filter(|r| !r.stage_marks.is_empty()) else {
        return (None, Vec::new());
    };
    let share = |stage: Stage| {
        e.contributions
            .as_ref()
            .and_then(|c| c.p50_share.get(stage.as_str()).copied())
    };
    let rows = result
        .stage_marks
        .iter()
        .filter(|m| m.p50_at.is_some())
        .map(|m| StageRow {
            stage: m.stage,
            entry_p50_sec: m.p50_at.map(|t| (t - e.as_of).num_seconds()),
            dwell_p50_sec: None,
            p50_share: share(m.stage),
            reach_pct: None,
        })
        .collect();
    (Some("stage_marks".to_string()), rows)
}

fn quad(q: (i64, i64, i64, i64)) -> [i64; 4] {
    [q.0, q.1, q.2, q.3]
}

/// Replay `explanation`, check parity, and explain it.
#[must_use]
pub fn report(explanation: &Explanation) -> ExplainReport {
    let recorded = explanation.quantiles_with_p90();
    let replayed = run_explanation(explanation);
    let parity = match (recorded, replayed) {
        (None, _) if explanation.result.is_none() => Parity::NoEstimate,
        (None, _) => Parity::NotReplayable {
            reason: "recorded before result.p90_sec existed".to_string(),
        },
        (Some(r), Some(p)) if r == p => Parity::Exact,
        (Some(_), Some(_)) => Parity::Mismatch,
        (Some(_), None) => Parity::NotReplayable {
            reason: explanation.replayable_reason.clone().unwrap_or_else(|| {
                if explanation.truncated.is_empty() || explanation.truncated == [TRUNCATED_FEATURES]
                {
                    "the record lacks a field the replay reads".to_string()
                } else {
                    format!("truncated: {}", explanation.truncated.join(", "))
                }
            }),
        },
    };
    let (breakdown_source, stages) = breakdown(explanation);
    let subject = &explanation.subject;
    ExplainReport {
        schema: EXPLAIN_SCHEMA.to_string(),
        estimate_id: explanation.estimate_id.clone(),
        heuristic: explanation.heuristic.clone(),
        kind: explanation.kind.as_str().to_string(),
        as_of: explanation.as_of,
        subject: format!("{}#{}", subject.repo, subject.issue),
        engine: explanation.replay_engine().as_str().to_string(),
        recorded: recorded.map(quad),
        replayed: replayed.map(quad),
        parity,
        truncated: explanation.truncated.clone(),
        breakdown_source,
        stages,
        inputs: marginal(explanation),
        context: context(explanation).into_iter().collect(),
    }
}
