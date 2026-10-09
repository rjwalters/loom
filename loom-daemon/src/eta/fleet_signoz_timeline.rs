//! A SigNoz-sourced timeline of PRs and issues (#10519, slice 2 of #10511):
//! label timelines, merge and close instants, CI state and queue state, as
//! knowable at one cutoff.
//!
//! # Shape (mirrors [`super::fleet_signoz`])
//!
//! - **Pure.** Nothing here reads the network. Rows come from
//!   [`super::fleet_signoz_timeline_rows`] (the query, admission and the page
//!   walk); [`Timeline::build`] turns them into a timeline. Slice 3 (#10520)
//!   makes this the primary source of fit/refresh history, with forge reads
//!   only as gap-fill.
//! - **Point-in-time.** [`Timeline::build`] keeps exactly the rows with
//!   `observed_at <= cutoff` ([`super::point_in_time::knowable_at`]) before it
//!   does anything else, so a row observed after the cutoff can change
//!   nothing, not even a dedupe decision. A row with no `observed_at` is never
//!   kept.
//! - **Deterministic.** Input order does not matter; ties break on record id.
//!
//! # The two sources
//!
//! A webhook record (an exact receipt time) reaches SigNoz once per loom-ui
//! exporter (the export and d1sync, both [`Source::Webhook`]); its copies
//! share one identity, so they are one row, never a corroboration. The daemon
//! (a polling time, up to one listing interval late) contributes `pr.resolved`
//! for a merge or close, and its stage journal's label rows
//! (`eta.stage_sample`, #10756), whole label sets.
//!
//! **Rules** (one unit test each):
//!
//! 1. **Delivery identity.** Rows with one `record_id` are one row; the first
//!    knowable is kept. A record any knowable copy flags `noop` (it changed
//!    nothing: one exporter may omit the flag) adds no event.
//! 2. **Webhook time wins.** A daemon occurrence and a webhook occurrence of
//!    the same `(repo, number, label, transition)` (or the same lifecycle
//!    event) are one event, dated by the webhook. The pair matches when the
//!    webhook time falls after the daemon's previous occurrence of that key
//!    and no later than [`MATCH_SLACK_SEC`] after this one's observation;
//!    the closest such webhook time is taken.
//! 3. **Daemon time otherwise.** A daemon occurrence with no webhook partner
//!    is kept at the daemon-observed time.
//! 4. **Dedupe.** Within one source, occurrences of one key at the same
//!    instant are one event, so a redelivered or re-exported row never
//!    counts twice.
//!
//! The daemon's label rows (stage journal rows, `eta.stage_sample`) carry the
//! whole label set, not a single change. Consecutive sets of one item are diffed into
//! changes, dated at the later set's observation. An item's first set is a
//! baseline and dates nothing: the daemon cannot know when those labels were
//! added. Every set is still kept on the item ([`ItemTimeline::label_sets`]),
//! so an item the daemon only ever saw as a baseline (or as repeated identical
//! sets) is a known item, never a missing one (#10520).
//!
//! # Coverage
//!
//! [`Coverage`] records the earliest knowable row per family and source, so a
//! caller can ask whether SigNoz covers a whole fit window
//! ([`super::WINDOW_DAYS`], 60 days on main) before it skips a forge read.

use super::fleet_signoz_timeline_rows::{
    CiDurationRow, CiJobRow, CiRunRow, ItemKey, Lifecycle, QueueEntry, Row, RowBody, Source,
    Transition,
};
use super::point_in_time::knowable_at;
use chrono::{DateTime, Duration, Utc};
use std::collections::{BTreeMap, BTreeSet};

/// How far after a daemon observation a webhook time may fall and still be
/// the same event: the webhook's receipt can trail a fast poll by seconds.
pub const MATCH_SLACK_SEC: i64 = 120;

/// One label change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabelEvent {
    pub label: String,
    pub transition: Transition,
    /// The event time: the webhook's when present, else the daemon's.
    pub at: DateTime<Utc>,
    /// When it first became knowable, from either source.
    pub observed_at: DateTime<Utc>,
    /// Whose time `at` is.
    pub source: Source,
    /// Both sources saw it.
    pub corroborated: bool,
}

/// One open, close, merge or reopen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LifecycleEvent {
    pub event: Lifecycle,
    pub at: DateTime<Utc>,
    pub observed_at: DateTime<Utc>,
    pub source: Source,
    pub corroborated: bool,
}

/// One issue's or PR's timeline.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ItemTimeline {
    /// In `(at, label)` order.
    pub labels: Vec<LabelEvent>,
    /// In `at` order.
    pub lifecycle: Vec<LifecycleEvent>,
    /// The daemon's whole label sets, `(observed_at, labels)`, ascending: the
    /// first is the baseline. They date no change of their own (the diffs
    /// between them are already in `labels`); they say the item exists, when
    /// the daemon saw it, and what it carried then.
    pub label_sets: Vec<(DateTime<Utc>, BTreeSet<String>)>,
}

impl ItemTimeline {
    /// How the item stands after its last lifecycle event: merged, closed, or
    /// `None` (open, reopened, or nothing known).
    #[must_use]
    pub fn resolution(&self) -> Option<&LifecycleEvent> {
        self.lifecycle
            .iter()
            .rfind(|e| e.event != Lifecycle::Opened)
            .filter(|e| matches!(e.event, Lifecycle::Merged | Lifecycle::Closed))
    }

    /// The merge instant, when the item is merged.
    #[must_use]
    pub fn merged_at(&self) -> Option<DateTime<Utc>> {
        self.resolution()
            .filter(|e| e.event == Lifecycle::Merged)
            .map(|e| e.at)
    }

    /// The close instant, when the item is closed without a merge.
    #[must_use]
    pub fn closed_at(&self) -> Option<DateTime<Utc>> {
        self.resolution()
            .filter(|e| e.event == Lifecycle::Closed)
            .map(|e| e.at)
    }

    /// The labels the timeline says the item carried at `at`, from its
    /// recorded changes only.
    #[must_use]
    pub fn labels_at(&self, at: DateTime<Utc>) -> BTreeSet<String> {
        let mut labels = BTreeSet::new();
        for event in self.labels.iter().filter(|e| e.at <= at) {
            match event.transition {
                Transition::Added => labels.insert(event.label.clone()),
                Transition::Removed => labels.remove(&event.label),
            };
        }
        labels
    }

    /// The labels the item carried at `at`: the latest daemon set observed by
    /// then, with the recorded changes after it applied; just
    /// [`ItemTimeline::labels_at`] when the daemon saw no set by then. Unlike
    /// `labels_at`, this knows a baseline's labels, which no change dates.
    #[must_use]
    pub fn current_labels(&self, at: DateTime<Utc>) -> BTreeSet<String> {
        let Some((seen, set)) = self.label_sets.iter().rfind(|(seen, _)| *seen <= at) else {
            return self.labels_at(at);
        };
        let mut labels = set.clone();
        for event in self.labels.iter().filter(|e| e.at > *seen && e.at <= at) {
            match event.transition {
                Transition::Added => labels.insert(event.label.clone()),
                Transition::Removed => labels.remove(&event.label),
            };
        }
        labels
    }

    /// The baseline's labels that no recorded change explains: carried at the
    /// first daemon set, yet absent from [`ItemTimeline::labels_at`] at that
    /// set's observation (plus [`MATCH_SLACK_SEC`], a webhook receipt's lag).
    /// Their addition time is unknown.
    #[must_use]
    pub fn undated_baseline_labels(&self) -> BTreeSet<String> {
        let Some((seen, set)) = self.label_sets.first() else {
            return BTreeSet::new();
        };
        let dated = self.labels_at(*seen + Duration::seconds(MATCH_SLACK_SEC));
        set.difference(&dated).cloned().collect()
    }

    /// Every instant the item became knowable to have changed or been seen:
    /// each label event, lifecycle event and daemon set, at the later of its
    /// time and its observation.
    pub fn seen_at(&self) -> impl Iterator<Item = DateTime<Utc>> + '_ {
        let label = self.labels.iter().map(|e| e.at.max(e.observed_at));
        let life = self.lifecycle.iter().map(|e| e.at.max(e.observed_at));
        let sets = self.label_sets.iter().map(|(seen, _)| *seen);
        label.chain(life).chain(sets)
    }
}

/// One CI run, with what is known of its jobs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CiRunState {
    pub run: CiRunRow,
    pub observed_at: DateTime<Utc>,
    /// Latest-known jobs of the run, by job id.
    pub jobs: Vec<CiJobRow>,
    /// `ci.duration` samples recorded for this run and attempt.
    pub duration_samples: Vec<CiDurationRow>,
}

/// CI state for one `(repo, ref)`: the latest run of each workflow.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CiState {
    /// By workflow name.
    pub latest: BTreeMap<String, CiRunState>,
}

impl CiState {
    /// Every latest run concluded `success`.
    #[must_use]
    pub fn all_green(&self) -> bool {
        !self.latest.is_empty()
            && self
                .latest
                .values()
                .all(|r| r.run.conclusion.as_deref() == Some("success"))
    }
}

/// The latest knowable ready queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueState {
    pub tick_at: DateTime<Utc>,
    pub observed_at: DateTime<Utc>,
    /// The repo's rows, by issue.
    pub entries: BTreeMap<u32, QueueEntry>,
}

/// A family of timeline rows, for [`Coverage`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Family {
    Labels,
    Lifecycle,
    Ci,
    Queue,
}

/// The earliest knowable row per `(family, source)`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Coverage {
    pub earliest: BTreeMap<(Family, Source), DateTime<Utc>>,
}

impl Coverage {
    /// The earliest knowable row of `family` from any source.
    #[must_use]
    pub fn first(&self, family: Family) -> Option<DateTime<Utc>> {
        self.earliest
            .iter()
            .filter(|((f, _), _)| *f == family)
            .map(|(_, at)| *at)
            .min()
    }

    /// Whether `family` has rows from the start of the `window_days` window
    /// that ends at `cutoff`: SigNoz alone can then answer for the window.
    #[must_use]
    pub fn covers(&self, family: Family, cutoff: DateTime<Utc>, window_days: i64) -> bool {
        let start = cutoff
            .checked_sub_signed(Duration::days(window_days))
            .unwrap_or(DateTime::<Utc>::MIN_UTC);
        self.first(family).is_some_and(|first| first <= start)
    }
}

/// What [`Timeline::build`] dropped or merged.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TimelineStats {
    /// Rows in.
    pub rows: usize,
    /// Observed after the cutoff, or never observed.
    pub not_knowable: usize,
    /// Repeats of a held `record_id` (rule 1).
    pub duplicate_records: usize,
    /// Records flagged `noop` by a knowable copy (rule 1), dropped.
    pub noop: usize,
    /// Same-source repeats of one event (rule 4), and repeats of a CI run,
    /// job or duration sample.
    pub duplicate_events: usize,
    /// Daemon occurrences matched to a webhook occurrence (rule 2).
    pub corroborated: usize,
}

/// Every item, CI ref and the queue, as knowable at `cutoff`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Timeline {
    pub cutoff: DateTime<Utc>,
    pub items: BTreeMap<ItemKey, ItemTimeline>,
    /// By `(repo, ref)`; a run with no ref is under `""`.
    pub ci: BTreeMap<(String, String), CiState>,
    pub queue: Option<QueueState>,
    pub coverage: Coverage,
    pub stats: TimelineStats,
}

/// One occurrence of an event from one source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Occurrence {
    at: DateTime<Utc>,
    observed_at: DateTime<Utc>,
}

/// One event after the sources were merged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Merged {
    at: DateTime<Utc>,
    observed_at: DateTime<Utc>,
    source: Source,
    corroborated: bool,
}

/// Rule 4: one occurrence per instant, the earliest knowable kept.
fn dedupe(mut list: Vec<Occurrence>, stats: &mut TimelineStats) -> Vec<Occurrence> {
    list.sort();
    let before = list.len();
    list.dedup_by_key(|o| o.at);
    stats.duplicate_events += before - list.len();
    list
}

/// Rules 2–4 for one key.
fn merge_sources(
    webhook: Vec<Occurrence>,
    daemon: Vec<Occurrence>,
    stats: &mut TimelineStats,
) -> Vec<Merged> {
    let webhook = dedupe(webhook, stats);
    let daemon = dedupe(daemon, stats);
    let mut taken = vec![false; webhook.len()];
    let mut out = Vec::with_capacity(webhook.len() + daemon.len());
    let mut lower: Option<DateTime<Utc>> = None;
    for d in &daemon {
        let upper = d.observed_at + Duration::seconds(MATCH_SLACK_SEC);
        let distance = |w: &Occurrence| (w.at - d.observed_at).num_milliseconds().abs();
        let partner = webhook
            .iter()
            .enumerate()
            .filter(|(i, w)| !taken[*i] && lower.is_none_or(|l| w.at > l) && w.at <= upper)
            .min_by_key(|(_, w)| (distance(w), w.at))
            .map(|(i, _)| i);
        match partner {
            Some(i) => {
                taken[i] = true;
                stats.corroborated += 1;
                out.push(Merged {
                    at: webhook[i].at,
                    observed_at: webhook[i].observed_at.min(d.observed_at),
                    source: Source::Webhook,
                    corroborated: true,
                });
            }
            None => out.push(Merged {
                at: d.at,
                observed_at: d.observed_at,
                source: Source::Daemon,
                corroborated: false,
            }),
        }
        lower = Some(lower.map_or(d.at, |l| l.max(d.at)));
    }
    for (w, _) in webhook.iter().zip(&taken).filter(|(_, taken)| !**taken) {
        out.push(Merged {
            at: w.at,
            observed_at: w.observed_at,
            source: Source::Webhook,
            corroborated: false,
        });
    }
    out
}

type Sides = (Vec<Occurrence>, Vec<Occurrence>);
/// A daemon label set, at its observation.
type ObservedSet<'a> = (DateTime<Utc>, &'a BTreeSet<String>);
/// `(repo, run_id, run_attempt)`.
type RunKey<'a> = (&'a str, u64, u32);
/// `(repo, run_id, run_attempt, job_id)`.
type DurationKey<'a> = (&'a str, u64, u32, Option<u64>);

fn push(sides: &mut Sides, source: Source, occurrence: Occurrence) {
    match source {
        Source::Webhook => sides.0.push(occurrence),
        Source::Daemon => sides.1.push(occurrence),
    }
}

impl Timeline {
    /// The timeline of `rows` as knowable at `cutoff`. Input order does not
    /// matter.
    #[must_use]
    pub fn build(rows: &[Row], cutoff: DateTime<Utc>) -> Self {
        let mut stats = TimelineStats {
            rows: rows.len(),
            ..TimelineStats::default()
        };
        // Point-in-time first: nothing observed after the cutoff takes part.
        let mut knowable: Vec<(DateTime<Utc>, &Row)> = knowable_at(rows, cutoff)
            .filter_map(|row| row.observed_at.map(|at| (at, row)))
            .collect();
        stats.not_knowable = rows.len() - knowable.len();
        knowable.sort_by(|a, b| (a.0, &a.1.record_id).cmp(&(b.0, &b.1.record_id)));
        // Rule 1. The noop flag is the record's, whichever copy carries it.
        let noop: BTreeSet<&str> = knowable
            .iter()
            .filter_map(|&(_, row)| row.noop.then_some(row.record_id.as_str()))
            .collect();
        stats.noop = noop.len();
        let mut ids = BTreeSet::new();
        knowable.retain(|&(_, row)| {
            let fresh = ids.insert(row.record_id.as_str());
            if !fresh {
                stats.duplicate_records += 1;
            }
            fresh && !noop.contains(row.record_id.as_str())
        });

        let mut coverage = Coverage::default();
        let mut labels: BTreeMap<(ItemKey, String, Transition), Sides> = BTreeMap::new();
        let mut lifecycle: BTreeMap<(ItemKey, Lifecycle), Sides> = BTreeMap::new();
        let mut sets: BTreeMap<ItemKey, Vec<ObservedSet<'_>>> = BTreeMap::new();
        // CI identities are per repo: a run id is unique only within one.
        let mut runs: BTreeMap<RunKey<'_>, (DateTime<Utc>, &CiRunRow)> = BTreeMap::new();
        let mut jobs: BTreeMap<(&str, u64), &CiJobRow> = BTreeMap::new();
        let mut durations: BTreeMap<DurationKey<'_>, &CiDurationRow> = BTreeMap::new();
        let mut queue: Option<QueueState> = None;
        for (observed_at, row) in &knowable {
            let observed_at = *observed_at;
            let family = match &row.body {
                RowBody::Label { .. } | RowBody::LabelSet { .. } => Family::Labels,
                RowBody::Lifecycle { .. } => Family::Lifecycle,
                RowBody::Queue { .. } => Family::Queue,
                _ => Family::Ci,
            };
            coverage
                .earliest
                .entry((family, row.source))
                .and_modify(|at| *at = (*at).min(observed_at))
                .or_insert(observed_at);
            match &row.body {
                RowBody::Label {
                    item,
                    label,
                    transition,
                    at,
                } => push(
                    labels
                        .entry((item.clone(), label.clone(), *transition))
                        .or_default(),
                    row.source,
                    Occurrence {
                        at: *at,
                        observed_at,
                    },
                ),
                RowBody::LabelSet { item, labels } => {
                    sets.entry(item.clone())
                        .or_default()
                        .push((observed_at, labels));
                }
                RowBody::Lifecycle { item, event, at } => push(
                    lifecycle.entry((item.clone(), *event)).or_default(),
                    row.source,
                    Occurrence {
                        at: *at,
                        observed_at,
                    },
                ),
                RowBody::CiRun(run) => {
                    let key = (row.repo.as_str(), run.run_id, run.run_attempt);
                    match runs.entry(key) {
                        std::collections::btree_map::Entry::Occupied(_) => {
                            stats.duplicate_events += 1;
                        }
                        std::collections::btree_map::Entry::Vacant(slot) => {
                            slot.insert((observed_at, run));
                        }
                    }
                }
                RowBody::CiJob(job) => {
                    // A rerun reuses no job id; the latest completion wins.
                    let key = (row.repo.as_str(), job.job_id);
                    match jobs.get(&key) {
                        Some(held) if held.completed_at >= job.completed_at => {
                            stats.duplicate_events += 1;
                        }
                        Some(_) => {
                            stats.duplicate_events += 1;
                            jobs.insert(key, job);
                        }
                        None => {
                            jobs.insert(key, job);
                        }
                    }
                }
                RowBody::CiDuration(sample) => {
                    let key = (row.repo.as_str(), sample.run_id, sample.run_attempt, sample.job_id);
                    if durations.insert(key, sample).is_some() {
                        stats.duplicate_events += 1;
                    }
                }
                RowBody::Queue { tick_at, entries } => {
                    let newer = queue
                        .as_ref()
                        .is_none_or(|q| (*tick_at, observed_at) > (q.tick_at, q.observed_at));
                    if newer {
                        queue = Some(QueueState {
                            tick_at: *tick_at,
                            observed_at,
                            entries: entries.iter().map(|e| (e.issue, e.clone())).collect(),
                        });
                    }
                }
            }
        }
        // The daemon's label sets, diffed into changes at each observation,
        // and each kept whole on its item (a baseline-only item still exists).
        let mut items: BTreeMap<ItemKey, ItemTimeline> = BTreeMap::new();
        for (item, mut list) in sets {
            list.sort_by_key(|(at, _)| *at);
            items.entry(item.clone()).or_default().label_sets =
                list.iter().map(|(at, set)| (*at, (*set).clone())).collect();
            for pair in list.windows(2) {
                let ((_, before), (at, after)) = (pair[0], pair[1]);
                let occurrence = Occurrence {
                    at,
                    observed_at: at,
                };
                for (changed, transition) in [
                    (after.difference(before), Transition::Added),
                    (before.difference(after), Transition::Removed),
                ] {
                    for label in changed {
                        push(
                            labels
                                .entry((item.clone(), label.clone(), transition))
                                .or_default(),
                            Source::Daemon,
                            occurrence,
                        );
                    }
                }
            }
        }

        for ((item, label, transition), (webhook, daemon)) in labels {
            let timeline = items.entry(item).or_default();
            for m in merge_sources(webhook, daemon, &mut stats) {
                timeline.labels.push(LabelEvent {
                    label: label.clone(),
                    transition,
                    at: m.at,
                    observed_at: m.observed_at,
                    source: m.source,
                    corroborated: m.corroborated,
                });
            }
        }
        for ((item, event), (webhook, daemon)) in lifecycle {
            let timeline = items.entry(item).or_default();
            for m in merge_sources(webhook, daemon, &mut stats) {
                timeline.lifecycle.push(LifecycleEvent {
                    event,
                    at: m.at,
                    observed_at: m.observed_at,
                    source: m.source,
                    corroborated: m.corroborated,
                });
            }
        }
        for timeline in items.values_mut() {
            timeline.labels.sort_by(|a, b| {
                (a.at, &a.label, a.transition).cmp(&(b.at, &b.label, b.transition))
            });
            timeline.lifecycle.sort_by_key(|e| (e.at, e.event));
        }

        let mut ci: BTreeMap<(String, String), CiState> = BTreeMap::new();
        for ((repo, run_id, attempt), (observed_at, run)) in &runs {
            let key = (repo.to_string(), run.git_ref.clone().unwrap_or_default());
            let state = CiRunState {
                run: (*run).clone(),
                observed_at: *observed_at,
                jobs: jobs
                    .iter()
                    .filter(|((r, _), j)| r == repo && j.run_id == *run_id)
                    .map(|(_, j)| (*j).clone())
                    .collect(),
                duration_samples: durations
                    .iter()
                    .filter(|((r, id, a, _), _)| r == repo && id == run_id && a == attempt)
                    .map(|(_, d)| (*d).clone())
                    .collect(),
            };
            let latest = &mut ci.entry(key).or_default().latest;
            let newer = latest.get(&run.workflow).is_none_or(|held| {
                (run.completed_at, run.run_id, run.run_attempt)
                    > (held.run.completed_at, held.run.run_id, held.run.run_attempt)
            });
            if newer {
                latest.insert(run.workflow.clone(), state);
            }
        }

        Timeline {
            cutoff,
            items,
            ci,
            queue,
            coverage,
            stats,
        }
    }

    /// One item's timeline.
    #[must_use]
    pub fn item(&self, key: &ItemKey) -> Option<&ItemTimeline> {
        self.items.get(key)
    }

    /// CI state for `git_ref` of the timeline's repo.
    #[must_use]
    pub fn ci_for_ref(&self, repo: &str, git_ref: &str) -> Option<&CiState> {
        self.ci
            .get(&(repo.to_ascii_lowercase(), git_ref.to_string()))
    }

    /// The issue's row in the latest knowable ready queue.
    #[must_use]
    pub fn queue_entry(&self, issue: u32) -> Option<&QueueEntry> {
        self.queue.as_ref()?.entries.get(&issue)
    }
}
