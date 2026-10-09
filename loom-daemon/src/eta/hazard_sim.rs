//! The competing-risks hold and sequencing simulator (#10523) behind
//! `land-2026-10-06-held-heron`.
//!
//! # Why
//!
//! twin-otter-b's stage-exit model draws one exit per stage. It cannot say
//! "this PR is held, and holds here are released in batches", and it cannot
//! see that a sequenced PR waits on another PR rather than on the merge
//! queue. Offline (loom-experiments#19, 52 point-in-time folds) a
//! competing-risks event simulator loses to twin-otter-b overall but wins on
//! exactly the PRs that are **held or sequenced at `as_of`**, so the hybrid
//! routes only those to it.
//!
//! # The chain
//!
//! | state | exits, each a cause-specific hazard per hour ([`Rates`]) |
//! |---|---|
//! | `merge_wait` (approved, free) | merged (`merge`), `merge_hold` (`hold_entry`), `sequenced` (`sequence_entry`) |
//! | `merge_hold` | `merge_wait` (`hold_release`), merged (`hold_merge`); both × the hour-of-day weight |
//! | `sequenced` | `merge_wait` (`desequence`), merged (`sequenced_merge`) |
//! | the current spell | as its state, but the release or de-sequence hazard is read by the spell's age |
//!
//! The current spell is the hold or sequencing the item is in at `as_of`.
//! Its release (de-sequence) hazard is piecewise constant over spell-age bins
//! ([`AGE_BOUNDS_SEC`]), each shrunk toward the pooled rate by
//! [`AGE_PRIOR_EVENTS`] pseudo-events, because a hold that has lasted three
//! days is not released at the rate a fresh one is. A later spell (a
//! re-hold, a re-sequencing) uses the pooled rate.
//!
//! **Operator availability.** A hold is ended by a human (or by a Champion
//! acting on one), so every exit from a hold is multiplied by a UTC
//! hour-of-day weight: the hour's observed hold exits over its expected ones,
//! shrunk toward 1 by [`HOUR_PRIOR_EVENTS`] and normalised to an
//! exposure-weighted mean of exactly 1, so it moves *when* releases happen,
//! not how many.
//!
//! # Rates: point in time
//!
//! Every hazard is events ÷ exposure over the [`WINDOW_DAYS`] before `as_of`
//! (twin-otter's fit window), read from what the tracker already holds:
//!
//! - the split `merge_wait` and `merge_hold` episodes (#10218), each viewed
//!   at `as_of` ([`StageEpisode::view_at`]), so an episode still open then is
//!   exposure without an event, whatever happened later;
//! - the PR label-flag timeline (#10245), changes strictly before `as_of`,
//!   for the sequenced spells inside `merge_wait`.
//!
//! The repo's own history is used when it has [`MIN_SPELL_EXITS`] exits from
//! the item's side state and [`MIN_MERGES`] free merges; else every repo in
//! the history; else there is no answer and the heuristic serves twin-otter-b.
//!
//! # The forward solution: no draws
//!
//! [`solve`] integrates the chain's forward equations out to
//! [`HORIZON_SEC`]. The hour weight and the spell's age bin are read every
//! [`STEP_SEC`]; within a step, in each of [`SUBSTEPS`] substeps, every state
//! loses `1 − e^{−qΔt}` of its mass (`q` its total exit rate), split over
//! the exits in proportion to their rates. The land quantiles are read off the
//! landed mass, interpolated within the step. Nothing is random, so a replay
//! reproduces the answer from [`HeldHeronRecord`] alone; every number it
//! reads is stored rounded to six decimals so the JSON parses back to the
//! exact value used. A quantile the chain does not reach by the horizon is
//! the horizon.
//!
//! # Not modelled
//!
//! Champion vs operator holds (the fleet snapshot has no label actors), CI
//! failures, conflicts and Doctor rounds out of `merge_wait` (they censor),
//! and the coupling of a sequenced PR to its predecessor's own ETA (#10510).

use super::episodes::{EpisodeNext, StageEpisode};
use super::flag_timeline::RepoFlagChange;
use super::history::{same_repo, Level, StageSamples};
use super::labels::FLAG_SEQUENCED;
use super::Stage;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Days of history the hazards are estimated over.
pub const WINDOW_DAYS: i64 = 14;
/// The forward solution's step, seconds.
pub const STEP_SEC: i64 = 300;
/// Substeps per [`STEP_SEC`] step (10 s each).
pub const SUBSTEPS: u32 = 30;
/// How far the forward solution runs, seconds (14 days).
pub const HORIZON_SEC: i64 = 14 * 86_400;
/// Fewest exits from the item's side state (release or merge from a hold;
/// de-sequence or merge while sequenced) a level needs.
pub const MIN_SPELL_EXITS: u32 = 5;
/// Fewest merges from a free `merge_wait` a level needs.
pub const MIN_MERGES: u32 = 5;
/// Upper bounds of the spell-age bins, seconds: 1 h, 4 h, 12 h, 24 h, 72 h
/// (the last bin is open).
pub const AGE_BOUNDS_SEC: [i64; 5] = [3_600, 14_400, 43_200, 86_400, 259_200];
/// Pseudo-events shrinking each spell-age bin's rate toward the pooled rate.
pub const AGE_PRIOR_EVENTS: f64 = 1.0;
/// Pseudo-events shrinking each hour-of-day weight toward 1.
pub const HOUR_PRIOR_EVENTS: f64 = 2.0;

/// The side state an item is in at `as_of`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SideState {
    /// An approved PR under an operator or Champion hold (`merge_hold`).
    MergeHold,
    /// An approved PR carrying `loom:sequenced`.
    Sequenced,
}

/// The chain's cause-specific hazards, events per hour.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Rates {
    /// Free `merge_wait` → merged.
    pub merge: f64,
    /// Free `merge_wait` → `merge_hold`.
    pub hold_entry: f64,
    /// Free `merge_wait` → `sequenced`.
    pub sequence_entry: f64,
    /// `merge_hold` → `merge_wait` (pooled over spell age).
    pub hold_release: f64,
    /// `merge_hold` → merged.
    pub hold_merge: f64,
    /// `sequenced` → `merge_wait` (pooled over spell age).
    pub desequence: f64,
    /// `sequenced` → merged.
    pub sequenced_merge: f64,
}

/// How many of each event the window held.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Counts {
    /// Free `merge_wait` → merged.
    pub merge: u32,
    /// Free `merge_wait` → `merge_hold`.
    pub hold_entry: u32,
    /// Free `merge_wait` → `sequenced`.
    pub sequence_entry: u32,
    /// `merge_hold` → another stage.
    pub hold_release: u32,
    /// `merge_hold` → merged.
    pub hold_merge: u32,
    /// `sequenced` → free `merge_wait`.
    pub desequence: u32,
    /// `sequenced` → merged.
    pub sequenced_merge: u32,
}

/// Exposure per state in the window, hours.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Exposure {
    /// Free (unsequenced) `merge_wait`.
    pub merge_wait_h: f64,
    /// `merge_hold`.
    pub merge_hold_h: f64,
    /// Sequenced `merge_wait`.
    pub sequenced_h: f64,
}

/// What `land-2026-10-06-held-heron` simulated: everything [`solve`] reads,
/// plus the evidence behind it. Recorded as `explanation.held_heron`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HeldHeronRecord {
    /// The side state at `as_of`.
    pub state: SideState,
    /// The history level the hazards came from: `repo` or `host`.
    pub level: String,
    /// [`WINDOW_DAYS`].
    pub window_days: i64,
    /// The hazards, events per hour.
    pub rates_per_h: Rates,
    /// The events behind them.
    pub events: Counts,
    /// The exposure behind them.
    pub exposure: Exposure,
    /// How long the current spell has lasted, when known. `None` (a
    /// sequenced spell with no flag timeline): the pooled rate is used.
    pub spell_age_sec: Option<i64>,
    /// [`AGE_BOUNDS_SEC`].
    pub age_bounds_sec: Vec<i64>,
    /// The current spell's release (de-sequence) hazard per age bin, events
    /// per hour; one more entry than `age_bounds_sec`. Empty when the age is
    /// unknown.
    pub spell_exit_by_age_per_h: Vec<f64>,
    /// The multiplier on every exit from a hold, by UTC hour (24 entries).
    pub hold_exit_by_hour: Vec<f64>,
    /// [`STEP_SEC`].
    pub step_sec: i64,
    /// [`SUBSTEPS`].
    pub substeps: u32,
    /// [`HORIZON_SEC`].
    pub horizon_sec: i64,
    /// The chain's probability of having landed by the horizon.
    pub landed_by_horizon: f64,
}

impl HeldHeronRecord {
    /// Exits from the side state the item is in: the evidence its answer
    /// most depends on (`result.samples_min`).
    #[must_use]
    pub fn spell_exits(&self) -> u32 {
        match self.state {
            SideState::MergeHold => self.events.hold_release + self.events.hold_merge,
            SideState::Sequenced => self.events.desequence + self.events.sequenced_merge,
        }
    }
}

/// [`solve`]'s answer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Solution {
    /// `(p25, p50, p75, p90)`, seconds from `as_of`.
    pub quantiles: (i64, i64, i64, i64),
    /// The landed mass at the horizon.
    pub landed_by_horizon: f64,
}

/// Round to six decimals (see the module docs).
fn round6(x: f64) -> f64 {
    (x * 1_000_000.0).round() / 1_000_000.0
}

/// One sequenced spell, `[start, end)` in Unix seconds; `end` `None` while
/// it was still open at `as_of`.
#[derive(Debug, Clone, Copy)]
struct Spell {
    start: i64,
    end: Option<i64>,
}

/// Every PR's sequenced spells from flag changes strictly before `as_of`,
/// keyed by `(lower-case repo, PR)`.
fn sequenced_spells(
    changes: &[RepoFlagChange],
    as_of: DateTime<Utc>,
) -> BTreeMap<(String, u32), Vec<Spell>> {
    let mut by_pr: BTreeMap<(String, u32), Vec<(i64, u8)>> = BTreeMap::new();
    for c in changes.iter().filter(|c| c.change.at < as_of) {
        by_pr
            .entry((c.repo.to_ascii_lowercase(), c.change.pr_number))
            .or_default()
            .push((c.change.at.timestamp(), c.change.flags));
    }
    by_pr
        .into_iter()
        .map(|(key, mut changes)| {
            changes.sort_unstable();
            let mut spells: Vec<Spell> = Vec::new();
            let mut on = false;
            for (at, flags) in changes {
                let now = flags & FLAG_SEQUENCED != 0;
                if now && !on {
                    spells.push(Spell {
                        start: at,
                        end: None,
                    });
                } else if !now && on {
                    if let Some(last) = spells.last_mut() {
                        last.end = Some(at);
                    }
                }
                on = now;
            }
            (key, spells)
        })
        .collect()
}

/// When the sequenced spell `pr` is in at `as_of` began, from the flag
/// timeline, or `None` when it is not sequenced there (or has no timeline).
#[must_use]
pub fn sequenced_since(
    history: &StageSamples,
    repo: &str,
    pr: u32,
    as_of: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    let spells = sequenced_spells(&history.flag_changes, as_of);
    let open = spells
        .get(&(repo.to_ascii_lowercase(), pr))?
        .last()
        .filter(|s| s.end.is_none())?;
    DateTime::from_timestamp(open.start, 0)
}

/// The spell-age bin of `age` seconds.
fn age_bin(age: i64) -> usize {
    AGE_BOUNDS_SEC.iter().take_while(|&&b| age >= b).count()
}

/// Seconds of `[a0, a1)` and `[b0, b1)` in common.
fn overlap(a0: i64, a1: i64, b0: i64, b1: i64) -> i64 {
    (a1.min(b1) - a0.max(b0)).max(0)
}

/// Exposure and events of one spell-age-binned exit.
#[derive(Debug, Default)]
struct AgeTally {
    exposure_sec: [i64; AGE_BOUNDS_SEC.len() + 1],
    events: [u32; AGE_BOUNDS_SEC.len() + 1],
}

impl AgeTally {
    /// Add the exposure between ages `a0` and `a1` seconds.
    fn expose(&mut self, a0: i64, a1: i64) {
        let mut lo = 0;
        for (bin, slot) in self.exposure_sec.iter_mut().enumerate() {
            let hi = AGE_BOUNDS_SEC.get(bin).copied().unwrap_or(i64::MAX);
            *slot += overlap(a0, a1, lo, hi);
            lo = hi;
        }
    }

    /// Each bin's rate per hour, shrunk toward `pooled`.
    fn rates(&self, pooled: f64) -> Vec<f64> {
        self.exposure_sec
            .iter()
            .zip(self.events)
            .map(|(&secs, events)| {
                if pooled <= 0.0 {
                    return 0.0;
                }
                let hours = secs as f64 / 3600.0;
                let prior_hours = AGE_PRIOR_EVENTS / pooled;
                round6((f64::from(events) + AGE_PRIOR_EVENTS) / (hours + prior_hours))
            })
            .collect()
    }
}

/// Everything counted over one level's window.
#[derive(Debug, Default)]
struct Tally {
    events: Counts,
    merge_wait_sec: i64,
    merge_hold_sec: i64,
    sequenced_sec: i64,
    hold_age: AgeTally,
    sequenced_age: AgeTally,
    hold_hour_sec: [i64; 24],
    hold_hour_exits: [u32; 24],
}

/// The UTC hour of Unix second `t`.
fn hour_of(t: i64) -> usize {
    t.div_euclid(3_600).rem_euclid(24) as usize
}

impl Tally {
    /// Count every episode at `level` over `[from, as_of)`.
    fn count(
        history: &StageSamples,
        repo: &str,
        level: Level,
        as_of: DateTime<Utc>,
        spells: &BTreeMap<(String, u32), Vec<Spell>>,
    ) -> Tally {
        let from = (as_of - Duration::days(WINDOW_DAYS)).timestamp();
        let mut tally = Tally::default();
        let none: Vec<Spell> = Vec::new();
        for episode in &history.episodes {
            if level == Level::Repo && !same_repo(&episode.repo, repo) {
                continue;
            }
            let Some(seen) = episode.view_at(as_of) else {
                continue;
            };
            if seen.last_at().timestamp() < from {
                continue;
            }
            match seen.stage {
                Stage::MergeWait => {
                    let key = (seen.repo.to_ascii_lowercase(), seen.pr_number);
                    let pr_spells = spells.get(&key).unwrap_or(&none);
                    tally.merge_wait(&seen, from, as_of.timestamp(), pr_spells);
                }
                Stage::MergeHold => tally.merge_hold(&seen, from),
                _ => {}
            }
        }
        tally
    }

    /// One `merge_wait` episode, split into free and sequenced time.
    fn merge_wait(&mut self, e: &StageEpisode, from: i64, as_of: i64, spells: &[Spell]) {
        let start = e.entered_at.timestamp();
        let hi = e.last_at().timestamp();
        let lo = start.max(from);
        let mut sequenced = 0;
        for spell in spells {
            let end = spell.end.unwrap_or(as_of);
            let (x0, x1) = (spell.start.max(lo), end.min(hi));
            if x1 > x0 {
                sequenced += x1 - x0;
                self.sequenced_age
                    .expose(x0 - spell.start, x1 - spell.start);
            }
            if spell.start > start && spell.start >= lo && spell.start < hi {
                self.events.sequence_entry += 1;
            }
            if let Some(x) = spell.end.filter(|&x| x > start && x >= lo && x < hi) {
                self.events.desequence += 1;
                self.sequenced_age.events[age_bin(x - spell.start)] += 1;
            }
        }
        self.sequenced_sec += sequenced;
        self.merge_wait_sec += (hi - lo).max(0) - sequenced;
        let (Some(next), Some(at)) = (e.next(), e.ended_at()) else {
            return;
        };
        let at = at.timestamp();
        if at < from {
            return;
        }
        // The state just before the exit: sequenced if a spell covers it.
        let sequenced_at_end = spells
            .iter()
            .any(|s| s.start < at && s.end.is_none_or(|x| x >= at));
        match next {
            EpisodeNext::Merged if sequenced_at_end => self.events.sequenced_merge += 1,
            EpisodeNext::Merged => self.events.merge += 1,
            EpisodeNext::Stage(Stage::MergeHold) if !sequenced_at_end => {
                self.events.hold_entry += 1;
            }
            // A hold over a sequenced PR, a Doctor round, a close: censored.
            _ => {}
        }
    }

    /// One `merge_hold` episode.
    fn merge_hold(&mut self, e: &StageEpisode, from: i64) {
        let entered = e.entered_at.timestamp();
        let hi = e.last_at().timestamp();
        let lo = entered.max(from);
        if hi > lo {
            self.merge_hold_sec += hi - lo;
            self.hold_age.expose(lo - entered, hi - entered);
            let mut t = lo;
            while t < hi {
                let next = ((t.div_euclid(3_600)) + 1) * 3_600;
                let end = next.min(hi);
                self.hold_hour_sec[hour_of(t)] += end - t;
                t = end;
            }
        }
        let (Some(next), Some(at)) = (e.next(), e.ended_at()) else {
            return;
        };
        let at = at.timestamp();
        if at < from {
            return;
        }
        match next {
            EpisodeNext::Merged => self.events.hold_merge += 1,
            EpisodeNext::Stage(Stage::MergeWait) => {
                self.events.hold_release += 1;
                self.hold_age.events[age_bin(at - entered)] += 1;
            }
            // A hold that sends the PR back to rework (`doctor`, review) is
            // not a release into the free `merge_wait` the chain models, and
            // a close ends the PR: censored, like a sequenced PR's own hold.
            EpisodeNext::Stage(_) | EpisodeNext::Closed => return,
        }
        self.hold_hour_exits[hour_of(at)] += 1;
    }

    /// Whether this tally can answer an item in `state`.
    fn sufficient(&self, state: SideState) -> bool {
        let exits = match state {
            SideState::MergeHold => self.events.hold_release + self.events.hold_merge,
            SideState::Sequenced => self.events.desequence + self.events.sequenced_merge,
        };
        exits >= MIN_SPELL_EXITS && self.events.merge >= MIN_MERGES
    }

    /// The hazards, events per hour.
    fn rates(&self) -> Rates {
        let per_h = |events: u32, secs: i64| {
            if secs <= 0 {
                0.0
            } else {
                round6(f64::from(events) / (secs as f64 / 3600.0))
            }
        };
        let c = &self.events;
        Rates {
            merge: per_h(c.merge, self.merge_wait_sec),
            hold_entry: per_h(c.hold_entry, self.merge_wait_sec),
            sequence_entry: per_h(c.sequence_entry, self.merge_wait_sec),
            hold_release: per_h(c.hold_release, self.merge_hold_sec),
            hold_merge: per_h(c.hold_merge, self.merge_hold_sec),
            desequence: per_h(c.desequence, self.sequenced_sec),
            sequenced_merge: per_h(c.sequenced_merge, self.sequenced_sec),
        }
    }

    /// The hour-of-day weights on hold exits (see the module docs).
    fn hour_weights(&self) -> Vec<f64> {
        let exits: u32 = self.hold_hour_exits.iter().sum();
        let exposure: i64 = self.hold_hour_sec.iter().sum();
        if exits == 0 || exposure <= 0 {
            return vec![1.0; 24];
        }
        let rate = f64::from(exits) / exposure as f64;
        let raw: Vec<f64> = (0..24)
            .map(|h| {
                let expected = self.hold_hour_sec[h] as f64 * rate;
                (f64::from(self.hold_hour_exits[h]) + HOUR_PRIOR_EVENTS)
                    / (expected + HOUR_PRIOR_EVENTS)
            })
            .collect();
        let weighted: f64 = (0..24).map(|h| self.hold_hour_sec[h] as f64 * raw[h]).sum();
        let scale = exposure as f64 / weighted;
        raw.into_iter().map(|w| round6(w * scale)).collect()
    }
}

/// Estimate the chain for an item of `repo` in `state` at `as_of`, whose
/// current spell has lasted `spell_age_sec` (when known). `None` when no
/// level has the evidence ([`MIN_SPELL_EXITS`], [`MIN_MERGES`]). Pure.
#[must_use]
pub fn fit(
    history: &StageSamples,
    repo: &str,
    as_of: DateTime<Utc>,
    state: SideState,
    spell_age_sec: Option<i64>,
) -> Option<HeldHeronRecord> {
    let spells = sequenced_spells(&history.flag_changes, as_of);
    let (level, tally) = [Level::Repo, Level::Host]
        .into_iter()
        .map(|level| (level, Tally::count(history, repo, level, as_of, &spells)))
        .find(|(_, tally)| tally.sufficient(state))?;
    let rates = tally.rates();
    let spell_exit_by_age_per_h = match (spell_age_sec, state) {
        (None, _) => Vec::new(),
        (Some(_), SideState::MergeHold) => tally.hold_age.rates(rates.hold_release),
        (Some(_), SideState::Sequenced) => tally.sequenced_age.rates(rates.desequence),
    };
    let hours = |secs: i64| round6(secs as f64 / 3600.0);
    Some(HeldHeronRecord {
        state,
        level: level.as_str().to_string(),
        window_days: WINDOW_DAYS,
        rates_per_h: rates,
        events: tally.events,
        exposure: Exposure {
            merge_wait_h: hours(tally.merge_wait_sec),
            merge_hold_h: hours(tally.merge_hold_sec),
            sequenced_h: hours(tally.sequenced_sec),
        },
        spell_age_sec: spell_age_sec.map(|a| a.max(0)),
        age_bounds_sec: AGE_BOUNDS_SEC.to_vec(),
        spell_exit_by_age_per_h,
        hold_exit_by_hour: tally.hour_weights(),
        step_sec: STEP_SEC,
        substeps: SUBSTEPS,
        horizon_sec: HORIZON_SEC,
        landed_by_horizon: 0.0,
    })
}

/// One state's exits over one substep of `dt_h` hours at constant `rates`:
/// the fraction of its mass that leaves (`1 − e^{−qΔt}`), and each exit's
/// share of it.
#[derive(Debug, Clone, Copy)]
struct Exits<const N: usize> {
    leaves: f64,
    shares: [f64; N],
}

impl<const N: usize> Exits<N> {
    fn new(rates: [f64; N], dt_h: f64) -> Self {
        let total: f64 = rates.iter().sum();
        if total <= 0.0 {
            return Exits {
                leaves: 0.0,
                shares: [0.0; N],
            };
        }
        Exits {
            leaves: -(-total * dt_h).exp_m1(),
            shares: rates.map(|r| r / total),
        }
    }

    /// What `mass` sends to each exit.
    fn flows(&self, mass: f64) -> [f64; N] {
        let out = mass * self.leaves;
        self.shares.map(|s| out * s)
    }
}

/// Integrate `record`'s chain from `as_of` (see the module docs). `None`
/// when the record is malformed (a hand-edited replay). Pure and
/// deterministic.
///
/// The hour-of-day weight and the current spell's age bin are read at each
/// step's start; within a step the rates are constant and the chain is
/// advanced in `substeps` equal substeps, so a path through several states
/// is delayed by at most half a substep per transition.
#[must_use]
pub fn solve(record: &HeldHeronRecord, as_of: DateTime<Utc>) -> Option<Solution> {
    let r = record.rates_per_h;
    let all = [
        r.merge,
        r.hold_entry,
        r.sequence_entry,
        r.hold_release,
        r.hold_merge,
        r.desequence,
        r.sequenced_merge,
    ];
    let by_age = &record.spell_exit_by_age_per_h;
    let valid = record.step_sec > 0
        && record.substeps > 0
        && record.horizon_sec >= record.step_sec
        && record.hold_exit_by_hour.len() == 24
        && (by_age.is_empty() || by_age.len() == record.age_bounds_sec.len() + 1)
        && all
            .iter()
            .chain(by_age)
            .chain(&record.hold_exit_by_hour)
            .all(|x| x.is_finite() && *x >= 0.0);
    if !valid {
        return None;
    }
    let spell_rate = |t: i64, pooled: f64| match record.spell_age_sec {
        Some(age) if !by_age.is_empty() => {
            let a = age + t;
            by_age[record
                .age_bounds_sec
                .iter()
                .take_while(|&&b| a >= b)
                .count()]
        }
        _ => pooled,
    };
    let step = record.step_sec;
    let sub_sec = step as f64 / f64::from(record.substeps);
    let dt_h = sub_sec / 3600.0;
    let start = as_of.timestamp();
    let free_exits = Exits::new([r.merge, r.hold_entry, r.sequence_entry], dt_h);
    let sequenced_exits = Exits::new([r.desequence, r.sequenced_merge], dt_h);
    let (mut spell, mut free, mut held, mut sequenced, mut landed) = (1.0, 0.0, 0.0, 0.0, 0.0);
    let levels = [0.25, 0.5, 0.75, 0.9];
    let mut found: [Option<i64>; 4] = [None; 4];
    for k in 0..record.horizon_sec / step {
        let t = k * step;
        let w = record.hold_exit_by_hour[hour_of(start + t)];
        let spell_exits = match record.state {
            SideState::MergeHold => {
                Exits::new([spell_rate(t, r.hold_release) * w, r.hold_merge * w], dt_h)
            }
            SideState::Sequenced => {
                Exits::new([spell_rate(t, r.desequence), r.sequenced_merge], dt_h)
            }
        };
        let held_exits = Exits::new([r.hold_release * w, r.hold_merge * w], dt_h);
        for j in 0..record.substeps {
            let [s_free, s_land] = spell_exits.flows(spell);
            let [f_land, f_hold, f_seq] = free_exits.flows(free);
            let [h_free, h_land] = held_exits.flows(held);
            let [q_free, q_land] = sequenced_exits.flows(sequenced);
            let before = landed;
            spell -= s_free + s_land;
            free += s_free + h_free + q_free - (f_land + f_hold + f_seq);
            held += f_hold - (h_free + h_land);
            sequenced += f_seq - (q_free + q_land);
            landed += s_land + f_land + h_land + q_land;
            for (slot, q) in found.iter_mut().zip(levels) {
                if slot.is_none() && landed >= q {
                    let frac = (q - before) / (landed - before);
                    let at = t as f64 + (f64::from(j) + frac) * sub_sec;
                    *slot = Some(at.round() as i64);
                }
            }
        }
    }
    let [p25, p50, p75, p90] = found.map(|q| q.unwrap_or(record.horizon_sec));
    Some(Solution {
        quantiles: (p25, p50, p75, p90),
        landed_by_horizon: round6(landed),
    })
}
