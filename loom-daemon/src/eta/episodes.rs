//! Stage episodes (#10218): the stages one PR passed through, as consecutive,
//! non-overlapping intervals derived from its label history. This is the
//! record a hold-aware estimator fits (#10221's per-stage dwell times and
//! next-stage counts), and the only place the **split** `merge_wait` and
//! `merge_hold` exist.
//!
//! # Why a record of its own
//!
//! Every path-engine heuristic reads `merge_wait` as **pooled**: a
//! [`super::history::StageSample`] runs from the approval in force at merge to
//! the merge, operator hold included, and must keep doing so (`land-v2` is
//! `current` and the shadow baseline). An episode instead ends whenever the
//! labels resolve to a different stage, so an approved PR that was held reads
//! `merge_wait → merge_hold → merge_wait → merged`: the hold-free wait and the
//! hold, each with its own duration and exit. Keeping the two definitions in
//! two records is what lets the new one exist without moving the old one.
//!
//! # Input: normalized label events
//!
//! Every source produces one [`EpisodeInput`] per PR: its label changes, each
//! with the instant `at` and a source sequence number `seq`, plus how the PR
//! ended ([`PrEnd`]).
//!
//! - **Forge timeline** ([`input_from_pr_history`]): `seq` is the event's
//!   index in the timeline, which `PrHistory::new` sorts with a *stable* sort,
//!   so same-second events keep the API's order.
//! - **Label stream** (later, #10197's webhook mirror): `seq` is delivery
//!   order. Only the adapter is new; the derivation below is shared.
//!
//! # The replay, and why it is deterministic
//!
//! Events are taken in `(at, seq)` order. Starting from an empty label set,
//! **every event at one instant is applied in `seq` order, and the stage is
//! resolved once for that instant** with
//! [`super::labels::stage_from_pr_labels`], the one label → stage definition
//! (the tracker and every later consumer call the same function). Hence:
//!
//! - the result is a function of the set of `(at, seq, event)`; the order the
//!   input arrives in does not matter;
//! - no episode has zero length: two label changes in the same second (a
//!   `--remove-label --add-label` edit) are one transition;
//! - `seq` decides only between changes to the **same** label at the same
//!   instant: `unlabeled X` then `labeled X` leaves `X` applied, the reverse
//!   leaves it removed;
//! - a label re-applied while already in force changes no resolution, so it
//!   opens no new episode.
//!
//! # Output
//!
//! [`StageEpisode`] `{ repo, pr_number, stage, entered_at, end }`. `stage` is
//! one of `review_wait`, `doctor`, `merge_wait` (split) and `merge_hold`;
//! `end` is [`EpisodeEnd::Left`] (`next` is a stage, `merged` or `closed`),
//! [`EpisodeEnd::Unstaged`] (the labels stopped resolving to any stage: a
//! non-operator hold, contradictory or no review labels) or
//! [`EpisodeEnd::Open`] (still running at the cut, censored there). The
//! daemon's `doctor` is #10221's `doctor_wait`.
//!
//! # Cuts, and leak-freedom
//!
//! [`derive`] at `as_of` reads only events with `at < as_of` and an end
//! instant before `as_of`. Each instant's resolution depends only on events
//! at or before it, so the derivation is **causal**: cut at any `T`, every
//! episode that ended before `T` is identical to the full derivation's, and an
//! episode running at `T` has the same `entered_at`. [`StageEpisode::view_at`]
//! reconstructs that cut-at-`T` view of a stored episode without its events,
//! which is how a snapshot built today replays at an earlier instant.
//!
//! The one exception is a PR closed unmerged whose source has no close
//! instant ([`PrEnd::Closed`] with `None`): its last episode ends
//! [`EpisodeEnd::Unstaged`] at the PR's last label event, which a cut before
//! the close cannot know. The forge adapter reads `closedAt` so this does not
//! arise from it.
//!
//! # Knowability
//!
//! Episodes store the events' own instants. Each consumer applies its own
//! knowability lag (#10221 adds two minutes); none is baked in here.

use super::labels::stage_from_pr_labels;
use super::Stage;
use crate::pr_latency::history::{PrEvent, PrHistory, PrState};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// One label change, as every source normalizes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabelEvent {
    /// When it happened.
    pub at: DateTime<Utc>,
    /// The source's order: the tie-break between events at the same instant.
    pub seq: u64,
    /// The label.
    pub label: String,
    /// `true` for applied, `false` for removed.
    pub added: bool,
}

/// How a PR ended, as far as its source knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrEnd {
    /// Not merged or closed.
    Open,
    /// Merged at.
    Merged(DateTime<Utc>),
    /// Closed without merging, at the instant when the source knows it.
    Closed(Option<DateTime<Utc>>),
}

/// One PR's normalized label history: the input every adapter produces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EpisodeInput {
    /// `owner/repo`.
    pub repo: String,
    /// The PR.
    pub pr_number: u32,
    /// Its label changes, in any order.
    pub events: Vec<LabelEvent>,
    /// How it ended.
    pub end: PrEnd,
}

/// Where an episode went.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(into = "String", try_from = "String")]
pub enum EpisodeNext {
    /// Another stage.
    Stage(Stage),
    /// The PR merged.
    Merged,
    /// The PR closed without merging.
    Closed,
}

impl EpisodeNext {
    /// The wire name: a stage's own name, `merged` or `closed`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            EpisodeNext::Stage(stage) => stage.as_str(),
            EpisodeNext::Merged => "merged",
            EpisodeNext::Closed => "closed",
        }
    }
}

impl From<EpisodeNext> for String {
    fn from(next: EpisodeNext) -> String {
        next.as_str().to_string()
    }
}

impl TryFrom<String> for EpisodeNext {
    type Error = String;

    fn try_from(name: String) -> Result<Self, Self::Error> {
        match name.as_str() {
            "merged" => Ok(EpisodeNext::Merged),
            "closed" => Ok(EpisodeNext::Closed),
            _ => serde_json::from_value::<Stage>(serde_json::Value::String(name.clone()))
                .map(EpisodeNext::Stage)
                .map_err(|_| format!("unknown episode exit {name:?}")),
        }
    }
}

/// How an episode ended, or that it had not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EpisodeEnd {
    /// Left at `at` for `next`.
    Left {
        /// The instant.
        at: DateTime<Utc>,
        /// Where it went.
        next: EpisodeNext,
    },
    /// At `at` the labels stopped resolving to any stage.
    Unstaged {
        /// The instant.
        at: DateTime<Utc>,
    },
    /// Still running at `at`, the cut it was derived (or viewed) at: a
    /// right-censored interval.
    Open {
        /// The cut.
        at: DateTime<Utc>,
    },
}

/// One stage visit of one PR.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StageEpisode {
    /// `owner/repo`.
    pub repo: String,
    /// The PR.
    pub pr_number: u32,
    /// The stage.
    pub stage: Stage,
    /// The instant the labels first resolved to it.
    pub entered_at: DateTime<Utc>,
    /// How it ended.
    pub end: EpisodeEnd,
}

impl StageEpisode {
    /// The instant it ended, when it has ([`EpisodeEnd::Left`] or
    /// [`EpisodeEnd::Unstaged`]).
    #[must_use]
    pub fn ended_at(&self) -> Option<DateTime<Utc>> {
        match self.end {
            EpisodeEnd::Left { at, .. } | EpisodeEnd::Unstaged { at } => Some(at),
            EpisodeEnd::Open { .. } => None,
        }
    }

    /// The last instant this episode describes: its end, or its cut.
    #[must_use]
    pub fn last_at(&self) -> DateTime<Utc> {
        match self.end {
            EpisodeEnd::Left { at, .. } | EpisodeEnd::Unstaged { at } | EpisodeEnd::Open { at } => {
                at
            }
        }
    }

    /// Where it went, when it left for a stage, a merge or a close.
    #[must_use]
    pub fn next(&self) -> Option<EpisodeNext> {
        match self.end {
            EpisodeEnd::Left { next, .. } => Some(next),
            _ => None,
        }
    }

    /// Whether the stage **completed**: it left for another stage or a merge.
    /// Every other end (closed, unstaged, still open) is a lower bound.
    #[must_use]
    pub fn completed(&self) -> bool {
        matches!(self.next(), Some(EpisodeNext::Stage(_) | EpisodeNext::Merged))
    }

    /// Whole seconds from entry to [`Self::last_at`]: the duration when
    /// [`Self::completed`], else a lower bound on it.
    #[must_use]
    pub fn duration_sec(&self) -> i64 {
        (self.last_at() - self.entered_at).num_seconds().max(0)
    }

    /// This episode as a derivation cut at `as_of` would have produced it:
    /// `None` when it had not started, itself when it ended before `as_of`,
    /// else [`EpisodeEnd::Open`] at the earlier of `as_of` and its own cut.
    #[must_use]
    pub fn view_at(&self, as_of: DateTime<Utc>) -> Option<StageEpisode> {
        if self.entered_at >= as_of {
            return None;
        }
        match self.ended_at() {
            Some(at) if at < as_of => Some(self.clone()),
            _ => Some(StageEpisode {
                end: EpisodeEnd::Open {
                    at: self.last_at().min(as_of),
                },
                ..self.clone()
            }),
        }
    }

    /// The canonical order: repo, PR, entry. One PR's episodes never share an
    /// entry instant, so this is total over a derivation's output.
    #[must_use]
    pub fn key(&self) -> (String, u32, DateTime<Utc>, Stage) {
        (self.repo.to_ascii_lowercase(), self.pr_number, self.entered_at, self.stage)
    }

    /// The one-line canonical form a snapshot id is digested over.
    #[must_use]
    pub fn digest_line(&self) -> String {
        let instant = crate::telemetry::trace::instant;
        let end = match self.end {
            EpisodeEnd::Left { at, next } => format!("left|{}|{}", instant(at), next.as_str()),
            EpisodeEnd::Unstaged { at } => format!("unstaged|{}", instant(at)),
            EpisodeEnd::Open { at } => format!("open|{}", instant(at)),
        };
        format!(
            "episode|{}|{}|{}|{}|{end}",
            self.repo.to_ascii_lowercase(),
            self.pr_number,
            self.stage.as_str(),
            instant(self.entered_at),
        )
    }
}

/// The replay every label-history derivation shares (#10245): `input`'s
/// events before `as_of` (and before its merge or close) in `(at, seq)` order,
/// from an empty label set, calling `visit` once per instant with the labels
/// in force after every event at that instant. Returns the merge or close, when
/// one is before `as_of`.
///
/// One copy of the tie rule, so the stage episodes ([`derive`]) and the label
/// flags (`flag_timeline`) cannot drift apart.
pub(crate) fn replay(
    input: &EpisodeInput,
    as_of: DateTime<Utc>,
    mut visit: impl FnMut(DateTime<Utc>, &[String]),
) -> Option<(DateTime<Utc>, EpisodeNext)> {
    let mut events: Vec<&LabelEvent> = input.events.iter().filter(|e| e.at < as_of).collect();
    events.sort_by(|a, b| a.at.cmp(&b.at).then(a.seq.cmp(&b.seq)));
    let finish = match input.end {
        PrEnd::Merged(at) if at < as_of => Some((at, EpisodeNext::Merged)),
        PrEnd::Closed(Some(at)) if at < as_of => Some((at, EpisodeNext::Closed)),
        _ => None,
    };
    // Nothing that happens at or after the merge or close moves a stage.
    if let Some((at, _)) = finish {
        events.retain(|e| e.at < at);
    }
    let mut labels: BTreeSet<String> = BTreeSet::new();
    let mut i = 0;
    while i < events.len() {
        let at = events[i].at;
        while let Some(event) = events.get(i).filter(|e| e.at == at) {
            if event.added {
                labels.insert(event.label.clone());
            } else {
                labels.remove(&event.label);
            }
            i += 1;
        }
        let present: Vec<String> = labels.iter().cloned().collect();
        visit(at, &present);
    }
    finish
}

/// Every stage episode of `input`, as knowable at `as_of`, in entry order.
#[must_use]
pub fn derive(input: &EpisodeInput, as_of: DateTime<Utc>) -> Vec<StageEpisode> {
    let episode = |stage: Stage, entered_at: DateTime<Utc>, end: EpisodeEnd| StageEpisode {
        repo: input.repo.clone(),
        pr_number: input.pr_number,
        stage,
        entered_at,
        end,
    };
    let mut current: Option<(Stage, DateTime<Utc>)> = None;
    let mut out = Vec::new();
    let mut last_event: Option<DateTime<Utc>> = None;
    let finish = replay(input, as_of, |at, present| {
        last_event = Some(at);
        let resolved = stage_from_pr_labels(present).ok();
        match (current, resolved) {
            (Some((stage, _)), Some(next)) if stage == next => {}
            (Some((stage, entered_at)), next) => {
                let end = match next {
                    Some(next) => EpisodeEnd::Left {
                        at,
                        next: EpisodeNext::Stage(next),
                    },
                    None => EpisodeEnd::Unstaged { at },
                };
                out.push(episode(stage, entered_at, end));
                current = next.map(|next| (next, at));
            }
            (None, next) => current = next.map(|next| (next, at)),
        }
    });

    if let Some((stage, entered_at)) = current {
        let end = match (finish, input.end) {
            (Some((at, next)), _) => EpisodeEnd::Left { at, next },
            // Closed with no known instant: end at the last label event (see
            // the module docs for why this one case is not causal).
            (None, PrEnd::Closed(None)) => EpisodeEnd::Unstaged {
                at: last_event.unwrap_or(entered_at),
            },
            _ => EpisodeEnd::Open { at: as_of },
        };
        out.push(episode(stage, entered_at, end));
    }
    out
}

/// The forge adapter: one PR's timeline as an [`EpisodeInput`]. `seq` is the
/// event's index in [`PrHistory::events`] (timeline order; ties keep the
/// API's order through the stable sort in `PrHistory::new`).
#[must_use]
pub fn input_from_pr_history(h: &PrHistory, repo: &str) -> EpisodeInput {
    let events = h
        .events
        .iter()
        .enumerate()
        .filter_map(|(seq, event)| {
            let (label, at, added) = match event {
                PrEvent::Labeled { label, at } => (label, at, true),
                PrEvent::Unlabeled { label, at } => (label, at, false),
                PrEvent::Pushed { .. }
                | PrEvent::Merged { .. }
                | PrEvent::Closed { .. }
                | PrEvent::Reopened { .. } => return None,
            };
            Some(LabelEvent {
                at: *at,
                seq: seq as u64,
                label: label.clone(),
                added,
            })
        })
        .collect();
    let end = match (h.state, h.merged_at) {
        (PrState::Open, _) => PrEnd::Open,
        (PrState::Merged, Some(at)) => PrEnd::Merged(at),
        (PrState::Merged | PrState::Closed, _) => PrEnd::Closed(h.closed_at),
    };
    EpisodeInput {
        repo: repo.to_string(),
        pr_number: h.number,
        events,
        end,
    }
}

/// Every stage episode of one PR's forge history, as knowable at `as_of`.
#[must_use]
pub fn episodes_from_pr_history(
    h: &PrHistory,
    repo: &str,
    as_of: DateTime<Utc>,
) -> Vec<StageEpisode> {
    derive(&input_from_pr_history(h, repo), as_of)
}
