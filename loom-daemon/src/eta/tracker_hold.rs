//! The operator hold as an **overlay** on the pooled `merge_wait` track
//! (#10218).
//!
//! # Why an overlay, not a stage transition
//!
//! Every path-engine heuristic reads `merge_wait` as pooled: from the
//! approval to the merge, hold included. So the tracker keeps the pooled `merge_wait`
//! track exactly as before and records the hold beside it:
//!
//! - **Entering the hold** never closes the pooled track. It opens the overlay
//!   and journals a boundary-only `label.transition` row (`stage: merge_wait`,
//!   `left_at`, `next_stage: merge_hold`, no `duration_sec`, no
//!   `censored_sec`): an exit in journal-derived drain counts, never a
//!   [`crate::eta::history::StageSample`].
//! - **Leaving the hold** closes the overlay with a `stage: merge_hold` row,
//!   carrying `duration_sec` when its entry was observed (a `censored_sec`
//!   lower bound when the labels stopped resolving to any stage). The pooled
//!   track then carries on as it always has: back to `merge_wait` changes
//!   nothing, `doctor` closes the pooled `merge_wait` exactly as before.
//! - **A merge or close while held** writes the pooled row unchanged plus a
//!   `merge_hold` row: completed at the merge, censored on a close.
//! - **Journal-derived drain and merge counts** (#10201) read each hold end
//!   once: the `merge_hold` row is the departure (and the merge), and the
//!   pooled `merge_wait` row at the same instant (doctor, close, merge) is
//!   not counted again. A held PR that leaves the listings still open is a
//!   `merge_hold` departure, never a merge.
//!
//! While held the item keeps `refused = blocked`, as it had before #10218,
//! so the item's emit signature — and with it when a path-engine refusal is
//! emitted and that it is not refreshed — is unchanged. [`held_stage`]
//! reports `At(MergeHold)`, entered at the overlay, which every path-engine
//! heuristic refuses as `blocked` with a byte-identical explanation. After a
//! release the item is `At(MergeWait)` with the **pooled** entry, and
//! [`episode_entered_at`] is the release instant, for a hold-aware
//! heuristic's split age. `merge_hold` is never pushed into the item's
//! observed stages, so `eta.outcome`'s `stages_actual` is unchanged.
//!
//! # Two views of a held item (#10284)
//!
//! A heuristic that [`models_hold`](crate::eta::Heuristic::models_hold)
//! (the shadow `land-2026-10-04-twin-otter` and its `-b`) estimates a held
//! PR from its fit's `merge_hold`, which was trained on real queue counts.
//! So the tracker builds two inputs for a held item:
//!
//! - the **described** view ([`described`]), every other heuristic's: the
//!   `blocked` refusal's features, its stage-dependent queue counts omitted
//!   as `no_stage`, byte for byte as before;
//! - the **modeled** view ([`Tracker::modeled_input`]), the hold-aware
//!   heuristics': the same input with its features recomputed for
//!   `At(MergeHold)` entered at the hold, so the queue counts come from the
//!   same `queue_features` call training makes for a `merge_hold` row. Its
//!   explanation records those features, so a replay reads what the model
//!   read.
//!
//! Each hold-aware series of a held item has its own emit signature,
//! `merge_hold` with no refusal reason ([`held_signature`]): the hold's entry
//! and release are transitions of that series, it refreshes every
//! `refresh_secs` under the hourly cap like any answered series, and a
//! refusal of its own (`no_model` until a fit lands) refreshes too, so a fit
//! that arrives mid-hold is picked up within one interval. Its Monte Carlo
//! seed is the hold visit's (`visit_entry` is the hold entry while held), so
//! refreshes within one visit replay the same uniforms and a re-entry after
//! a release draws new ones. On the fleet roster a tracked held PR is in
//! `merge_hold` from its hold entry ([`roster_entry`]), as training's split
//! episode is.
//!
//! # The one deliberate change to a shipped stage's local samples
//!
//! `review_wait` → approved-and-held within one listing interval used to keep
//! `review_wait` open for the whole hold and close it, verdict `pass`, at the
//! release. It now closes at the observation of the approval, verdict `pass`
//! with its attempt, exactly as `review_wait → merge_wait` does and as the
//! forge's PL1 measures it; the pooled `merge_wait` and the overlay open at
//! the same instant. A verdict an in-sweep `judge` phase left pending is
//! likewise settled when the hold is seen rather than at its release (the row
//! is the same, written earlier).
//!
//! # Not covered
//!
//! An item that ends without a listing or a PR read while the overlay is open
//! (an in-sweep merge, an issue close) drops the overlay with the item, with
//! no `merge_hold` row. The overlay is reported only while the pooled track
//! is `merge_wait`.

use super::{Effects, EstimateContext, Item, ItemKey, PrState, PrView, StageTrack, Tracker};
use crate::eta::emit::Signature;
use crate::eta::journal::JournalEntry;
use crate::eta::labels::stage_from_pr_labels;
use crate::eta::{
    AgeSource, CurrentStage, CurrentState, EstimateInput, Kind, NoEstimateReason, Stage,
};
use chrono::{DateTime, Utc};

/// One item's hold overlay.
#[derive(Debug, Clone, Default)]
pub(super) struct Hold {
    /// The running hold, while the PR is in `merge_hold`.
    open: Option<StageTrack>,
    /// When the last hold was lifted back to `merge_wait`.
    released_at: Option<DateTime<Utc>>,
}

/// The item's current stage when it is held: `merge_hold`, entered at the
/// overlay. `None` when it is not held, or the pooled track has left
/// `merge_wait` by a path this overlay does not watch.
pub(super) fn held_stage(item: &Item, now: DateTime<Utc>) -> Option<CurrentStage> {
    let open = item.hold.open.as_ref()?;
    if item.stage.as_ref().map(|s| s.stage) != Some(Stage::MergeWait) {
        return None;
    }
    Some(CurrentStage {
        stage: Stage::MergeHold,
        entered_at: Some(open.entered_at),
        age_sec: (now - open.entered_at).num_seconds().max(0),
        age_source: open.source,
        rework_rounds: item.rework_rounds,
        episode_entered_at: None,
    })
}

/// [`CurrentStage::episode_entered_at`] for an item that is not held: the
/// release instant, for a PR back in the `merge_wait` it was approved into.
pub(super) fn episode_entered_at(item: &Item) -> Option<DateTime<Utc>> {
    let pooled = item.stage.as_ref()?;
    item.hold
        .released_at
        .filter(|at| pooled.stage == Stage::MergeWait && *at > pooled.entered_at)
}

/// The state an item's described `features` (#10201) describe. A held item
/// is the `blocked` refusal it was before #10218, so its stage-dependent
/// queue features stay omitted as `no_stage` and every path-engine
/// heuristic's refusal of it, `features` included, is byte-identical to its
/// refusal of a held PR before the stage existed. A heuristic that models
/// the hold reads the modeled view instead ([`Tracker::modeled_input`]).
pub(super) fn described(current: &CurrentState) -> CurrentState {
    match current {
        CurrentState::At(held) if held.stage == Stage::MergeHold => {
            CurrentState::Refused(NoEstimateReason::Blocked)
        }
        other => other.clone(),
    }
}

/// The emit signature of a hold-aware series of a held item: `merge_hold`,
/// unrefused, so it refreshes on the ordinary cadence.
pub(super) fn held_signature(item: &Item) -> Signature {
    Signature {
        stage: Some(Stage::MergeHold),
        rework_rounds: item.rework_rounds,
        reason: None,
    }
}

/// When a tracked `item` the fleet listing shows in `stage` entered it: its
/// hold entry for `merge_hold`, else its pooled track's entry when that is
/// in `stage`. `None` when the tracker does not follow it there.
pub(super) fn roster_entry(item: &Item, stage: Stage) -> Option<DateTime<Utc>> {
    match (stage, &item.hold.open) {
        (Stage::MergeHold, Some(open)) => Some(open.entered_at),
        _ => item
            .stage
            .as_ref()
            .filter(|track| track.stage == stage)
            .map(|track| track.entered_at),
    }
}

/// [`roster_entry`] as a hold-aware model's training sees it (#10312): a
/// tracked `merge_wait` PR released from a hold enters at its release, the
/// split episode's entry. Every other PR is [`roster_entry`].
pub(super) fn episode_roster_entry(item: &Item, stage: Stage) -> Option<DateTime<Utc>> {
    match stage {
        Stage::MergeWait => episode_entered_at(item).or_else(|| roster_entry(item, stage)),
        _ => roster_entry(item, stage),
    }
}

impl Tracker {
    /// The modeled view of an item's `described` input, when `kind` has a
    /// heuristic that models the hold, with its series signature: the
    /// hold's ([`held_signature`]) for a held item, `None` (the item's own)
    /// for a `merge_wait` item (#10312), whose queue position the episode
    /// roster and its episode entry give. `None` for any other input.
    pub(super) fn modeled_input(
        &self,
        key: &ItemKey,
        item: &Item,
        kind: Kind,
        described: &EstimateInput,
        ctx: &EstimateContext<'_>,
    ) -> Option<(EstimateInput, Option<Signature>)> {
        let CurrentState::At(current) = &described.current else {
            return None;
        };
        let signature = match current.stage {
            Stage::MergeHold => Some(held_signature(item)),
            Stage::MergeWait => None,
            _ => return None,
        };
        if !ctx.registry.for_kind(kind).any(|h| h.models_hold()) {
            return None;
        }
        let mut modeled = described.clone();
        (modeled.features, modeled.features_omitted) =
            self.recorded_features(key, item, &described.current, ctx, described.as_of, true);
        Some((modeled, signature))
    }
}

#[cfg(test)]
impl Tracker {
    /// The `land` input `key` would be estimated from at `now`: what the
    /// hold tests assert `CurrentState` on.
    pub(crate) fn land_input(
        &self,
        key: &ItemKey,
        ctx: &EstimateContext<'_>,
        now: DateTime<Utc>,
    ) -> Option<EstimateInput> {
        let item = self.items.get(key)?;
        self.input_for(key, item, Kind::Land, ctx, now)
    }

    /// The `land` input a hold-aware heuristic reads for `key` at `now`: the
    /// modeled view when the item is held, else [`Self::land_input`].
    pub(crate) fn hold_aware_land_input(
        &self,
        key: &ItemKey,
        ctx: &EstimateContext<'_>,
        now: DateTime<Utc>,
    ) -> Option<EstimateInput> {
        let item = self.items.get(key)?;
        let described = self.input_for(key, item, Kind::Land, ctx, now)?;
        Some(match self.modeled_input(key, item, Kind::Land, &described, ctx) {
            Some((modeled, _)) => modeled,
            None => described,
        })
    }
}

impl Tracker {
    /// The listing hook: called for each PR before the pooled stage logic.
    /// Closes the overlay when the labels no longer say `merge_hold`, and
    /// returns `false` so the pooled logic runs as it always has. When they
    /// do say `merge_hold`, handles the PR completely and returns `true`.
    pub(super) fn hold_listing(
        &mut self,
        key: &ItemKey,
        pr: &PrView,
        is_new: bool,
        now: DateTime<Utc>,
        resolution_sec: i64,
        effects: &mut Effects,
    ) -> bool {
        let resolved = stage_from_pr_labels(&pr.labels);
        if resolved != Ok(Stage::MergeHold) {
            let next = resolved.ok();
            let event = "label.transition";
            let closed = self.close_hold(key, next, next.is_some(), now, event, resolution_sec);
            if let Some(mut row) = closed {
                row.raw = serde_json::json!({"labels": pr.labels});
                effects.journal.push(row);
            }
            return false;
        }
        self.enter_hold(key, pr, is_new, now, resolution_sec, effects);
        true
    }

    /// A PR that left the review listings while held: the `merge_hold` row,
    /// completed at a merge, censored at `now` otherwise.
    pub(super) fn hold_resolved(
        &mut self,
        key: &ItemKey,
        state: PrState,
        now: DateTime<Utc>,
    ) -> Option<JournalEntry> {
        let (at, completed, name) = match state {
            PrState::Merged(at) => (at, true, "merged"),
            PrState::Closed => (now, false, "closed"),
            PrState::Open => (now, false, "open"),
        };
        let mut row = self.close_hold(key, None, completed, at, "pr.resolved", 0)?;
        row.raw = serde_json::json!({"pr": row.pr_number, "state": name});
        Some(row)
    }

    /// Close the open overlay at `at`, if there is one. `next` is the stage
    /// the labels resolve to now (`None` for a merge, a close or no stage).
    fn close_hold(
        &mut self,
        key: &ItemKey,
        next: Option<Stage>,
        completed: bool,
        at: DateTime<Utc>,
        event: &str,
        resolution_sec: i64,
    ) -> Option<JournalEntry> {
        let item = self.items.get_mut(key)?;
        let open = item.hold.open.take()?;
        if next == Some(Stage::MergeWait) {
            item.hold.released_at = Some(at);
        }
        let item = item.clone();
        let mut row = self.row(event, &item, at);
        row.stage = Some(Stage::MergeHold);
        row.left_at = Some(at);
        row.next_stage = next;
        row.resolution_sec = Some(resolution_sec);
        if open.exact {
            let sec = (at - open.entered_at).num_seconds().max(0);
            row.entered_at = Some(open.entered_at);
            if completed {
                row.duration_sec = Some(sec);
            } else {
                row.censored_sec = Some(sec);
            }
        }
        Some(row)
    }

    /// The labels say `merge_hold`: the pooled track moves as it would to
    /// `merge_wait`, the item stays refused `blocked`, and the overlay opens
    /// once the pooled track is in `merge_wait`.
    fn enter_hold(
        &mut self,
        key: &ItemKey,
        pr: &PrView,
        is_new: bool,
        now: DateTime<Utc>,
        resolution_sec: i64,
        effects: &mut Effects,
    ) {
        let raw = serde_json::json!({"labels": pr.labels});
        let mark = effects.journal.len();
        let item = self
            .items
            .get_mut(key)
            .unwrap_or_else(|| unreachable!("the listing created it"));
        let was_refused = item.refused.replace(NoEstimateReason::Blocked);
        if is_new || item.stage.is_none() && item.verdict_pending_since.is_none() {
            // First sight while held: both tracks start at the `updated_at`
            // lower bound, so neither ever yields a duration.
            let track = StageTrack {
                stage: Stage::MergeWait,
                entered_at: pr.updated_at.unwrap_or(now).min(now),
                source: AgeSource::UpdatedAtLowerBound,
                exact: false,
            };
            item.hold.open = Some(StageTrack {
                stage: Stage::MergeHold,
                ..track.clone()
            });
            item.stage = Some(track);
            let item = item.clone();
            let mut row = self.row("label.first_seen", &item, now);
            row.next_stage = Some(Stage::MergeHold);
            row.raw = serde_json::json!({"labels": pr.labels, "updated_at": pr.updated_at});
            effects.journal.push(row);
            effects.dirty.push(key.clone());
            return;
        }

        let pending = item.verdict_pending_since;
        let sweep_drives = item.sweep_running && pending.is_none();
        let current = item.stage.as_ref().map(|s| s.stage);
        let attempt = item.rework_rounds + 1;
        if pending.is_none() && !sweep_drives && current == Some(Stage::ReviewWait) {
            // The external approval below is a Judge verdict: note it in the
            // item's facts as the pooled path does (#10231).
            item.facts.note_verdict(attempt, true, now);
        }
        let mut boundary = true;
        if let Some(since) = pending {
            // The labels settle a pending in-sweep verdict: approved.
            let mut row = self.settle_verdict(key, Stage::MergeWait, "pass", since, true, now);
            row.raw = raw.clone();
            effects.journal.push(row);
        } else if !sweep_drives && current != Some(Stage::MergeWait) {
            // An external approval observed together with the hold: the
            // stage it closes goes straight to the hold.
            let mut row = self.transition(
                key,
                Some(Stage::MergeWait),
                now,
                AgeSource::TrackerObserved,
                "label.transition",
                false,
                resolution_sec,
            );
            row.next_stage = Some(Stage::MergeHold);
            if current == Some(Stage::ReviewWait) {
                row.verdict = Some("pass".to_string());
                row.attempt = Some(attempt);
            }
            row.raw = raw.clone();
            effects.journal.push(row);
            boundary = false;
        }

        let item = self
            .items
            .get_mut(key)
            .unwrap_or_else(|| unreachable!("the listing created it"));
        let pooled_merge_wait = item.stage.as_ref().map(|s| s.stage) == Some(Stage::MergeWait);
        if pooled_merge_wait && item.hold.open.is_none() {
            item.hold.open = Some(StageTrack {
                stage: Stage::MergeHold,
                entered_at: now,
                source: AgeSource::TrackerObserved,
                exact: true,
            });
            if boundary {
                let item = item.clone();
                let mut row = self.row("label.transition", &item, now);
                row.stage = Some(Stage::MergeWait);
                row.left_at = Some(now);
                row.next_stage = Some(Stage::MergeHold);
                row.resolution_sec = Some(resolution_sec);
                row.raw = raw;
                effects.journal.push(row);
            }
        }
        if was_refused != Some(NoEstimateReason::Blocked) || effects.journal.len() > mark {
            effects.dirty.push(key.clone());
        }
    }
}
