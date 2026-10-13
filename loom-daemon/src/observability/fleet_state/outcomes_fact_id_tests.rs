//! Every `eta.stage_outcome` the producer builds has a `loom.fact_id` key or
//! a missing-reason (#11401), beside the shared helpers of `outcomes_tests`.

use super::*;
use crate::telemetry::kinds::stage_outcome::{fact_id_missing_total, FactIdMissingReason};

fn hold() -> &'static str {
    MERGE_HOLD_LABELS.iter().next().expect("a hold label")
}

fn held_sweep(stage: FleetStage, pr: Option<u32>, sweep_id: &str) -> HeldSweep {
    HeldSweep {
        repo: REPO.to_string(),
        issue: 40,
        stage,
        entered_at: t0() - Duration::minutes(20),
        entered_at_lower_bound: false,
        pr,
        overflow: false,
        start: Default::default(),
        sweep_id: Some(sweep_id.to_string()),
    }
}

/// The input of PR 21 (for issue 20) carrying `labels`; no labels is a PR
/// that left the review listings.
fn pr_with(labels: &[&str]) -> FleetInput {
    if labels.is_empty() {
        return input(&[], Vec::new());
    }
    let mut i = input(&[(21, labels[0], 20)], Vec::new());
    i.listings[0].prs[0].labels = labels.iter().map(|l| (*l).to_string()).collect();
    i
}

/// The producer's own invariant: a record either has a fact-id key or says
/// why not.
fn assert_keyed_or_explained(records: &[TelemetryRecord], case: &str) {
    for r in stages(records) {
        assert!(
            r.has_fact_key() || r.fact_id_missing_reason.is_some(),
            "{case}: neither a fact-id key nor a reason: {r:?}"
        );
        assert_eq!(
            r.has_fact_key(),
            r.missing_fact_id_reason().is_none(),
            "{case}: exactly one of a fact id and a reason"
        );
    }
}

#[test]
fn a_held_sweep_stage_exit_is_keyed_on_its_sweep_id() {
    let mut forge = Forge::default();
    let out = run(
        &[
            input(
                &[],
                vec![held_sweep(
                    FleetStage::SweepCurator,
                    None,
                    "sweep-issue-40-1",
                )],
            ),
            input(
                &[],
                vec![held_sweep(
                    FleetStage::SweepBuilder,
                    None,
                    "sweep-issue-40-1",
                )],
            ),
            input(&[], Vec::new()),
        ],
        Duration::minutes(5),
        &mut forge,
    );
    for pass in &out[1..] {
        let r = stages(pass)[0];
        assert_eq!(r.forge_transition_at, None);
        assert_eq!(r.sweep_id.as_deref(), Some("sweep-issue-40-1"));
        assert!(r.has_fact_key(), "{r:?}");
        assert_eq!(r.missing_fact_id_reason(), None);
    }
    assert_eq!(stages(&out[2])[0].stage, FleetStage::SweepBuilder);
}

/// Every `fleet.state` stage, left for every other stage or out of the
/// listings, under every forge failure: no exit has neither a fact-id key nor
/// a reason, and a held sweep's exit always has a key.
#[test]
fn no_exit_of_any_stage_is_emitted_without_a_fact_id_or_a_reason() {
    let hold = hold();
    let label_sets: [&[&str]; 5] = [
        &["loom:review-requested"],
        &["loom:changes-requested"],
        &["loom:pr"],
        &["loom:pr", hold],
        &[],
    ];
    let merged = PullFacts {
        merged_at: Some(t0() + Duration::minutes(3)),
        closed_at: Some(t0() + Duration::minutes(3)),
    };
    let modes = ["ok", "history_fails", "pull_fails", "pr_open"];
    for mode in modes {
        let forge_for = || {
            let mut forge = Forge::default();
            forge.label(21, "loom:review-requested", t0() - Duration::hours(3));
            forge.label(21, "loom:pr", t0() - Duration::hours(2));
            forge.label(21, "loom:changes-requested", t0() + Duration::minutes(2));
            match mode {
                "history_fails" => forge.history_fails = true,
                "pull_fails" => forge.pull_failures = usize::MAX,
                "pr_open" => {
                    forge.pulls.insert(
                        21,
                        PullFacts {
                            merged_at: None,
                            closed_at: None,
                        },
                    );
                }
                _ => {
                    forge.pulls.insert(21, merged);
                }
            }
            forge
        };
        for from in &label_sets[..4] {
            for to in label_sets {
                if *from == to {
                    continue;
                }
                let out =
                    run(&[pr_with(from), pr_with(to)], Duration::minutes(5), &mut forge_for());
                assert_keyed_or_explained(&out[1], &format!("{mode}: {from:?} -> {to:?}"));
            }
        }
        let sweep_stages = [
            (FleetStage::SweepCurator, None),
            (FleetStage::SweepBuilder, None),
            (FleetStage::ReviewWait, Some(21)),
            (FleetStage::Doctor, Some(21)),
            (FleetStage::MergeWait, Some(21)),
        ];
        for (from, from_pr) in sweep_stages {
            let nexts = sweep_stages.iter().map(|s| Some(*s)).chain([None]);
            for next in nexts.filter(|n| n.is_none_or(|(s, _)| s != from)) {
                let after = next.map(|(s, pr)| held_sweep(s, pr, "sweep-issue-40-9"));
                let out = run(
                    &[
                        input(&[], vec![held_sweep(from, from_pr, "sweep-issue-40-9")]),
                        input(&[], after.into_iter().collect()),
                    ],
                    Duration::minutes(5),
                    &mut forge_for(),
                );
                let case = format!("{mode}: held {from:?} -> {next:?}");
                assert_keyed_or_explained(&out[1], &case);
                for r in stages(&out[1]) {
                    assert!(r.has_fact_key(), "{case}: a sweep stage is always keyed: {r:?}");
                }
            }
        }
    }
}

#[test]
fn a_released_hold_is_dated_by_the_hold_removal() {
    let hold = hold();
    let released_at = t0() + Duration::minutes(7);
    let mut forge = Forge::default();
    forge.label(21, "loom:pr", t0() - Duration::hours(2));
    forge.label(21, hold, t0() - Duration::hours(1));
    forge.unlabel(21, hold, released_at);
    let out = run(
        &[
            pr_with(&["loom:pr", hold]),
            pr_with(&["loom:pr", hold]),
            pr_with(&["loom:pr"]),
        ],
        Duration::minutes(5),
        &mut forge,
    );
    let r = stages(&out[2])[0];
    assert_eq!((r.stage, r.exit), (FleetStage::MergeHold, StageExit::Released));
    assert_eq!(r.forge_transition_at, Some(released_at), "the hold's unlabeled event");
    assert_eq!((r.left_at, r.resolution_sec), (released_at, Some(0)));
    assert_eq!(r.fact_id_missing_reason, None);
}

#[test]
fn a_failed_read_with_no_forge_entry_says_so() {
    let mut forge = Forge {
        history_fails: true,
        ..Forge::default()
    };
    let out = run(
        &[pr_with(&["loom:review-requested"]), pr_with(&["loom:pr"])],
        Duration::minutes(5),
        &mut forge,
    );
    let r = stages(&out[1])[0];
    assert!(!r.has_fact_key());
    assert_eq!(r.missing_fact_id_reason(), Some(FactIdMissingReason::ForgeReadFailed));
}

#[test]
fn reads_that_succeed_but_date_nothing_say_no_forge_instant() {
    let mut forge = Forge::default();
    forge.label(21, "loom:pr", t0() - Duration::days(3));
    let out = run(
        &[pr_with(&["loom:review-requested"]), pr_with(&["loom:pr"])],
        Duration::minutes(5),
        &mut forge,
    );
    let r = stages(&out[1])[0];
    assert_eq!((r.forge_transition_at, r.entered_at), (None, None));
    assert_eq!(r.missing_fact_id_reason(), Some(FactIdMissingReason::NoForgeInstant));
}

#[test]
fn a_failed_exit_read_falls_back_to_the_forge_dated_entry() {
    // The PR's `pulls/{n}` read fails, so its departure has no forge instant;
    // its history still dates the `merge_wait` entry.
    let mut forge = Forge {
        pull_failures: 1,
        ..Forge::default()
    };
    forge.label(21, "loom:pr", t0() - Duration::hours(1));
    let out = run(&[pr_with(&["loom:pr"]), pr_with(&[])], Duration::minutes(5), &mut forge);
    let r = stages(&out[1])[0];
    assert_eq!((r.exit, r.forge_transition_at), (StageExit::Unknown, None));
    assert_eq!(r.fact_id_missing_reason, Some(FactIdMissingReason::ForgeReadFailed));
    assert_eq!(r.forge_entered_at(), Some(t0() - Duration::hours(1)));
    assert!(r.has_fact_key(), "keyed on the forge-dated entry");
}

#[test]
fn offering_an_exit_without_a_fact_id_is_counted_for_host_export() {
    let mut forge = Forge {
        history_fails: true,
        ..Forge::default()
    };
    let out = run(
        &[pr_with(&["loom:review-requested"]), pr_with(&["loom:pr"])],
        Duration::minutes(5),
        &mut forge,
    );
    let queue = Arc::new(CapturingQueue::default());
    let sink = FleetStateSink::new(queue, "host-a");
    let before = fact_id_missing_total();
    assert_eq!(offer(out[1].clone(), &sink), 1);
    // The counter is process-wide and tests run in parallel: at least ours.
    assert!(fact_id_missing_total() > before);
}
