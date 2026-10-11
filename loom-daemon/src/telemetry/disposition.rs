//! `sweep.outcome`'s `disposition` — what a sweep actually DID (Issue #9441).
//!
//! # Why `result` was not enough
//!
//! [`SweepResult`] answers "did the process end well?", which is not the
//! question any throughput or effort metric actually asks. Measured on the
//! fleet telemetry store over 2026-08-15..09-29 (26,260 outcomes):
//!
//! - **`success` does not mean landed.** Only 337 of 8,808 successes carried a
//!   `pr_number`. 4,825 of them finished in under 300 s with no PR, no tokens
//!   and no phases — re-dispatch/no-op sweeps against an issue that was already
//!   done. One issue accumulated 970 such "successes" across 40 days.
//! - **`failure` is mostly unclassified.** Of 16,898 failures, 5,326 were
//!   sub-60 s deaths with no phase info (725 at exactly 0 s), 2,810 stopped
//!   after the Curator, 6,373 showed no phase info at all, and only 235 carried
//!   a substantive signal (a Judge `fail` or a Doctor phase).
//! - Nothing separated "the environment broke" (spawn death, account
//!   exhaustion, rate limit, cancel) from "the work was hard" (Judge rejected,
//!   Doctor loop exhausted).
//!
//! [`SweepDisposition`] is that missing axis. It is **additive**: `result` is
//! unchanged and still emitted, so every existing consumer keeps working.
//!
//! # A derivation, not new instrumentation
//!
//! [`classify_disposition`] is a pure function of signals the outcome path
//! already holds at emit time — the PR number sampled off the checkpoint, the
//! sampled phase history, the wall-clock duration, the `failure_class` the
//! sibling `sweep-outcomes.jsonl` record computes, the Judge/Doctor signals
//! read off the PR's label timeline, and (the one addition, folded into an
//! existing REST read rather than a new one — see
//! `sweep_registry::outcome_journal::complexity_signal`) the issue's own end
//! state. Being pure is what makes the two invariants below testable without a
//! registry, a forge, or a filesystem.
//!
//! # The two invariants
//!
//! 1. **`landed` ⇔ the sweep produced a PR.** Exactly one arm of
//!    [`disposition_of`] returns [`SweepDisposition::Landed`], and it is the
//!    first one, guarded on `pr_number.is_some()`; every other arm is
//!    unreachable while a PR is present. So the biconditional holds by
//!    construction, and `contract_landed_iff_pr_present` proves it over the
//!    whole signal space rather than on a handful of examples.
//!
//!    This deliberately outranks `result`: a sweep that was cancelled after
//!    opening a PR still *produced* that PR, and "did this sweep land work?"
//!    must have exactly one answer. Nothing is lost — `result` still reports
//!    `cancelled` on that record.
//!
//! 2. **`failure_class` is mandatory for `env_failure`, `substantive_failure`
//!    and `unknown`.** [`classify_disposition`] returns the disposition and the
//!    (possibly synthesized) class as ONE value, so it is structurally
//!    impossible for a caller to take one without the other. When no classifier
//!    labeled the transition, a bounded-cardinality `unclassified:*` label is
//!    synthesized from the strongest observed signal — never `""`, never a
//!    silent absence.
//!
//! # `unknown` is counted, never merged
//!
//! [`SweepDisposition::Unknown`] is reserved for a genuinely unobservable
//! outcome: a non-short success with no PR and no forge signal, a
//! human-decision block, or a failure with no classifier label, no phase
//! history, and a duration too long to call a spawn death. It is a real bucket
//! a fleet operator is expected to watch and drive down — folding it into a
//! neighbour would hide exactly the measurement gap this field exists to close.

use serde::{Deserialize, Serialize};

use super::{JudgeVerdict, PhaseDuration, SweepResult};

/// A clean, PR-less, phase-less sweep shorter than this is a no-op
/// re-dispatch, not a landing (Issue #9441). 300 s is the boundary the fleet
/// measurement drew: 4,825 of 8,808 "successes" finished under it with no PR,
/// no tokens and no phases.
pub const NOOP_MAX_DURATION_SEC: i64 = 300;

/// A phase-less death shorter than this never reached work — a spawn or
/// pre-flight death, i.e. the environment, not the task (Issue #9441). 60 s is
/// the boundary the fleet measurement drew: 5,326 failures fell under it with
/// no phase info at all, 725 of them at exactly 0 s.
pub const SPAWN_DEATH_MAX_DURATION_SEC: i64 = 60;

/// The lifecycle phases whose observation proves the sweep got past curation
/// and into real build work. Used both to recognize a substantive failure and
/// to refuse to call a closed issue "the Curator closed it" when the sweep had
/// demonstrably moved on.
const BUILD_WORK_PHASES: [&str; 4] = ["builder", "judge", "doctor", "merge"];

/// The closed phase vocabulary a synthesized `unclassified:*` label may name.
/// Anything else folds to `other`, so the synthesized-class cardinality stays
/// bounded no matter what a future checkpoint marker spells.
const KNOWN_PHASES: [&str; 5] = ["curator", "builder", "judge", "doctor", "merge"];

/// The synthesized class for a failure with no classifier label, no phase
/// history, and a duration too long to call a spawn death (Issue #10642 reads
/// it back to attach a cause and to count these deaths toward the PR-less
/// retry hold).
pub const NO_PHASE_SIGNAL_CLASS: &str = "unclassified:no-phase-signal";

/// What a sweep actually did, independent of how its process ended (Issue
/// #9441) — the axis [`SweepResult`] cannot express.
///
/// Low-cardinality and closed by design: a metric that groups on this must not
/// grow new buckets without a schema change. Serialized `snake_case`, matching
/// every other enum in this schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SweepDisposition {
    /// The sweep produced a PR for its issue. The one unambiguous "work
    /// landed" signal, and the only arm that may be returned when
    /// `pr_number` is present (see the module doc's invariant 1).
    Landed,
    /// Nothing to do: the issue was already closed/landed before this sweep
    /// started, or the sweep exited clean and short having produced no PR, no
    /// phase and no work. The re-dispatch shape that made `success` useless as
    /// a throughput signal.
    NoopAlreadyDone,
    /// The Curator closed the issue instead of building it (the
    /// "Issues Are Suggestions" path) — a correct outcome, not a failure.
    CuratorClosed,
    /// The Curator relabeled the issue back to `loom:triage`/`loom:curated`
    /// instead of building it — likewise a correct outcome.
    CuratorRescoped,
    /// The *environment* broke: a spawn/pre-flight death, account or credit
    /// exhaustion, a rate limit, a harness execution error. Carries a
    /// mandatory `failure_class`.
    EnvFailure,
    /// The *work* did not succeed: the Judge rejected it, the Doctor loop was
    /// exhausted, or the Builder could not finish. Carries a mandatory
    /// `failure_class`.
    SubstantiveFailure,
    /// An operator- or watchdog-initiated cancellation.
    Cancelled,
    /// Genuinely unobservable. Counted, never silently merged into a
    /// neighbouring bucket. Carries a mandatory `failure_class`.
    #[default]
    Unknown,
}

impl SweepDisposition {
    /// The exact wire string this variant serializes to — available without
    /// serializing, for log lines and span attributes.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Landed => "landed",
            Self::NoopAlreadyDone => "noop_already_done",
            Self::CuratorClosed => "curator_closed",
            Self::CuratorRescoped => "curator_rescoped",
            Self::EnvFailure => "env_failure",
            Self::SubstantiveFailure => "substantive_failure",
            Self::Cancelled => "cancelled",
            Self::Unknown => "unknown",
        }
    }

    /// Whether [`crate::telemetry::SweepOutcomeRecord::failure_class`] is
    /// **mandatory** for this disposition (Issue #9441, invariant 2): every
    /// disposition that asserts something went wrong — or that admits it
    /// cannot tell — must say what, or the field is no better than the
    /// unclassified `failure` it replaces.
    #[must_use]
    pub fn requires_failure_class(self) -> bool {
        matches!(self, Self::EnvFailure | Self::SubstantiveFailure | Self::Unknown)
    }
}

/// The issue's own end state relative to this sweep, when the outcome path's
/// forge read succeeded (Issue #9441).
///
/// This is what separates the three otherwise-identical "clean exit, no PR"
/// shapes — already done, Curator closed it, Curator rescoped it — that
/// `result: success` folds into one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssueEndState {
    /// The issue was already closed when this sweep was dispatched: there was
    /// never anything for it to do.
    ClosedBeforeDispatch,
    /// The issue was open at dispatch and is closed now.
    ClosedDuringSweep,
    /// The issue is open, carries a pre-build label (`loom:triage` /
    /// `loom:curated`) and carries **neither** `loom:issue` nor
    /// `loom:building` — it was handed back for re-scoping rather than built.
    ///
    /// Both halves are load-bearing, because both pre-build labels are
    /// *additive milestones* that outlive the state they described:
    ///
    /// - `loom:curated` is never stripped once applied, so promotion and the
    ///   Builder's claim both leave it in place — presence alone would mark
    ///   essentially every built issue as a hand-back (#9441, PR #9471).
    /// - `loom:issue` is restored by the reaper's own orphaned-claim recovery
    ///   (`loom:building` → `loom:issue`) on a crash or cancel *before* the
    ///   outcome record is written, so its presence means "back in the queue",
    ///   never "rescoped".
    Rescoped,
    /// The issue is open and nothing about its labels suggests a hand-back.
    StillOpen,
}

/// Every signal [`classify_disposition`] reads, borrowed from the outcome
/// record under construction. A struct rather than eight positional arguments
/// so an added signal cannot silently shift an existing one.
#[derive(Debug, Clone, Copy)]
pub struct DispositionSignals<'a> {
    /// The terminal [`SweepResult`] this record already carries.
    pub result: SweepResult,
    /// The PR this sweep produced, when its checkpoint named one.
    pub pr_number: Option<u32>,
    /// The classifier label for this terminal transition, when one exists.
    pub failure_class: Option<&'a str>,
    /// The sampled per-phase breakdown, in lifecycle order.
    pub phase_durations: &'a [PhaseDuration],
    /// Wall-clock seconds from dispatch to terminal transition.
    pub total_duration_sec: i64,
    /// Judge verdicts off the PR's label timeline; `None` when not read.
    pub judge_verdicts: Option<&'a [JudgeVerdict]>,
    /// Completed Doctor cycles off the same timeline; `None` when not read.
    pub doctor_cycles: Option<u32>,
    /// The issue's end state; `None` when the forge read was skipped or failed.
    pub issue_end_state: Option<IssueEndState>,
}

impl DispositionSignals<'_> {
    /// Whether any phase at or past `builder` was observed — the sweep
    /// demonstrably got out of curation and into build work.
    fn reached_build_work(&self) -> bool {
        self.phase_durations
            .iter()
            .any(|p| BUILD_WORK_PHASES.contains(&p.phase.as_str()))
    }

    /// Whether a Judge rejected this sweep's PR.
    fn judge_rejected(&self) -> bool {
        self.judge_verdicts
            .is_some_and(|v| v.iter().any(|verdict| verdict.verdict == "fail"))
    }

    /// Whether the Doctor loop ran at least once.
    fn doctor_looped(&self) -> bool {
        self.doctor_cycles.is_some_and(|cycles| cycles > 0)
    }

    /// Whether anything proves this sweep attempted — and failed at — the
    /// actual work, as opposed to dying before reaching it.
    fn has_substantive_signal(&self) -> bool {
        self.judge_rejected() || self.doctor_looped() || self.reached_build_work()
    }

    /// Whether this death never reached any observable phase and was too short
    /// to have done work — the spawn/pre-flight shape.
    fn looks_like_spawn_death(&self) -> bool {
        self.phase_durations.is_empty() && self.total_duration_sec < SPAWN_DEATH_MAX_DURATION_SEC
    }

    /// The last observed phase, folded to the closed [`KNOWN_PHASES`]
    /// vocabulary so a synthesized label's cardinality stays bounded.
    fn last_phase_label(&self) -> Option<&'static str> {
        let raw = self.phase_durations.last()?.phase.as_str();
        Some(
            KNOWN_PHASES
                .iter()
                .find(|known| **known == raw)
                .copied()
                .unwrap_or("other"),
        )
    }
}

/// Re-class an `unclassified:after-curator` stop that belongs to a no-op loop
/// (Issue #10156) as `loop_class` (`noop-loop:human-gate` /
/// `noop-loop:blocked`). Anything else — another disposition, another class,
/// or no loop — passes through, so a first-time Curator-only stop stays
/// `after-curator`.
#[must_use]
pub fn apply_noop_loop_class(
    disposition: SweepDisposition,
    class: Option<String>,
    loop_class: Option<&str>,
) -> Option<String> {
    match (disposition, class.as_deref(), loop_class) {
        (SweepDisposition::Unknown, Some("unclassified:after-curator"), Some(lc)) => {
            Some(lc.to_string())
        }
        _ => class,
    }
}

/// Whether `class` names an *environmental* fault — the run broke, rather than
/// the work being hard (Issue #9441).
///
/// The vocabulary is `sweep_registry::crash_signals`' own output plus the
/// pre-flight classifier's: `preflight-*` (nothing ever started),
/// `account-exhausted:*` and `no-usable-account` (the credential pool, not the
/// issue), `execution-error` (the harness died mid-run), `self-kill:*` (a
/// background-wait self-termination). Deliberately **not** `exit-<code>`: a
/// bare non-zero exit says nothing about whether the work or the environment
/// was at fault, so it falls through to the phase-based arms instead of being
/// guessed at.
#[must_use]
pub fn is_environmental_failure_class(class: &str) -> bool {
    class.starts_with("preflight-")
        || class.starts_with("account-exhausted:")
        || class.starts_with("self-kill:")
        || class == "no-usable-account"
        || class == "execution-error"
}

/// Classify a terminal sweep transition into its [`SweepDisposition`] and the
/// `failure_class` that must accompany it (Issue #9441).
///
/// Returns BOTH as one value on purpose: invariant 2 (`failure_class` is
/// mandatory for `env_failure` / `substantive_failure` / `unknown`) is
/// unenforceable if a caller can take the disposition and leave the class
/// behind. When the transition carried a real classifier label it is passed
/// through verbatim — synthesis only ever fills a hole, it never overwrites.
#[must_use]
pub fn classify_disposition(
    signals: &DispositionSignals<'_>,
) -> (SweepDisposition, Option<String>) {
    let disposition = disposition_of(signals);
    let failure_class = signals.failure_class.map(str::to_owned).or_else(|| {
        disposition
            .requires_failure_class()
            .then(|| synthesized_failure_class(disposition, signals))
    });
    debug_assert!(
        !disposition.requires_failure_class() || failure_class.is_some(),
        "#9441 invariant 2: {} must carry a failure_class",
        disposition.as_str()
    );
    (disposition, failure_class)
}

/// The disposition alone, strongest signal first. Exactly one arm returns
/// [`SweepDisposition::Landed`] and it is the first, which is what makes
/// invariant 1 hold by construction.
fn disposition_of(signals: &DispositionSignals<'_>) -> SweepDisposition {
    // 1. A PR is the one unambiguous "this sweep produced work" signal, and it
    //    outranks `result` — see the module doc.
    if signals.pr_number.is_some() {
        return SweepDisposition::Landed;
    }
    // 2. An operator/watchdog cancel is a deliberate, known ending. Nothing
    //    below could describe it more precisely.
    if signals.result == SweepResult::Cancelled {
        return SweepDisposition::Cancelled;
    }
    // 3. The one forge answer that is derived from a TIMESTAMP rather than
    //    from labels: the issue was already closed when this sweep was
    //    dispatched, so there was never anything to do — whatever went wrong
    //    afterwards. Being timestamp-derived, it cannot be poisoned by a stale
    //    label the way the two arms in step 5 can, so it stays on top.
    if signals.issue_end_state == Some(IssueEndState::ClosedBeforeDispatch) {
        return SweepDisposition::NoopAlreadyDone;
    }
    // 4. A failure whose classifier label names an environmental fault is
    //    settled outright — an exhausted account is not a hard task, and it is
    //    not a Curator decision either, no matter which phase it struck in or
    //    what the issue's labels happen to say.
    //
    //    This deliberately outranks the label-derived forge arms below
    //    (#9441, Judge finding on PR #9471): those labels are additive
    //    milestones that outlive the state they described, so letting one
    //    shadow an explicit environmental verdict would report a dead token
    //    pool as `curator_rescoped` — a bucket documented as a *correct*
    //    outcome and exempt from the mandatory-`failure_class` invariant.
    //    Restricted to `Failure` because that is the only result that reached
    //    this check before the reorder (`Cancelled` returns at step 2,
    //    `Success`/`Blocked` at steps 6/7), which keeps the change a pure
    //    reordering rather than a new behaviour for those results.
    if signals.result == SweepResult::Failure
        && signals
            .failure_class
            .is_some_and(is_environmental_failure_class)
    {
        return SweepDisposition::EnvFailure;
    }
    // 5. The forge's remaining, label-derived answers. `ClosedDuringSweep` and
    //    `Rescoped` are attributed to the Curator only while the sweep
    //    never reached build work: past that point a closed issue is far more
    //    likely a landed PR this record failed to sample than a Curator
    //    decision, and guessing "curator_closed" there would overstate the
    //    one bucket an operator reads as "working as intended".
    match signals.issue_end_state {
        Some(IssueEndState::ClosedDuringSweep) if !signals.reached_build_work() => {
            return SweepDisposition::CuratorClosed;
        }
        Some(IssueEndState::Rescoped) if !signals.reached_build_work() => {
            return SweepDisposition::CuratorRescoped;
        }
        _ => {}
    }
    // 6. The local no-op shape, for every host whose forge read was skipped or
    //    failed: a clean, short run that produced no PR and no phase at all.
    if signals.result == SweepResult::Success
        && signals.phase_durations.is_empty()
        && signals.total_duration_sec < NOOP_MAX_DURATION_SEC
    {
        return SweepDisposition::NoopAlreadyDone;
    }
    // 7. A success with no PR that is NOT the short no-op shape, and a
    //    human-decision block, are both genuinely unobservable from here.
    //    Counted as `unknown` rather than folded into a failure bucket they
    //    did not earn.
    if matches!(signals.result, SweepResult::Success | SweepResult::Blocked) {
        return SweepDisposition::Unknown;
    }
    // 8. Evidence the work itself was attempted and did not succeed.
    if signals.has_substantive_signal() {
        return SweepDisposition::SubstantiveFailure;
    }
    // 9. No phase, no classifier label, gone in under a minute: a spawn or
    //    pre-flight death the classifier simply did not have a signature for.
    if signals.looks_like_spawn_death() {
        return SweepDisposition::EnvFailure;
    }
    SweepDisposition::Unknown
}

/// The `unclassified:*` label to stamp when a disposition demands a
/// `failure_class` and no classifier produced one.
///
/// Prefixed so no consumer can mistake a synthesized label for a real
/// classifier verdict, and drawn from the strongest observed signal so it
/// still says something. Cardinality is bounded by [`KNOWN_PHASES`].
fn synthesized_failure_class(
    disposition: SweepDisposition,
    signals: &DispositionSignals<'_>,
) -> String {
    match disposition {
        SweepDisposition::EnvFailure => {
            if signals.looks_like_spawn_death() {
                "unclassified:spawn-death".to_string()
            } else {
                "unclassified:env".to_string()
            }
        }
        SweepDisposition::SubstantiveFailure => {
            if signals.judge_rejected() {
                "unclassified:judge-rejected".to_string()
            } else if signals.doctor_looped() {
                "unclassified:doctor-loop".to_string()
            } else {
                signals.last_phase_label().map_or_else(
                    || "unclassified:substantive".to_string(),
                    |phase| format!("unclassified:stopped-after-{phase}"),
                )
            }
        }
        // `Unknown` is the only remaining arm that requires a class; the
        // others never reach here (see `requires_failure_class`).
        _ => match signals.result {
            SweepResult::Success => "unclassified:success-without-pr".to_string(),
            SweepResult::Blocked => "unclassified:blocked-on-human-decision".to_string(),
            _ => signals.last_phase_label().map_or_else(
                || NO_PHASE_SIGNAL_CLASS.to_string(),
                |phase| format!("unclassified:after-{phase}"),
            ),
        },
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn phases(names: &[&str]) -> Vec<PhaseDuration> {
        names
            .iter()
            .map(|p| PhaseDuration::new((*p).to_string(), 10))
            .collect()
    }

    fn verdicts(kinds: &[&str]) -> Vec<JudgeVerdict> {
        kinds
            .iter()
            .enumerate()
            .map(|(i, v)| JudgeVerdict {
                attempt: u32::try_from(i + 1).unwrap(),
                verdict: (*v).to_string(),
            })
            .collect()
    }

    /// A baseline "nothing observed" signal set, mutated per test.
    fn base(result: SweepResult) -> DispositionSignals<'static> {
        DispositionSignals {
            result,
            pr_number: None,
            failure_class: None,
            phase_durations: &[],
            total_duration_sec: 0,
            judge_verdicts: None,
            doctor_cycles: None,
            issue_end_state: None,
        }
    }

    // ------------------------------------------------------------------
    // Invariant 1 (the #9441 acceptance criterion): landed ⇔ a PR exists.
    // ------------------------------------------------------------------

    /// Exhaustive contract test over the whole signal space, not a handful of
    /// examples: for EVERY combination of result, classifier label, phase
    /// history, duration, timeline signal and issue end state, the
    /// disposition is `landed` exactly when a PR is present.
    #[test]
    fn contract_landed_iff_pr_present() {
        let phase_sets = [
            phases(&[]),
            phases(&["curator"]),
            phases(&["curator", "builder"]),
            phases(&["curator", "builder", "judge", "doctor"]),
            phases(&["curator", "builder", "judge", "merge"]),
        ];
        let verdict_sets = [verdicts(&[]), verdicts(&["pass"]), verdicts(&["fail"])];
        let mut landed = 0_usize;
        let mut not_landed = 0_usize;
        for result in [
            SweepResult::Success,
            SweepResult::Failure,
            SweepResult::Cancelled,
            SweepResult::Blocked,
        ] {
            for class in [
                None,
                Some("preflight-token-selection-failed"),
                Some("account-exhausted:model-credits-exhausted"),
                Some("exit-1"),
                Some("execution-error"),
            ] {
                for phase_set in &phase_sets {
                    for duration in [0_i64, 30, 120, 2_730, 7_200] {
                        for verdict_set in &verdict_sets {
                            for cycles in [None, Some(0_u32), Some(2)] {
                                for end_state in [
                                    None,
                                    Some(IssueEndState::ClosedBeforeDispatch),
                                    Some(IssueEndState::ClosedDuringSweep),
                                    Some(IssueEndState::Rescoped),
                                    Some(IssueEndState::StillOpen),
                                ] {
                                    for pr_number in [None, Some(4710_u32)] {
                                        let signals = DispositionSignals {
                                            result,
                                            pr_number,
                                            failure_class: class,
                                            phase_durations: phase_set,
                                            total_duration_sec: duration,
                                            judge_verdicts: Some(verdict_set),
                                            doctor_cycles: cycles,
                                            issue_end_state: end_state,
                                        };
                                        let (disposition, _) = classify_disposition(&signals);
                                        assert_eq!(
                                            disposition == SweepDisposition::Landed,
                                            pr_number.is_some(),
                                            "landed must hold exactly when a PR exists: \
                                             {signals:?} -> {disposition:?}"
                                        );
                                        if pr_number.is_some() {
                                            landed += 1;
                                        } else {
                                            not_landed += 1;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        // Guard against the combinatorial loop silently collapsing to nothing.
        assert!(landed > 1_000 && not_landed > 1_000, "{landed}/{not_landed}");
    }

    // ------------------------------------------------------------------
    // Invariant 2: failure_class is mandatory for env/substantive/unknown.
    // ------------------------------------------------------------------

    /// Over the same signal space, every disposition that asserts a fault (or
    /// admits it cannot tell) carries a non-empty `failure_class`, and a real
    /// classifier label is never overwritten by a synthesized one.
    #[test]
    fn contract_failure_class_is_mandatory_where_required() {
        let phase_sets = [phases(&[]), phases(&["curator"]), phases(&["builder"])];
        let mut required_seen = 0_usize;
        for result in [
            SweepResult::Success,
            SweepResult::Failure,
            SweepResult::Cancelled,
            SweepResult::Blocked,
        ] {
            for class in [None, Some("exit-1"), Some("preflight-no-cli-start")] {
                for phase_set in &phase_sets {
                    for duration in [0_i64, 45, 600] {
                        for end_state in [
                            None,
                            Some(IssueEndState::ClosedBeforeDispatch),
                            Some(IssueEndState::ClosedDuringSweep),
                            Some(IssueEndState::Rescoped),
                            Some(IssueEndState::StillOpen),
                        ] {
                            let signals = DispositionSignals {
                                result,
                                pr_number: None,
                                failure_class: class,
                                phase_durations: phase_set,
                                total_duration_sec: duration,
                                judge_verdicts: None,
                                doctor_cycles: None,
                                issue_end_state: end_state,
                            };
                            let (disposition, resolved) = classify_disposition(&signals);
                            if disposition.requires_failure_class() {
                                required_seen += 1;
                                let resolved = resolved
                                    .as_deref()
                                    .unwrap_or_else(|| panic!("missing class for {signals:?}"));
                                assert!(!resolved.is_empty());
                            }
                            if let Some(original) = class {
                                assert_eq!(
                                    resolved.as_deref(),
                                    Some(original),
                                    "a real classifier label is never overwritten"
                                );
                            }
                        }
                    }
                }
            }
        }
        assert!(required_seen > 0);
    }

    // ------------------------------------------------------------------
    // Per-arm behaviour.
    // ------------------------------------------------------------------

    /// AC3: the 4,825-record shape — a clean, short, PR-less, phase-less
    /// re-dispatch — reports `noop_already_done`, not a landing-shaped
    /// `success`.
    #[test]
    fn short_clean_redispatch_is_a_noop_not_a_landing() {
        let signals = DispositionSignals {
            total_duration_sec: 41,
            ..base(SweepResult::Success)
        };
        let (disposition, class) = classify_disposition(&signals);
        assert_eq!(disposition, SweepDisposition::NoopAlreadyDone);
        assert_eq!(class, None, "a no-op is not a fault and needs no class");
    }

    #[test]
    fn success_past_the_noop_window_with_no_pr_is_unknown_with_a_class() {
        let signals = DispositionSignals {
            total_duration_sec: NOOP_MAX_DURATION_SEC + 1,
            ..base(SweepResult::Success)
        };
        let (disposition, class) = classify_disposition(&signals);
        assert_eq!(disposition, SweepDisposition::Unknown);
        assert_eq!(class.as_deref(), Some("unclassified:success-without-pr"));
    }

    #[test]
    fn an_issue_closed_before_dispatch_is_always_a_noop() {
        // Even a long, failing run against an already-closed issue had
        // nothing to do.
        let signals = DispositionSignals {
            total_duration_sec: 3_600,
            issue_end_state: Some(IssueEndState::ClosedBeforeDispatch),
            ..base(SweepResult::Failure)
        };
        assert_eq!(classify_disposition(&signals).0, SweepDisposition::NoopAlreadyDone);
    }

    #[test]
    fn curator_close_and_rescope_are_read_off_the_issue_end_state() {
        let closed = DispositionSignals {
            issue_end_state: Some(IssueEndState::ClosedDuringSweep),
            total_duration_sec: 900,
            ..base(SweepResult::Success)
        };
        assert_eq!(classify_disposition(&closed).0, SweepDisposition::CuratorClosed);

        let rescoped = DispositionSignals {
            issue_end_state: Some(IssueEndState::Rescoped),
            total_duration_sec: 900,
            ..base(SweepResult::Failure)
        };
        assert_eq!(classify_disposition(&rescoped).0, SweepDisposition::CuratorRescoped);
    }

    /// A sweep that reached build work and whose issue is now closed is NOT
    /// attributed to the Curator — the far likelier story is a landed PR this
    /// record never sampled, and `curator_closed` is the bucket an operator
    /// reads as "working as intended".
    #[test]
    fn a_closed_issue_after_build_work_is_not_charged_to_the_curator() {
        let phase_set = phases(&["curator", "builder", "judge"]);
        let signals = DispositionSignals {
            issue_end_state: Some(IssueEndState::ClosedDuringSweep),
            phase_durations: &phase_set,
            total_duration_sec: 1_800,
            ..base(SweepResult::Failure)
        };
        assert_ne!(classify_disposition(&signals).0, SweepDisposition::CuratorClosed);
    }

    #[test]
    fn environmental_classes_outrank_every_phase_signal() {
        let phase_set = phases(&["curator", "builder", "judge"]);
        for class in [
            "preflight-token-selection-failed",
            "preflight-no-cli-start",
            "account-exhausted:rate-limited",
            "account-exhausted:model-credits-exhausted",
            "no-usable-account",
            "execution-error",
            "self-kill:background-wait",
        ] {
            let signals = DispositionSignals {
                failure_class: Some(class),
                phase_durations: &phase_set,
                total_duration_sec: 1_200,
                ..base(SweepResult::Failure)
            };
            let (disposition, resolved) = classify_disposition(&signals);
            assert_eq!(disposition, SweepDisposition::EnvFailure, "{class}");
            assert_eq!(resolved.as_deref(), Some(class));
        }
    }

    /// #9441 regression (Judge finding on PR #9471): an environmental
    /// classifier verdict outranks the **label-derived** forge end states too,
    /// not just the phase signals. `loom:curated` is a persistent milestone
    /// this repo never strips, so before the fix a `preflight-*` or
    /// `account-exhausted:*` death on a normally-curated issue reported
    /// `curator_rescoped` — "working as intended" — and, because
    /// `curator_rescoped` is not in `requires_failure_class()`, silently
    /// dropped invariant 2's protection for that entire population.
    #[test]
    fn environmental_classes_outrank_the_label_derived_issue_end_state() {
        for end_state in [IssueEndState::Rescoped, IssueEndState::ClosedDuringSweep] {
            for (class, duration) in [
                ("preflight-token-selection-failed", 0_i64),
                ("preflight-no-cli-start", 2),
                ("account-exhausted:model-credits-exhausted", 3),
                ("no-usable-account", 5),
                ("execution-error", 900),
                ("self-kill:background-wait", 1_800),
            ] {
                let signals = DispositionSignals {
                    failure_class: Some(class),
                    issue_end_state: Some(end_state),
                    total_duration_sec: duration,
                    ..base(SweepResult::Failure)
                };
                let (disposition, resolved) = classify_disposition(&signals);
                assert_eq!(disposition, SweepDisposition::EnvFailure, "{class} + {end_state:?}");
                assert_eq!(
                    resolved.as_deref(),
                    Some(class),
                    "the classifier's own label survives: {class} + {end_state:?}"
                );
                assert!(
                    disposition.requires_failure_class(),
                    "and invariant 2 still applies to it: {class} + {end_state:?}"
                );
            }
        }
    }

    /// The timestamp-derived end state stays on top of the environmental arm:
    /// a sweep dispatched against an already-closed issue had nothing to do,
    /// and `ClosedBeforeDispatch` is a `closed_at` comparison, not a label —
    /// it cannot go stale the way the step-5 arms can.
    #[test]
    fn closed_before_dispatch_still_outranks_an_environmental_class() {
        let signals = DispositionSignals {
            failure_class: Some("preflight-token-selection-failed"),
            issue_end_state: Some(IssueEndState::ClosedBeforeDispatch),
            total_duration_sec: 0,
            ..base(SweepResult::Failure)
        };
        assert_eq!(classify_disposition(&signals).0, SweepDisposition::NoopAlreadyDone);
    }

    /// A non-environmental class leaves the forge end state in charge, so the
    /// reorder above narrowed nothing else: a genuine Curator hand-back is
    /// still `curator_rescoped`.
    #[test]
    fn a_non_environmental_class_leaves_the_rescope_arm_in_charge() {
        let signals = DispositionSignals {
            failure_class: Some("exit-1"),
            issue_end_state: Some(IssueEndState::Rescoped),
            total_duration_sec: 900,
            ..base(SweepResult::Failure)
        };
        assert_eq!(classify_disposition(&signals).0, SweepDisposition::CuratorRescoped);
    }

    /// A bare `exit-<code>` is deliberately NOT environmental — it says
    /// nothing about fault, so the phase signals decide.
    #[test]
    fn a_bare_exit_code_is_not_environmental() {
        assert!(!is_environmental_failure_class("exit-1"));
        let phase_set = phases(&["curator", "builder"]);
        let signals = DispositionSignals {
            failure_class: Some("exit-1"),
            phase_durations: &phase_set,
            total_duration_sec: 900,
            ..base(SweepResult::Failure)
        };
        assert_eq!(classify_disposition(&signals).0, SweepDisposition::SubstantiveFailure);
    }

    #[test]
    fn judge_rejection_and_doctor_loops_are_substantive() {
        let rejected = verdicts(&["fail"]);
        let signals = DispositionSignals {
            judge_verdicts: Some(&rejected),
            total_duration_sec: 2_400,
            ..base(SweepResult::Failure)
        };
        let (disposition, class) = classify_disposition(&signals);
        assert_eq!(disposition, SweepDisposition::SubstantiveFailure);
        assert_eq!(class.as_deref(), Some("unclassified:judge-rejected"));

        let looped = DispositionSignals {
            doctor_cycles: Some(3),
            total_duration_sec: 2_400,
            ..base(SweepResult::Failure)
        };
        let (disposition, class) = classify_disposition(&looped);
        assert_eq!(disposition, SweepDisposition::SubstantiveFailure);
        assert_eq!(class.as_deref(), Some("unclassified:doctor-loop"));
    }

    /// The 5,326-record shape: a sub-60 s death with no phase info and no
    /// classifier signature is still charged to the environment.
    #[test]
    fn unlabeled_sub_minute_phaseless_death_is_an_env_failure() {
        let signals = DispositionSignals {
            total_duration_sec: 0,
            ..base(SweepResult::Failure)
        };
        let (disposition, class) = classify_disposition(&signals);
        assert_eq!(disposition, SweepDisposition::EnvFailure);
        assert_eq!(class.as_deref(), Some("unclassified:spawn-death"));
    }

    /// The 6,373-record shape: past the spawn window with no phase info and
    /// nothing else to go on, `unknown` is the honest answer — and it still
    /// carries a class naming what was missing.
    #[test]
    fn long_phaseless_unlabeled_death_is_unknown_and_says_so() {
        let signals = DispositionSignals {
            total_duration_sec: 1_800,
            ..base(SweepResult::Failure)
        };
        let (disposition, class) = classify_disposition(&signals);
        assert_eq!(disposition, SweepDisposition::Unknown);
        assert_eq!(class.as_deref(), Some("unclassified:no-phase-signal"));
    }

    #[test]
    fn noop_loop_reclasses_only_the_after_curator_stop() {
        let signals = DispositionSignals {
            phase_durations: &phases(&["curator"]),
            total_duration_sec: 400,
            ..base(SweepResult::Failure)
        };
        let (disposition, class) = classify_disposition(&signals);
        assert_eq!(class.as_deref(), Some("unclassified:after-curator"));
        let none = apply_noop_loop_class(disposition, class.clone(), None);
        assert_eq!(none.as_deref(), Some("unclassified:after-curator"));
        let looped = apply_noop_loop_class(disposition, class, Some("noop-loop:human-gate"));
        assert_eq!(looped.as_deref(), Some("noop-loop:human-gate"));
        // A different class is never overwritten.
        let other = apply_noop_loop_class(
            SweepDisposition::Unknown,
            Some("unclassified:after-builder".into()),
            Some("noop-loop:blocked"),
        );
        assert_eq!(other.as_deref(), Some("unclassified:after-builder"));
    }

    /// The 2,810-record shape: stopped after the Curator, with no forge
    /// signal to say it was a deliberate close.
    #[test]
    fn stopped_after_curator_only_is_unknown_naming_the_phase() {
        let phase_set = phases(&["curator"]);
        let signals = DispositionSignals {
            phase_durations: &phase_set,
            total_duration_sec: 400,
            ..base(SweepResult::Failure)
        };
        let (disposition, class) = classify_disposition(&signals);
        assert_eq!(disposition, SweepDisposition::Unknown);
        assert_eq!(class.as_deref(), Some("unclassified:after-curator"));
    }

    #[test]
    fn a_cancel_is_cancelled_even_with_phases_observed() {
        let phase_set = phases(&["curator", "builder"]);
        let signals = DispositionSignals {
            phase_durations: &phase_set,
            total_duration_sec: 2_730,
            ..base(SweepResult::Cancelled)
        };
        let (disposition, class) = classify_disposition(&signals);
        assert_eq!(disposition, SweepDisposition::Cancelled);
        assert_eq!(class, None);
    }

    /// A cancel that had already opened a PR is `landed` — invariant 1 is
    /// absolute, and `result` still reports the cancellation.
    #[test]
    fn a_cancel_that_opened_a_pr_still_reads_as_landed() {
        let signals = DispositionSignals {
            pr_number: Some(9_441),
            ..base(SweepResult::Cancelled)
        };
        assert_eq!(classify_disposition(&signals).0, SweepDisposition::Landed);
    }

    #[test]
    fn synthesized_labels_fold_unknown_phases_to_a_bounded_vocabulary() {
        let phase_set = phases(&["some-future-marker"]);
        let signals = DispositionSignals {
            phase_durations: &phase_set,
            total_duration_sec: 900,
            ..base(SweepResult::Failure)
        };
        let (_, class) = classify_disposition(&signals);
        assert_eq!(class.as_deref(), Some("unclassified:after-other"));
    }

    #[test]
    fn wire_strings_match_the_documented_vocabulary() {
        for (variant, wire) in [
            (SweepDisposition::Landed, "landed"),
            (SweepDisposition::NoopAlreadyDone, "noop_already_done"),
            (SweepDisposition::CuratorClosed, "curator_closed"),
            (SweepDisposition::CuratorRescoped, "curator_rescoped"),
            (SweepDisposition::EnvFailure, "env_failure"),
            (SweepDisposition::SubstantiveFailure, "substantive_failure"),
            (SweepDisposition::Cancelled, "cancelled"),
            (SweepDisposition::Unknown, "unknown"),
        ] {
            assert_eq!(variant.as_str(), wire);
            assert_eq!(
                serde_json::to_value(variant).unwrap(),
                serde_json::Value::String(wire.to_string()),
                "as_str() and the serde tag must never drift"
            );
        }
    }

    #[test]
    fn only_fault_dispositions_require_a_class() {
        for variant in [
            SweepDisposition::EnvFailure,
            SweepDisposition::SubstantiveFailure,
            SweepDisposition::Unknown,
        ] {
            assert!(variant.requires_failure_class(), "{variant:?}");
        }
        for variant in [
            SweepDisposition::Landed,
            SweepDisposition::NoopAlreadyDone,
            SweepDisposition::CuratorClosed,
            SweepDisposition::CuratorRescoped,
            SweepDisposition::Cancelled,
        ] {
            assert!(!variant.requires_failure_class(), "{variant:?}");
        }
    }

    /// A pre-#9441 journal line carries no `disposition` key at all; it must
    /// still decode, as `unknown`, or every historical line would vanish from
    /// the (drop-on-parse-failure) readers.
    #[test]
    fn missing_disposition_decodes_as_unknown() {
        assert_eq!(SweepDisposition::default(), SweepDisposition::Unknown);
    }
}
