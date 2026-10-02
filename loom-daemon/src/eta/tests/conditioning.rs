//! Worked-only conditioning of the stage distributions (#9420).
//!
//! The defect this guards: the `loom.role_attempt` dwell population is
//! dominated by attempts that did no work (80.4% of this host's 130,657 role
//! ticks over 2026-09-18…10-02 never launched a session), so its
//! unconditioned median is milliseconds — builder literally 0 ms. Any reader
//! of `stages[].distribution` that admits such a sample publishes a fabricated
//! value.
//!
//! What is asserted here, in order: the refusal itself; that the refusal
//! degrades into the **existing** `MIN_SAMPLES` floor rather than a second
//! "unmeasured" mechanism; that the censored side conditions identically; that
//! a long attempt with nothing to show for it is NOT refused; and that a
//! worked-only local feed is left exactly as it was (every v1 estimate
//! byte-identical).

use super::{as_of, history_a, input_at};
use crate::eta::heuristics::LandV1;
use crate::eta::history::{SampleSource, StageSample, StageSamples};
use crate::eta::{Heuristic, Stage, MIN_SAMPLES};
use crate::telemetry::SweepOutcomeRecord;
use chrono::Duration;

const SOURCES: [SampleSource; 2] = [SampleSource::SweepOutcome, SampleSource::StageJournal];

fn sample(stage: Stage, duration_sec: i64, offset_hours: i64, worked: Option<bool>) -> StageSample {
    StageSample {
        repo: "rjwalters/loom".to_string(),
        stage,
        duration_sec,
        observed_at: as_of() - Duration::hours(offset_hours),
        source: SampleSource::SweepOutcome,
        host: "host-fixture-a".to_string(),
        worked,
    }
}

/// `n` samples of `stage`, all marked `worked`.
fn samples(stage: Stage, duration_sec: i64, n: usize, worked: Option<bool>) -> Vec<StageSample> {
    (0..n)
        .map(|i| sample(stage, duration_sec, i as i64 + 1, worked))
        .collect()
}

// ------------------------------------------------------------ the refusal

/// The millisecond-median shape, reproduced in miniature: 24 attempts that did
/// nothing (1 s each) beside 8 that did the work (1450 s — this host's measured
/// builder median). Unconditioned the p50 would be 1 s; conditioned it is the
/// real one, and only the worked samples are counted.
#[test]
fn a_distribution_reads_only_the_attempts_that_did_the_work() {
    let mut history = StageSamples::default();
    history
        .stages
        .extend(samples(Stage::SweepBuilder, 1, 24, Some(false)));
    history
        .stages
        .extend(samples(Stage::SweepBuilder, 1450, MIN_SAMPLES, Some(true)));

    let selection = history
        .select("rjwalters/loom", Stage::SweepBuilder, as_of(), &SOURCES)
        .expect("8 worked samples clear the floor");
    assert_eq!(selection.sorted.len(), MIN_SAMPLES, "only worked samples are summarised");
    assert!(selection.sorted.iter().all(|&d| d == 1450));
    assert_eq!(selection.excluded_unworked, 24, "and the refusal is counted, not silent");
    // Guard against the bug being "re-fixed" by a later reader: the median of
    // the full population is the value #9420 is about.
    let mut unconditioned: Vec<i64> = history.stages.iter().map(|s| s.duration_sec).collect();
    unconditioned.sort_unstable();
    assert_eq!(unconditioned[unconditioned.len() / 2], 1, "unconditioned p50 is the artefact");
}

/// Too few conditioned samples is **unmeasured**, not a value from a tiny or
/// no-op-dominated sample — and it arrives through the floor the estimators
/// already report as `insufficient_samples`, not a parallel mechanism.
#[test]
fn too_few_worked_samples_falls_through_the_existing_floor_to_unmeasured() {
    let mut history = StageSamples::default();
    // Plenty of evidence in total, almost none of it a measurement.
    history
        .stages
        .extend(samples(Stage::SweepBuilder, 1, 40, Some(false)));
    history
        .stages
        .extend(samples(Stage::SweepBuilder, 1450, MIN_SAMPLES - 1, Some(true)));
    assert_eq!(history.stages.len(), 40 + MIN_SAMPLES - 1);
    assert!(
        history
            .select("rjwalters/loom", Stage::SweepBuilder, as_of(), &SOURCES)
            .is_none(),
        "{} worked samples is below the floor at both levels",
        MIN_SAMPLES - 1
    );
    // One more worked sample, and the same call answers.
    history
        .stages
        .push(sample(Stage::SweepBuilder, 1450, 99, Some(true)));
    let selection = history
        .select("rjwalters/loom", Stage::SweepBuilder, as_of(), &SOURCES)
        .expect("the floor is met");
    assert_eq!(selection.sorted.len(), MIN_SAMPLES);
    assert_eq!(selection.excluded_unworked, 40);
}

/// The censored lower bounds condition on the same flag, so a `land-v2` grid
/// can never pair a worked-only observed set with an unconditioned censored
/// one — that would re-import the bias through the other door.
#[test]
fn the_censored_side_conditions_on_the_same_flag() {
    use crate::eta::history::Level;
    let mut history = StageSamples::default();
    history
        .censored
        .extend(samples(Stage::Doctor, 2, 5, Some(false)));
    history
        .censored
        .extend(samples(Stage::Doctor, 900, 3, Some(true)));
    let bounds =
        history.select_censored("rjwalters/loom", Stage::Doctor, as_of(), &SOURCES, Level::Repo);
    assert_eq!(bounds, vec![900, 900, 900]);
}

// ------------------------------------------------- what is NOT conditioned

/// The edge case the issue names: an attempt that produced **no forge action**
/// but legitimately ran long (a Judge that read a PR and posted nothing, a
/// Curator that researched without relabelling). It is worked — the span's
/// interval does measure the stage's work — so the `worked` signal is keyed on
/// "did a session run / is the start observed", never on an action tally.
///
/// `RoleTickActions` is documented as a **lower bound**; using an all-zero
/// tally as "no work" would throw away exactly these samples, which are the
/// slow tail an ETA most needs.
#[test]
fn a_long_attempt_with_no_forge_actions_is_still_a_worked_sample() {
    let mut history = StageSamples::default();
    history
        .stages
        .extend(samples(Stage::SweepCurator, 3600, MIN_SAMPLES, Some(true)));
    // Same durations, no explicit flag — the local-journal default. Also
    // admitted: `None` is "the producer did not say", not "did nothing".
    let mut unflagged = StageSamples::default();
    unflagged
        .stages
        .extend(samples(Stage::SweepCurator, 3600, MIN_SAMPLES, None));
    for (name, history) in [("worked", &history), ("unflagged", &unflagged)] {
        let selection = history
            .select("rjwalters/loom", Stage::SweepCurator, as_of(), &SOURCES)
            .unwrap_or_else(|| panic!("{name} history must summarise"));
        assert_eq!(selection.sorted, vec![3600; MIN_SAMPLES], "{name}");
        assert_eq!(selection.excluded_unworked, 0, "{name}");
    }
}

/// A **wait** stage is never conditioned on duration: `merge_wait` of 0 s (an
/// already-mergeable PR) and `ready_wait` of 0 s (an issue dispatched into a
/// free slot) are real measurements. Only the three stages whose whole
/// duration is one role running are role-attempt stages.
#[test]
fn only_the_role_attempt_stages_are_conditioned_on_a_zero_duration() {
    assert_eq!(
        Stage::EVERY
            .into_iter()
            .filter(|s| s.is_role_attempt())
            .collect::<Vec<_>>(),
        vec![Stage::SweepCurator, Stage::SweepBuilder, Stage::Doctor]
    );
    let record = outcome(&[
        ("curator", 0),
        ("builder", 0),
        ("doctor", 0),
        ("judge", 0),
        ("merge", 0),
    ]);
    let mut history = StageSamples::default();
    history.push_outcome(&record, as_of() - Duration::hours(1), "host-fixture-a");
    let worked: Vec<(Stage, Option<bool>)> =
        history.stages.iter().map(|s| (s.stage, s.worked)).collect();
    assert_eq!(
        worked,
        vec![
            (Stage::SweepCurator, Some(false)),
            (Stage::SweepBuilder, Some(false)),
            (Stage::Doctor, Some(false)),
            (Stage::ReviewWait, None),
            (Stage::MergeWait, None),
        ]
    );
    // A positive role phase duration carries no verdict either way.
    let positive = outcome(&[("builder", 1)]);
    let mut history = StageSamples::default();
    history.push_outcome(&positive, as_of() - Duration::hours(1), "host-fixture-a");
    assert_eq!(history.stages[0].worked, None);
}

// --------------------------------------------- measurably inert on real feeds

/// The whole point of conditioning on an explicit flag rather than on a
/// threshold: a feed with no artefact row in it is refused nothing, so its v1
/// estimate is byte-identical to its pre-#9420 self. (On this host's real
/// journals the refusal is 2 rows out of 2,810 — see [`super::super::history`]
/// `worked_phase` — so the conditioning is close to inert on local data and
/// decisive on the span aggregate it exists for.)
#[test]
fn a_worked_only_feed_is_refused_nothing_and_land_v1_is_unchanged() {
    let history = history_a();
    assert!(
        history.stages.iter().all(|s| s.worked != Some(false)),
        "the fixture history (and every feed shaped like it) is worked-only"
    );
    assert!(history.censored.iter().all(|s| s.worked != Some(false)));
    for stage in Stage::ALL {
        if let Some(selection) = history.select("rjwalters/loom", stage, as_of(), &SOURCES) {
            assert_eq!(selection.excluded_unworked, 0, "{stage}");
        }
    }
    // And the golden estimate still reproduces.
    let explanation = LandV1.estimate(&input_at(Stage::ReviewWait, 0, 0), &history);
    let golden: crate::eta::explanation::Explanation =
        serde_json::from_str(super::EXPLANATION_GOLDEN).expect("golden parses");
    assert_eq!(explanation.stages, golden.stages);
    assert_eq!(explanation.result, golden.result);
}

/// A `sweep.outcome` record carrying exactly `phases`, decoded from the wire
/// so this fixture cannot drift from the real record shape.
fn outcome(phases: &[(&str, i64)]) -> SweepOutcomeRecord {
    let durations: Vec<String> = phases
        .iter()
        .map(|(phase, secs)| format!(r#"{{"phase":"{phase}","duration_sec":{secs}}}"#))
        .collect();
    serde_json::from_str(&format!(
        r#"{{"kind":"sweep.outcome","repo":"rjwalters/loom","visibility":"public",
            "issue":9420,"sweep_id":"sweep-issue-9420-1790000000",
            "phase_durations":[{}],"total_duration_sec":17,"result":"success"}}"#,
        durations.join(",")
    ))
    .expect("the fixture record decodes")
}
