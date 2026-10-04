//! The point-in-time dataset: logged estimates and landings, keyed by the
//! instant each became knowable, and the fold rows built from them.

use crate::eta::explanation::{Explanation, FeatureOmitted, Features};
use crate::eta::score::OutcomeKind;
use crate::eta::{Kind, NoEstimateReason, Stage};
use crate::telemetry::kinds::eta::EtaOutcomeRecord;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;

/// One logged line as exported from the telemetry store: the instant the
/// daemon logged it (`observed_timestamp`), the event name, and the record
/// body. The body may be the record's JSON object or that JSON as a string
/// (how a log store keeps it).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LoggedLine {
    /// When the record was logged. The knowable-at instant is this plus the
    /// arrival margin.
    pub observed_at: DateTime<Utc>,
    /// `eta.estimate` or `eta.outcome`.
    pub event: String,
    /// The record body: an [`Explanation`] for an estimate, an
    /// [`EtaOutcomeRecord`] for an outcome.
    pub body: serde_json::Value,
}

/// A logged estimate.
#[derive(Debug, Clone, PartialEq)]
pub struct LoggedEstimate {
    /// When it became knowable.
    pub knowable_at: DateTime<Utc>,
    /// The estimate as logged.
    pub explanation: Explanation,
}

/// A logged resolution of an issue's `land` prediction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LandingEvent {
    /// `owner/repo#issue`.
    pub story: String,
    /// When it happened.
    pub actual_at: DateTime<Utc>,
    /// When it became knowable.
    pub knowable_at: DateTime<Utc>,
    /// `Landed` or `Abandoned`.
    pub outcome: OutcomeKind,
}

/// Records [`Logged::ingest`] did not keep, by reason.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Skipped {
    /// Estimates of a kind other than `land`.
    pub not_land: usize,
    /// Records whose build provenance is incomplete (accuracy queries
    /// exclude them).
    pub incomplete_provenance: usize,
    /// Outcomes that are neither a landing nor an abandonment.
    pub other_outcome: usize,
    /// Lines whose event is neither `eta.estimate` nor `eta.outcome`.
    pub other_event: usize,
    /// Lines whose body did not parse.
    pub unparsable: usize,
}

/// Everything logged, keyed by knowable-at.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Logged {
    /// `land` estimates.
    pub estimates: Vec<LoggedEstimate>,
    /// Landings and abandonments, one per distinct `(story, actual_at,
    /// outcome)`, at the earliest instant any outcome record made it known.
    pub events: Vec<LandingEvent>,
    /// What was dropped.
    pub skipped: Skipped,
}

fn parse_body<T: serde::de::DeserializeOwned>(body: &serde_json::Value) -> Option<T> {
    match body {
        serde_json::Value::String(s) => serde_json::from_str(s).ok(),
        other => serde_json::from_value(other.clone()).ok(),
    }
}

impl Logged {
    /// Ingest exported lines. `margin` is added to each line's
    /// `observed_at` to give its knowable-at instant.
    #[must_use]
    pub fn ingest(lines: &[LoggedLine], margin: Duration) -> Self {
        let mut logged = Logged::default();
        let mut events: BTreeMap<(String, DateTime<Utc>, u8), LandingEvent> = BTreeMap::new();
        for line in lines {
            let knowable_at = line.observed_at + margin;
            match line.event.as_str() {
                "eta.estimate" => {
                    let Some(explanation) = parse_body::<Explanation>(&line.body) else {
                        logged.skipped.unparsable += 1;
                        continue;
                    };
                    if explanation.kind != Kind::Land {
                        logged.skipped.not_land += 1;
                    } else if !explanation.loom.complete {
                        logged.skipped.incomplete_provenance += 1;
                    } else {
                        logged.estimates.push(LoggedEstimate {
                            knowable_at,
                            explanation,
                        });
                    }
                }
                "eta.outcome" => {
                    let Some(record) = parse_body::<EtaOutcomeRecord>(&line.body) else {
                        logged.skipped.unparsable += 1;
                        continue;
                    };
                    let tag = match record.score.outcome {
                        OutcomeKind::Landed => 0,
                        OutcomeKind::Abandoned => 1,
                        _ => {
                            logged.skipped.other_outcome += 1;
                            continue;
                        }
                    };
                    if record.estimate.kind != Kind::Land {
                        logged.skipped.not_land += 1;
                        continue;
                    }
                    if !record.loom.complete {
                        logged.skipped.incomplete_provenance += 1;
                        continue;
                    }
                    let story = format!("{}#{}", record.estimate.repo, record.estimate.issue);
                    let key = (story.clone(), record.score.actual_at, tag);
                    let event = LandingEvent {
                        story,
                        actual_at: record.score.actual_at,
                        knowable_at,
                        outcome: record.score.outcome,
                    };
                    events
                        .entry(key)
                        .and_modify(|e| {
                            if knowable_at < e.knowable_at {
                                e.knowable_at = knowable_at;
                            }
                        })
                        .or_insert(event);
                }
                _ => logged.skipped.other_event += 1,
            }
        }
        logged.events = events.into_values().collect();
        logged
    }

    /// The fold rows of every estimate with `as_of ∈ [from, until)`, as
    /// they could have been assembled at `horizon`: records knowable at or
    /// after `horizon` are dropped **before** anything is grouped or
    /// labelled, and every label is censored at `horizon`.
    ///
    /// For a training set, `until == horizon == cutoff`. For a validation
    /// set, `horizon` is the instant its outcomes are observed through.
    #[must_use]
    pub fn rows(
        &self,
        from: DateTime<Utc>,
        until: DateTime<Utc>,
        horizon: DateTime<Utc>,
    ) -> Vec<Row> {
        let mut known_events: BTreeMap<&str, Vec<&LandingEvent>> = BTreeMap::new();
        for event in self.events.iter().filter(|e| e.knowable_at < horizon) {
            known_events
                .entry(event.story.as_str())
                .or_default()
                .push(event);
        }
        let mut snapshots: BTreeMap<(String, DateTime<Utc>), Snapshot> = BTreeMap::new();
        for logged in &self.estimates {
            let e = &logged.explanation;
            if logged.knowable_at >= horizon || e.as_of < from || e.as_of >= until {
                continue;
            }
            let key = (e.subject.story.clone(), e.as_of);
            let snapshot = snapshots
                .entry(key)
                .or_insert_with(|| Snapshot::new(logged));
            snapshot.absorb(logged);
        }
        snapshots
            .into_values()
            .map(|snapshot| {
                let known = known_events
                    .get(snapshot.story.as_str())
                    .map_or(&[][..], Vec::as_slice);
                let label = label_for(&snapshot, known, horizon);
                Row { snapshot, label }
            })
            .collect()
    }
}

/// One heuristic's answer for a snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "answer")]
pub enum Answer {
    /// Remaining-time quartiles, seconds from `as_of`.
    Quantiles {
        /// 25th percentile.
        p25: i64,
        /// Median.
        p50: i64,
        /// 75th percentile.
        p75: i64,
    },
    /// No estimate.
    Refused {
        /// Why.
        reason: Option<NoEstimateReason>,
    },
}

impl Answer {
    /// `(p25, p50, p75)` when answered.
    #[must_use]
    pub fn quantiles(self) -> Option<(i64, i64, i64)> {
        match self {
            Answer::Quantiles { p25, p50, p75 } => Some((p25, p50, p75)),
            Answer::Refused { .. } => None,
        }
    }
}

/// Every logged estimate of one issue at one instant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    /// `owner/repo#issue`.
    pub story: String,
    /// `owner/repo`.
    pub repo: String,
    /// Issue number.
    pub issue: u32,
    /// The instant estimated.
    pub as_of: DateTime<Utc>,
    /// The latest knowable-at of the estimates grouped here.
    pub knowable_at: DateTime<Utc>,
    /// Current stage, when the estimate had one.
    pub stage: Option<Stage>,
    /// Seconds already spent in it.
    pub age_sec: Option<i64>,
    /// Judge rejections already taken.
    pub rework_rounds: Option<u32>,
    /// Features logged live at `as_of`.
    pub features: Option<Features>,
    /// Why features are null.
    pub features_omitted: Vec<FeatureOmitted>,
    /// Each heuristic's answer, by id.
    pub heuristics: BTreeMap<String, Answer>,
}

impl Snapshot {
    fn new(logged: &LoggedEstimate) -> Self {
        let e = &logged.explanation;
        Snapshot {
            story: e.subject.story.clone(),
            repo: e.subject.repo.clone(),
            issue: e.subject.issue,
            as_of: e.as_of,
            knowable_at: logged.knowable_at,
            stage: e.current_stage.as_ref().map(|c| c.stage),
            age_sec: e.current_stage.as_ref().map(|c| c.age_sec),
            rework_rounds: e.current_stage.as_ref().map(|c| c.rework_rounds),
            features: e.features.clone(),
            features_omitted: e.features_omitted.clone(),
            heuristics: BTreeMap::new(),
        }
    }

    fn absorb(&mut self, logged: &LoggedEstimate) {
        let e = &logged.explanation;
        if logged.knowable_at > self.knowable_at {
            self.knowable_at = logged.knowable_at;
        }
        let answer = match e.quantiles() {
            Some((p25, p50, p75)) => Answer::Quantiles { p25, p50, p75 },
            None => Answer::Refused {
                reason: e.no_estimate_reason,
            },
        };
        self.heuristics.insert(e.heuristic.clone(), answer);
    }
}

/// What a row's remaining time is known to be at its horizon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "label")]
pub enum Label {
    /// Landed `remaining_sec` after `as_of`, known by `knowable_at`.
    Landed {
        /// Seconds from `as_of` to the landing.
        remaining_sec: i64,
        /// When the landing became knowable (before the horizon).
        knowable_at: DateTime<Utc>,
    },
    /// Not known to have landed by `at`: the remaining time exceeds
    /// `elapsed_sec = at − as_of`.
    Censored {
        /// The horizon it is censored at.
        at: DateTime<Utc>,
        /// `at − as_of`, seconds.
        elapsed_sec: i64,
    },
    /// Closed as not planned: counted, never fitted or scored.
    Abandoned {
        /// When the abandonment became knowable (before the horizon).
        knowable_at: DateTime<Utc>,
    },
}

/// A snapshot and its label at the row set's horizon.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Row {
    /// What was known at `as_of`.
    pub snapshot: Snapshot,
    /// What was known about the outcome at the horizon.
    pub label: Label,
}

/// The issue's next resolution after `as_of` among its events known before
/// `horizon` (`known`), or censoring at `horizon`.
fn label_for(snapshot: &Snapshot, known: &[&LandingEvent], horizon: DateTime<Utc>) -> Label {
    let next = known
        .iter()
        .filter(|e| e.actual_at > snapshot.as_of)
        .min_by_key(|e| (e.actual_at, e.outcome == OutcomeKind::Abandoned));
    match next {
        Some(e) if e.outcome == OutcomeKind::Abandoned => Label::Abandoned {
            knowable_at: e.knowable_at,
        },
        Some(e) => Label::Landed {
            remaining_sec: (e.actual_at - snapshot.as_of).num_seconds(),
            knowable_at: e.knowable_at,
        },
        None => Label::Censored {
            at: horizon,
            elapsed_sec: (horizon - snapshot.as_of).num_seconds(),
        },
    }
}

/// A row that is not point-in-time with respect to its horizon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeakError {
    /// The offending row's story.
    pub story: String,
    /// Its `as_of`.
    pub as_of: DateTime<Utc>,
    /// The horizon it was checked against.
    pub horizon: DateTime<Utc>,
    /// What leaked.
    pub what: &'static str,
}

impl fmt::Display for LeakError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "temporal leak: {} at {} — {} (horizon {})",
            self.story, self.as_of, self.what, self.horizon
        )
    }
}

impl std::error::Error for LeakError {}

/// Assert that nothing in `rows`, features or outcome, is knowable at or
/// after `horizon`, and that every censored label is censored exactly there.
pub fn assert_point_in_time(rows: &[Row], horizon: DateTime<Utc>) -> Result<(), LeakError> {
    for row in rows {
        let s = &row.snapshot;
        let leak = |what| LeakError {
            story: s.story.clone(),
            as_of: s.as_of,
            horizon,
            what,
        };
        if s.as_of >= horizon {
            return Err(leak("estimate instant is at or after the horizon"));
        }
        if s.knowable_at >= horizon {
            return Err(leak("estimate is knowable at or after the horizon"));
        }
        if let Some(f) = &s.features {
            let read = [
                f.repo_friction_observed_at,
                f.pr_friction_observed_at,
                f.issue_created_at,
                f.pr_created_at,
            ];
            if read.iter().flatten().any(|t| *t > s.as_of) {
                return Err(leak("a feature was read after the estimate instant"));
            }
        }
        match row.label {
            Label::Landed {
                knowable_at,
                remaining_sec,
            } => {
                if knowable_at >= horizon {
                    return Err(leak("landing is knowable at or after the horizon"));
                }
                if remaining_sec <= 0 {
                    return Err(leak("landing does not follow the estimate"));
                }
            }
            Label::Abandoned { knowable_at } if knowable_at >= horizon => {
                return Err(leak("abandonment is knowable at or after the horizon"));
            }
            Label::Censored { at, elapsed_sec } => {
                if at != horizon || elapsed_sec != (horizon - s.as_of).num_seconds() {
                    return Err(leak("label is not censored at the horizon"));
                }
            }
            Label::Abandoned { .. } => {}
        }
    }
    Ok(())
}
