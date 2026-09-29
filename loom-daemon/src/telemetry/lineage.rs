//! `sweep.outcome`'s **attempt lineage** and **rework** vocabulary (Issue
//! #9444): `attempt_index`, `previous_sweep_id`, `trigger`, `rework_events`.
//!
//! # The gap
//!
//! When an issue takes more than one sweep — or more than one pass inside a
//! sweep — the telemetry never said **why**. Measured on the fleet store over
//! 2026-08-15..09-29: consecutive sweeps of the same issue landed within five
//! minutes of each other **7,099 times**, which looks like a retry storm, but
//! nothing linked an attempt to its predecessor or named what triggered it.
//! [`SweepDisposition`] (#9441) closed the "what did THIS sweep do" gap; it
//! cannot say "and why was it dispatched at all".
//!
//! That matters because the story-points work (#9429) needs to split the
//! effort an issue cost into three buckets, not one:
//!
//! - **clean** — the work itself;
//! - **substantive rework** — the Judge asked for real changes, so the issue
//!   is hard;
//! - **environmental rework** — main moved, the PR conflicted, an approval
//!   went stale, CI flaked, a spawn died. Cost that says nothing about the
//!   issue's size.
//!
//! Folding the last two together makes a flaky week look like a hard one.
//!
//! # Two axes, deliberately separate
//!
//! [`SweepTrigger`] answers **"why was this sweep dispatched?"** — a property
//! of the gap *between* attempts, derived from the predecessor's terminal
//! state. [`ReworkEvent`] answers **"what rework happened INSIDE this
//! sweep?"** — derived from the PR's own forge label timeline. A sweep can
//! have both (dispatched to retry a spawn death, then hit a merge conflict),
//! one, or neither.
//!
//! Every value on both axes carries a [`ReworkClass`] — or explicitly carries
//! none. `unknown` and `operator_redispatch` are *unclassified*, counted as
//! themselves: folding an unattributable retry into either rework bucket is
//! exactly the error this module exists to stop.
//!
//! # Derived, never newly instrumented
//!
//! Nothing here adds a forge call, a state file, or a dispatch-time plumb.
//! `attempt_index`/`previous_sweep_id`/`trigger` come from this host's own
//! durable `sweep.outcome` journal, read at the terminal transition;
//! `rework_events` come from the PR label timeline the same transition already
//! fetches for `doctor_cycles`/`judge_verdicts` (#8222). Being pure is what
//! makes the classification table below testable without a registry, a forge,
//! or a filesystem.

use serde::{Deserialize, Serialize};

use super::SweepResult;
use super::{disposition::is_environmental_failure_class, SweepDisposition};

/// Defensive cap on `rework_events` per record, mirroring
/// [`super::JudgeVerdict`]'s `MAX_JUDGE_VERDICTS` role: a normal PR produces a
/// handful of rework events, so this only ever truncates a pathologically
/// label-flapping timeline, never a real lifecycle.
pub const MAX_REWORK_EVENTS: usize = 32;

/// Whether a unit of rework is the **work** being hard or the **environment**
/// breaking (Issue #9444).
///
/// The whole point of the field: `substantive` rework is evidence about the
/// issue's size and belongs in a story-point estimate; `environmental` rework
/// is evidence about the fleet's health and must be excluded from one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReworkClass {
    /// The Judge asked for real changes and a Doctor made them. The issue was
    /// harder than the first pass assumed.
    Substantive,
    /// The ground moved: main advanced, the PR conflicted, an approval went
    /// stale, CI flaked, the spawn died, the account ran out. Says nothing
    /// about how hard the issue is.
    Environmental,
}

impl ReworkClass {
    /// The exact wire string this variant serializes to.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Substantive => "substantive",
            Self::Environmental => "environmental",
        }
    }
}

/// A unit of rework observed **inside** one sweep (Issue #9444).
///
/// Low-cardinality and closed by design, for the same reason
/// [`SweepDisposition`] is: a metric that groups on this must not grow new
/// buckets without a schema change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReworkKind {
    /// A Judge `loom:changes-requested` verdict that a Doctor closed the loop
    /// on (`loom:review-requested` came back). The **substantive** shape: the
    /// work needed real changes.
    Rejudge,
    /// An approval that had to be re-earned with no change request in between
    /// — `loom:pr` followed by another `loom:review-requested`. The #8248
    /// stale-required-check / stale-base shape: the base moved under an
    /// already-approved PR, so the Judge had to look again at work nobody
    /// faulted. **Environmental**.
    StaleBaseRejudge,
    /// The PR could not be merged because it conflicted with its base
    /// (`loom:merge-conflict`). **Environmental**.
    MergeConflict,
    /// CI failed on the PR and had to be re-run or fixed (`loom:ci-failure`).
    /// **Environmental**.
    CiRerun,
    /// The branch was rebased onto a moved base. **Environmental**.
    ///
    /// **Reserved, not yet emitted by any daemon path.** A clean rebase leaves
    /// no forge-visible trace, so the only site that knows it happened is the
    /// merge path's own stale-base handling in `merge-pr.sh` — a writer that
    /// needs the marker protocol tracked separately (see the schema doc's
    /// "Not yet emitted" note). The variant exists so the vocabulary, the
    /// classification table and the committed effort query are complete the
    /// day that writer lands, rather than needing a schema change then.
    Rebase,
}

impl ReworkKind {
    /// The exact wire string this variant serializes to.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Rejudge => "rejudge",
            Self::StaleBaseRejudge => "stale_base_rejudge",
            Self::MergeConflict => "merge_conflict",
            Self::CiRerun => "ci_rerun",
            Self::Rebase => "rebase",
        }
    }

    /// This kind's [`ReworkClass`] — **the classification table** the schema
    /// doc states normatively.
    ///
    /// Exactly one kind is substantive, and it is the one where a reviewer
    /// judged the *work* insufficient. Everything else is the environment: a
    /// conflict, a stale approval, a CI rerun and a rebase all happen to work
    /// that nobody faulted.
    #[must_use]
    pub fn classification(self) -> ReworkClass {
        match self {
            Self::Rejudge => ReworkClass::Substantive,
            Self::StaleBaseRejudge | Self::MergeConflict | Self::CiRerun | Self::Rebase => {
                ReworkClass::Environmental
            }
        }
    }
}

/// One unit of in-sweep rework, as it appears on `sweep.outcome`.
///
/// `classification` is serialized rather than left for the consumer to look
/// up: a dashboard querying the journal must not have to re-implement the
/// table above (and drift from it) to split a cost.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReworkEvent {
    /// What happened.
    pub kind: ReworkKind,
    /// Whether it was the work or the environment — always present, always
    /// [`ReworkKind::classification`] for `kind`.
    pub classification: ReworkClass,
    /// A short, bounded-vocabulary note on the proximate cause, when the
    /// deriving site had one (e.g. `"loom:merge-conflict"`). Never free-form
    /// agent prose: this rides a telemetry record that leaves the host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Wall-clock seconds the rework took, when both of its bounding forge
    /// events were observed. Omitted — never `0` — when the closing event was
    /// never seen (the rework is still open, or the timeline ended first).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_sec: Option<i64>,
}

impl ReworkEvent {
    /// Build an event whose `classification` is, by construction, the one the
    /// table gives for `kind` — the only constructor the deriving sites use,
    /// so a hand-set classification cannot drift from the table.
    #[must_use]
    pub fn new(kind: ReworkKind, reason: Option<String>, duration_sec: Option<i64>) -> Self {
        Self {
            kind,
            classification: kind.classification(),
            reason,
            duration_sec,
        }
    }
}

/// Why this sweep was dispatched (Issue #9444) — the axis that links an
/// attempt to its predecessor.
///
/// Closed, low-cardinality, `snake_case` on the wire, like every other enum in
/// this schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SweepTrigger {
    /// No prior attempt for this repo#issue in this host's journal. Not
    /// rework at all — this is the `clean` bucket's denominator.
    First,
    /// The previous attempt died of an environmental fault: a spawn or
    /// pre-flight death, account/credit exhaustion, a rate limit, a harness
    /// execution error, or a cancel.
    RetryAfterEnvFailure,
    /// The previous attempt failed at the work itself: the Judge rejected it,
    /// the Doctor loop was exhausted, or the Builder could not finish.
    RetryAfterSubstantiveFailure,
    /// Dispatched to act on a Judge change request — the Doctor loop.
    DoctorAfterChangesRequested,
    /// Dispatched because main moved and the branch needed a rebase.
    ///
    /// **Reserved, not yet emitted** — the paired [`ReworkKind::Rebase`] has
    /// no writer yet; see that variant's doc.
    RebaseMainMoved,
    /// Dispatched because the PR conflicted with its base.
    MergeConflict,
    /// Dispatched because an already-approved PR had to go back to the Judge
    /// with no change request against it — the base moved, or its required
    /// checks went stale (#8248).
    StaleBaseRejudge,
    /// Dispatched to fix or re-run failing CI on the PR.
    CiFailureFix,
    /// The previous attempt reached a non-failure terminal state and something
    /// re-dispatched the issue anyway.
    ///
    /// **Read this as "a decision, not a retry after a fault"** — this host's
    /// journal cannot separate an operator `dispatch_sweep` from a work-finder
    /// re-offer from the #9441 no-op re-dispatch storm, and deliberately does
    /// not guess between them. It is *not* counted as either kind of rework.
    OperatorRedispatch,
    /// A prior attempt exists but nothing observable says why this one
    /// followed it. Counted as itself, never folded into a neighbour — the
    /// same discipline [`SweepDisposition::Unknown`] applies.
    #[default]
    Unknown,
}

impl SweepTrigger {
    /// The exact wire string this variant serializes to — available without
    /// serializing, for log lines and span attributes.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::First => "first",
            Self::RetryAfterEnvFailure => "retry_after_env_failure",
            Self::RetryAfterSubstantiveFailure => "retry_after_substantive_failure",
            Self::DoctorAfterChangesRequested => "doctor_after_changes_requested",
            Self::RebaseMainMoved => "rebase_main_moved",
            Self::MergeConflict => "merge_conflict",
            Self::StaleBaseRejudge => "stale_base_rejudge",
            Self::CiFailureFix => "ci_failure_fix",
            Self::OperatorRedispatch => "operator_redispatch",
            Self::Unknown => "unknown",
        }
    }

    /// Which rework bucket a whole attempt dispatched for this reason belongs
    /// to — **the second half of the classification table**.
    ///
    /// `None` is a real answer with three distinct meanings, all of which must
    /// stay out of both rework buckets: [`Self::First`] is not rework at all,
    /// [`Self::OperatorRedispatch`] is a decision this host cannot attribute,
    /// and [`Self::Unknown`] is an admission. A consumer that coerces any of
    /// them into a bucket is inventing the number it went looking for.
    #[must_use]
    pub fn classification(self) -> Option<ReworkClass> {
        match self {
            Self::RetryAfterSubstantiveFailure | Self::DoctorAfterChangesRequested => {
                Some(ReworkClass::Substantive)
            }
            Self::RetryAfterEnvFailure
            | Self::RebaseMainMoved
            | Self::MergeConflict
            | Self::StaleBaseRejudge
            | Self::CiFailureFix => Some(ReworkClass::Environmental),
            Self::First | Self::OperatorRedispatch | Self::Unknown => None,
        }
    }
}

/// The predecessor's terminal state, as [`derive_trigger`] reads it. A borrow
/// of three fields off the previous `sweep.outcome` record — kept as a struct
/// so an added signal cannot silently shift an existing positional argument.
#[derive(Debug, Clone, Copy)]
pub struct PriorAttempt<'a> {
    /// The predecessor's `disposition` (#9441). A journal line written before
    /// that field existed decodes as [`SweepDisposition::Unknown`], which is
    /// why [`derive_trigger`] keeps a `result`-based fallback.
    pub disposition: SweepDisposition,
    /// The predecessor's terminal `result`.
    pub result: SweepResult,
    /// The predecessor's `failure_class`, when it carried one.
    pub failure_class: Option<&'a str>,
}

/// One sweep's place in its issue's attempt sequence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptLineage {
    /// 1-based count of terminal sweeps for this repo#issue in this host's
    /// journal, this one included.
    pub attempt_index: u32,
    /// The immediately preceding attempt's `sweep_id`; `None` on attempt 1.
    pub previous_sweep_id: Option<String>,
    /// Why this attempt followed that one.
    pub trigger: SweepTrigger,
}

/// Derive [`SweepTrigger`] from the predecessor's terminal state plus the
/// rework this attempt itself observed.
///
/// Precedence, strongest evidence first:
///
/// 1. **No predecessor ⇒ [`SweepTrigger::First`].** Nothing else can apply.
/// 2. **The predecessor FAILED ⇒ the retry buckets.** A fault is the proximate
///    cause of the re-dispatch, whatever this attempt later ran into: a sweep
///    retried after a spawn death that then hits a merge conflict was
///    *dispatched* for the spawn death, and the conflict is recorded on the
///    `rework_events` axis instead. Not doing this would let in-sweep rework
///    silently overwrite the dispatch reason.
/// 3. **The predecessor did NOT fail** (it landed a PR, was a no-op, or the
///    Curator disposed of it) — so the re-dispatch was a decision, and this
///    attempt's own rework is the best available name for it: a conflict, a
///    stale-base re-judge, a CI fix, or a Doctor pass. With no rework observed
///    it is [`SweepTrigger::OperatorRedispatch`], which is explicitly *not* a
///    rework bucket.
#[must_use]
pub fn derive_trigger(previous: Option<&PriorAttempt<'_>>, rework: &[ReworkEvent]) -> SweepTrigger {
    let Some(previous) = previous else {
        return SweepTrigger::First;
    };
    match previous.disposition {
        SweepDisposition::EnvFailure | SweepDisposition::Cancelled => {
            SweepTrigger::RetryAfterEnvFailure
        }
        SweepDisposition::SubstantiveFailure => SweepTrigger::RetryAfterSubstantiveFailure,
        SweepDisposition::Landed
        | SweepDisposition::NoopAlreadyDone
        | SweepDisposition::CuratorClosed
        | SweepDisposition::CuratorRescoped => {
            trigger_named_by_rework(rework).unwrap_or(SweepTrigger::OperatorRedispatch)
        }
        // `unknown` covers BOTH a pre-#9441 journal line (the field did not
        // exist, and most of the 30-day window is such lines) and a genuinely
        // unobservable #9441 outcome. `result` + `failure_class` is the
        // coarser version of the same environmental/substantive split, so fall
        // back to it rather than reporting `unknown` for every historical
        // predecessor — while still reporting `unknown` where the coarser
        // signals are themselves silent.
        SweepDisposition::Unknown => legacy_trigger(previous, rework),
    }
}

/// [`derive_trigger`] for a predecessor whose `disposition` is `unknown`.
fn legacy_trigger(previous: &PriorAttempt<'_>, rework: &[ReworkEvent]) -> SweepTrigger {
    match previous.result {
        SweepResult::Cancelled => SweepTrigger::RetryAfterEnvFailure,
        SweepResult::Success => {
            trigger_named_by_rework(rework).unwrap_or(SweepTrigger::OperatorRedispatch)
        }
        SweepResult::Failure | SweepResult::Blocked => match previous.failure_class {
            Some(class) if is_environmental_failure_class(class) => {
                SweepTrigger::RetryAfterEnvFailure
            }
            // A classifier ran and named something that is not an
            // environmental signature (`exit-<code>`, a Judge rejection): the
            // work is the remaining explanation.
            Some(_) => SweepTrigger::RetryAfterSubstantiveFailure,
            // Nothing classified the predecessor at all — the 6,373-record
            // "no phase signal" shape. Charging it to either bucket would
            // manufacture the very number this field exists to measure.
            None => SweepTrigger::Unknown,
        },
    }
}

/// The most specific trigger this attempt's own rework can name, strongest
/// first. `None` when nothing was observed.
fn trigger_named_by_rework(rework: &[ReworkEvent]) -> Option<SweepTrigger> {
    let has = |kind: ReworkKind| rework.iter().any(|event| event.kind == kind);
    if has(ReworkKind::MergeConflict) {
        Some(SweepTrigger::MergeConflict)
    } else if has(ReworkKind::Rebase) {
        Some(SweepTrigger::RebaseMainMoved)
    } else if has(ReworkKind::StaleBaseRejudge) {
        Some(SweepTrigger::StaleBaseRejudge)
    } else if has(ReworkKind::CiRerun) {
        Some(SweepTrigger::CiFailureFix)
    } else if has(ReworkKind::Rejudge) {
        Some(SweepTrigger::DoctorAfterChangesRequested)
    } else {
        None
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn prior(disposition: SweepDisposition, result: SweepResult) -> PriorAttempt<'static> {
        PriorAttempt {
            disposition,
            result,
            failure_class: None,
        }
    }

    // ------------------------------------------------------------------
    // The classification table (the #9444 acceptance criteria's core).
    // ------------------------------------------------------------------

    /// AC2: a Judge `changes-requested` → Doctor loop is **substantive**, on
    /// both axes — the in-sweep event and the trigger of the sweep dispatched
    /// to act on it.
    #[test]
    fn a_judge_changes_requested_doctor_loop_is_substantive() {
        assert_eq!(ReworkKind::Rejudge.classification(), ReworkClass::Substantive);
        assert_eq!(
            SweepTrigger::DoctorAfterChangesRequested.classification(),
            Some(ReworkClass::Substantive)
        );
        assert_eq!(
            SweepTrigger::RetryAfterSubstantiveFailure.classification(),
            Some(ReworkClass::Substantive)
        );
    }

    /// AC1 (classification half): every infrastructure-shaped rework —
    /// conflict, stale-base re-judge, CI rerun, rebase — is **environmental**.
    #[test]
    fn infrastructure_shaped_rework_is_environmental() {
        for kind in [
            ReworkKind::MergeConflict,
            ReworkKind::StaleBaseRejudge,
            ReworkKind::CiRerun,
            ReworkKind::Rebase,
        ] {
            assert_eq!(kind.classification(), ReworkClass::Environmental, "{kind:?}");
        }
        for trigger in [
            SweepTrigger::MergeConflict,
            SweepTrigger::StaleBaseRejudge,
            SweepTrigger::CiFailureFix,
            SweepTrigger::RebaseMainMoved,
            SweepTrigger::RetryAfterEnvFailure,
        ] {
            assert_eq!(trigger.classification(), Some(ReworkClass::Environmental), "{trigger:?}");
        }
    }

    /// The three unattributable triggers are counted as themselves and must
    /// never land in a rework bucket.
    #[test]
    fn unattributable_triggers_are_in_neither_bucket() {
        for trigger in [
            SweepTrigger::First,
            SweepTrigger::OperatorRedispatch,
            SweepTrigger::Unknown,
        ] {
            assert_eq!(trigger.classification(), None, "{trigger:?}");
        }
    }

    /// `ReworkEvent::new` takes its classification from the table, so a
    /// serialized event can never disagree with `kind.classification()`.
    #[test]
    fn event_classification_cannot_drift_from_the_table() {
        for kind in [
            ReworkKind::Rejudge,
            ReworkKind::StaleBaseRejudge,
            ReworkKind::MergeConflict,
            ReworkKind::CiRerun,
            ReworkKind::Rebase,
        ] {
            let event = ReworkEvent::new(kind, None, None);
            assert_eq!(event.classification, kind.classification(), "{kind:?}");
        }
    }

    // ------------------------------------------------------------------
    // Trigger derivation.
    // ------------------------------------------------------------------

    #[test]
    fn no_predecessor_is_the_first_attempt() {
        assert_eq!(derive_trigger(None, &[]), SweepTrigger::First);
        // …and in-sweep rework never turns a first attempt into a retry.
        assert_eq!(
            derive_trigger(None, &[ReworkEvent::new(ReworkKind::MergeConflict, None, None)]),
            SweepTrigger::First
        );
    }

    /// AC3: a retry after a spawn death (the #9441 `env_failure` disposition)
    /// reports `retry_after_env_failure`.
    #[test]
    fn a_retry_after_a_spawn_death_is_an_env_retry() {
        let previous = prior(SweepDisposition::EnvFailure, SweepResult::Failure);
        assert_eq!(derive_trigger(Some(&previous), &[]), SweepTrigger::RetryAfterEnvFailure);
    }

    #[test]
    fn a_cancel_is_an_environmental_predecessor_too() {
        let previous = prior(SweepDisposition::Cancelled, SweepResult::Cancelled);
        assert_eq!(derive_trigger(Some(&previous), &[]), SweepTrigger::RetryAfterEnvFailure);
    }

    #[test]
    fn a_substantive_predecessor_is_a_substantive_retry() {
        let previous = prior(SweepDisposition::SubstantiveFailure, SweepResult::Failure);
        assert_eq!(
            derive_trigger(Some(&previous), &[]),
            SweepTrigger::RetryAfterSubstantiveFailure
        );
    }

    /// AC1 (trigger half): the predecessor landed a PR, this attempt's PR
    /// conflicted with its moved base ⇒ `trigger = merge_conflict`, which
    /// classifies `environmental`.
    #[test]
    fn a_conflict_after_a_landing_names_the_conflict_as_the_trigger() {
        let previous = prior(SweepDisposition::Landed, SweepResult::Success);
        let rework = [ReworkEvent::new(
            ReworkKind::MergeConflict,
            Some("loom:merge-conflict".into()),
            Some(600),
        )];
        let trigger = derive_trigger(Some(&previous), &rework);
        assert_eq!(trigger, SweepTrigger::MergeConflict);
        assert_eq!(trigger.classification(), Some(ReworkClass::Environmental));
    }

    #[test]
    fn rework_names_the_stale_base_rejudge_and_ci_fix_triggers_too() {
        let previous = prior(SweepDisposition::Landed, SweepResult::Success);
        for (kind, expected) in [
            (ReworkKind::StaleBaseRejudge, SweepTrigger::StaleBaseRejudge),
            (ReworkKind::CiRerun, SweepTrigger::CiFailureFix),
            (ReworkKind::Rejudge, SweepTrigger::DoctorAfterChangesRequested),
            (ReworkKind::Rebase, SweepTrigger::RebaseMainMoved),
        ] {
            let rework = [ReworkEvent::new(kind, None, None)];
            assert_eq!(derive_trigger(Some(&previous), &rework), expected, "{kind:?}");
        }
    }

    /// A FAILED predecessor keeps the retry bucket even when this attempt then
    /// hits a conflict — the fault is why it was dispatched; the conflict is
    /// recorded on the separate `rework_events` axis.
    #[test]
    fn in_sweep_rework_never_overwrites_a_fault_dispatch_reason() {
        let previous = prior(SweepDisposition::EnvFailure, SweepResult::Failure);
        let rework = [ReworkEvent::new(ReworkKind::MergeConflict, None, None)];
        assert_eq!(derive_trigger(Some(&previous), &rework), SweepTrigger::RetryAfterEnvFailure);
    }

    #[test]
    fn a_non_failure_predecessor_with_no_rework_is_a_redispatch_decision() {
        for disposition in [
            SweepDisposition::Landed,
            SweepDisposition::NoopAlreadyDone,
            SweepDisposition::CuratorClosed,
            SweepDisposition::CuratorRescoped,
        ] {
            let previous = prior(disposition, SweepResult::Success);
            let trigger = derive_trigger(Some(&previous), &[]);
            assert_eq!(trigger, SweepTrigger::OperatorRedispatch, "{disposition:?}");
            assert_eq!(trigger.classification(), None, "{disposition:?}");
        }
    }

    // ------------------------------------------------------------------
    // The pre-#9441 fallback.
    // ------------------------------------------------------------------

    /// A predecessor line written before `disposition` existed decodes as
    /// `unknown`; its `result` + `failure_class` still split the same way.
    #[test]
    fn a_legacy_predecessor_falls_back_to_result_and_failure_class() {
        for (class, expected) in [
            ("preflight-no-cli-start", SweepTrigger::RetryAfterEnvFailure),
            ("account-exhausted:model-credits-exhausted", SweepTrigger::RetryAfterEnvFailure),
            ("no-usable-account", SweepTrigger::RetryAfterEnvFailure),
            ("execution-error", SweepTrigger::RetryAfterEnvFailure),
            // A bare exit code is deliberately NOT environmental (#9441), so
            // the work is the remaining explanation.
            ("exit-1", SweepTrigger::RetryAfterSubstantiveFailure),
        ] {
            let previous = PriorAttempt {
                disposition: SweepDisposition::Unknown,
                result: SweepResult::Failure,
                failure_class: Some(class),
            };
            assert_eq!(derive_trigger(Some(&previous), &[]), expected, "{class}");
        }
    }

    /// A legacy failure nothing classified at all stays `unknown` rather than
    /// being charged to either rework bucket.
    #[test]
    fn a_legacy_failure_with_no_class_is_unknown_not_a_guess() {
        let previous = prior(SweepDisposition::Unknown, SweepResult::Failure);
        let trigger = derive_trigger(Some(&previous), &[]);
        assert_eq!(trigger, SweepTrigger::Unknown);
        assert_eq!(trigger.classification(), None);
    }

    #[test]
    fn a_legacy_success_or_cancel_predecessor_maps_like_its_disposition_would() {
        let cancelled = prior(SweepDisposition::Unknown, SweepResult::Cancelled);
        assert_eq!(derive_trigger(Some(&cancelled), &[]), SweepTrigger::RetryAfterEnvFailure);
        let success = prior(SweepDisposition::Unknown, SweepResult::Success);
        assert_eq!(derive_trigger(Some(&success), &[]), SweepTrigger::OperatorRedispatch);
        let rework = [ReworkEvent::new(ReworkKind::CiRerun, None, None)];
        assert_eq!(derive_trigger(Some(&success), &rework), SweepTrigger::CiFailureFix);
    }

    // ------------------------------------------------------------------
    // Wire contract.
    // ------------------------------------------------------------------

    #[test]
    fn wire_strings_match_the_documented_vocabulary() {
        for (variant, wire) in [
            (SweepTrigger::First, "first"),
            (SweepTrigger::RetryAfterEnvFailure, "retry_after_env_failure"),
            (SweepTrigger::RetryAfterSubstantiveFailure, "retry_after_substantive_failure"),
            (SweepTrigger::DoctorAfterChangesRequested, "doctor_after_changes_requested"),
            (SweepTrigger::RebaseMainMoved, "rebase_main_moved"),
            (SweepTrigger::MergeConflict, "merge_conflict"),
            (SweepTrigger::StaleBaseRejudge, "stale_base_rejudge"),
            (SweepTrigger::CiFailureFix, "ci_failure_fix"),
            (SweepTrigger::OperatorRedispatch, "operator_redispatch"),
            (SweepTrigger::Unknown, "unknown"),
        ] {
            assert_eq!(variant.as_str(), wire);
            assert_eq!(
                serde_json::to_value(variant).unwrap(),
                serde_json::Value::String(wire.to_string()),
                "as_str() and the serde tag must never drift"
            );
        }
        for (kind, wire) in [
            (ReworkKind::Rejudge, "rejudge"),
            (ReworkKind::StaleBaseRejudge, "stale_base_rejudge"),
            (ReworkKind::MergeConflict, "merge_conflict"),
            (ReworkKind::CiRerun, "ci_rerun"),
            (ReworkKind::Rebase, "rebase"),
        ] {
            assert_eq!(kind.as_str(), wire);
            assert_eq!(
                serde_json::to_value(kind).unwrap(),
                serde_json::Value::String(wire.to_string())
            );
        }
        for (class, wire) in [
            (ReworkClass::Substantive, "substantive"),
            (ReworkClass::Environmental, "environmental"),
        ] {
            assert_eq!(class.as_str(), wire);
            assert_eq!(
                serde_json::to_value(class).unwrap(),
                serde_json::Value::String(wire.to_string())
            );
        }
    }

    /// A rework event serializes with its classification inline and omits the
    /// two optional fields when unobserved — "unknown != zero", as everywhere
    /// else in this schema.
    #[test]
    fn an_unmeasured_event_omits_reason_and_duration() {
        let json = serde_json::to_value(ReworkEvent::new(ReworkKind::Rejudge, None, None)).unwrap();
        assert_eq!(json, serde_json::json!({"kind": "rejudge", "classification": "substantive"}));
    }

    #[test]
    fn a_measured_event_carries_both() {
        let json = serde_json::to_value(ReworkEvent::new(
            ReworkKind::MergeConflict,
            Some("loom:merge-conflict".into()),
            Some(1_800),
        ))
        .unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "kind": "merge_conflict",
                "classification": "environmental",
                "reason": "loom:merge-conflict",
                "duration_sec": 1_800,
            })
        );
    }

    /// A journal line written before this field existed carries no `trigger`;
    /// it must still decode, as `unknown`, or every historical line would
    /// vanish from the drop-on-parse-failure readers.
    #[test]
    fn missing_trigger_decodes_as_unknown() {
        assert_eq!(SweepTrigger::default(), SweepTrigger::Unknown);
    }
}
