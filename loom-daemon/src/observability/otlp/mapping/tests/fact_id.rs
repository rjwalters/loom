//! `loom.fact_id` -- the cross-host id of outcome facts (Issues #11125,
//! #11126): keyed on forge-observed instants, else the holding sweep's id or
//! a forge-dated entry (#11401), and never host- or emission-dependent.

use super::*;
use crate::telemetry::kinds::fleet_state::FleetStage;
use crate::telemetry::kinds::pr_resolved::{PrResolution, PrResolvedRecord};
use crate::telemetry::kinds::stage_outcome::{
    EnteredAtSource, FactIdMissingReason, StageExit, StageOutcomeRecord,
};
use crate::telemetry::provenance::Provenance;
use chrono::Duration;

fn provenance() -> Provenance {
    Provenance {
        version: "0.19.800".to_string(),
        revision: "9d8e226ce0123456789abcdef0123456789abcde".to_string(),
        tree_state: "clean".to_string(),
        complete: true,
    }
}

fn pr_record(number: u32, state: PrResolution, closed_at: i64) -> PrResolvedRecord {
    PrResolvedRecord {
        repo: "o/r".to_string(),
        pr_number: number,
        issue: Some(1),
        state,
        resolved_at: ts(),
        observed_at: ts() + Duration::seconds(30),
        resolution_sec: 0,
        closed_at: Some(ts() + Duration::seconds(closed_at)),
        loom: provenance(),
    }
}

fn pr(number: u32, state: PrResolution) -> TelemetryRecord {
    TelemetryRecord::PrResolved(pr_record(number, state, 0))
}

/// One host's record of `issue` leaving `stage` for `next`, whose forge
/// label event is at `ts()`, polled `poll_secs` after it.
fn stage_record(issue: u32, stage: FleetStage, next: FleetStage, poll: i64) -> StageOutcomeRecord {
    StageOutcomeRecord {
        repo: "o/r".to_string(),
        issue,
        pr_number: Some(issue + 1),
        stage,
        entered_at: None,
        entered_at_source: Some(EnteredAtSource::Unknown),
        left_at: ts(),
        dwell_sec: None,
        exit: StageExit::between(stage, next),
        next_stage: Some(next),
        event: "fleet.state".to_string(),
        observed_at: ts() + Duration::seconds(poll),
        resolution_sec: Some(0),
        forge_transition_at: Some(ts()),
        sweep_id: None,
        fact_id_missing_reason: None,
        loom: provenance(),
    }
}

fn stage(issue: u32, stage: FleetStage) -> TelemetryRecord {
    TelemetryRecord::StageOutcome(stage_record(issue, stage, FleetStage::MergeWait, 30))
}

fn ids(host: &str, record: &TelemetryRecord, shift_secs: i64) -> (String, String) {
    let mut e = envelope(host, record.clone());
    e.emitted_at += Duration::seconds(shift_secs);
    let log = log_record_for(&e).expect("maps to a log");
    let get = |key: &str| {
        log.attributes
            .iter()
            .find(|kv| kv.key == key)
            .and_then(|kv| match kv.value.as_ref()?.value.as_ref()? {
                any_value::Value::StringValue(s) => Some(s.clone()),
                _ => None,
            })
            .unwrap_or_default()
    };
    (get("loom.fact_id"), get("loom.record_id"))
}

#[test]
fn fact_id_ignores_host_and_emitted_at_but_record_id_does_not() {
    for record in [
        pr(7, PrResolution::Merged),
        stage(9, FleetStage::ReviewWait),
    ] {
        let (fact_a, rec_a) = ids("host-a", &record, 0);
        let (fact_b, rec_b) = ids("host-b", &record, 90);
        assert_eq!(fact_a.len(), 16);
        assert_eq!(fact_a, fact_b, "same natural key, same fact id");
        assert_ne!(rec_a, rec_b, "record_id still distinguishes deliveries");
    }
}

/// Two hosts poll at different times, so their `left_at`, `observed_at` and
/// `resolution_sec` differ; the forge's label-event instant does not.
#[test]
fn two_hosts_polling_one_transition_at_different_times_share_the_fact_id() {
    let early = stage_record(9, FleetStage::ReviewWait, FleetStage::MergeWait, 40);
    let late = StageOutcomeRecord {
        left_at: ts() + Duration::seconds(290),
        resolution_sec: Some(300),
        ..stage_record(9, FleetStage::ReviewWait, FleetStage::MergeWait, 290)
    };
    let a = ids("host-a", &TelemetryRecord::StageOutcome(early), 0).0;
    let b = ids("host-b", &TelemetryRecord::StageOutcome(late), 250).0;
    assert_eq!(a.len(), 16);
    assert_eq!(a, b);
}

/// The value of `key` on the log `record` maps to from `host`, or "".
fn attr_of(host: &str, record: &TelemetryRecord, key: &str) -> String {
    let log = log_record_for(&envelope(host, record.clone())).expect("maps to a log");
    log.attributes
        .iter()
        .find(|kv| kv.key == key)
        .and_then(|kv| match kv.value.as_ref()?.value.as_ref()? {
            any_value::Value::StringValue(s) => Some(s.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

/// A sweep-stage exit with no forge instant: what `outcomes` builds for a
/// row of this host's sweep `sweep`, entered at checkpoint `entered`.
fn sweep_record(sweep: &str, entered: i64) -> StageOutcomeRecord {
    StageOutcomeRecord {
        forge_transition_at: None,
        sweep_id: Some(sweep.to_string()),
        entered_at: Some(ts() - Duration::seconds(entered)),
        entered_at_source: Some(EnteredAtSource::Checkpoint),
        resolution_sec: Some(300),
        fact_id_missing_reason: Some(FactIdMissingReason::NoForgeInstant),
        ..stage_record(9, FleetStage::SweepCurator, FleetStage::SweepBuilder, 30)
    }
}

#[test]
fn a_transition_with_nothing_to_key_on_carries_a_reason_instead() {
    let record = StageOutcomeRecord {
        forge_transition_at: None,
        ..stage_record(9, FleetStage::SweepCurator, FleetStage::SweepBuilder, 30)
    };
    let record = TelemetryRecord::StageOutcome(record);
    let (fact, record_id) = ids("host-a", &record, 0);
    assert_eq!(fact, "", "no key, no fact id");
    assert_eq!(record_id.len(), 16);
    assert_eq!(
        attr_of("host-a", &record, "loom.fact_id_missing_reason"),
        "no_forge_instant",
        "an unset reason still reads as one"
    );
    let failed = TelemetryRecord::StageOutcome(StageOutcomeRecord {
        forge_transition_at: None,
        fact_id_missing_reason: Some(FactIdMissingReason::ForgeReadFailed),
        ..stage_record(9, FleetStage::ReviewWait, FleetStage::MergeWait, 30)
    });
    assert_eq!(attr_of("h", &failed, "loom.fact_id_missing_reason"), "forge_read_failed");
    let pr = PrResolvedRecord {
        closed_at: None,
        ..pr_record(7, PrResolution::Merged, 0)
    };
    assert_eq!(ids("host-a", &TelemetryRecord::PrResolved(pr), 0).0, "");
}

/// #11401: a sweep stage has no forge instant, so it keys on the sweep id;
/// the host, `observed_at`, `left_at` and emission time do not matter.
#[test]
fn a_sweep_stage_exit_is_keyed_on_its_sweep_id() {
    let a = TelemetryRecord::StageOutcome(sweep_record("sweep-issue-9-1", 600));
    let b = TelemetryRecord::StageOutcome(StageOutcomeRecord {
        observed_at: ts() + Duration::seconds(999),
        left_at: ts() + Duration::seconds(240),
        ..sweep_record("sweep-issue-9-1", 600)
    });
    let (fact_a, rec_a) = ids("host-a", &a, 0);
    let (fact_b, rec_b) = ids("host-b", &b, 90);
    assert_eq!(fact_a.len(), 16);
    assert_eq!(fact_a, fact_b, "stable across re-emission and hosts");
    assert_ne!(rec_a, rec_b);
    assert_eq!(attr_of("host-a", &a, "loom.fact_id_missing_reason"), "", "keyed: no reason");
    let other = TelemetryRecord::StageOutcome(sweep_record("sweep-issue-9-2", 600));
    assert_ne!(fact_a, ids("h", &other, 0).0, "another sweep, another fact");
    let reentry = TelemetryRecord::StageOutcome(sweep_record("sweep-issue-9-1", 60));
    assert_ne!(fact_a, ids("h", &reentry, 0).0, "a re-entry within the sweep is another fact");
}

/// No re-keying: an id derived from a forge instant is exactly the #11125 key,
/// whatever sweep id or entry the record also carries.
#[test]
fn a_forge_instant_id_is_unchanged() {
    let base = stage_record(9, FleetStage::ReviewWait, FleetStage::MergeWait, 30);
    let golden = super::super::fact_id::derive(
        "eta.stage_outcome",
        &[
            "o/r",
            "9",
            "review_wait",
            "merge_wait",
            &crate::telemetry::trace::instant(ts()),
        ],
    );
    let with_extras = StageOutcomeRecord {
        sweep_id: Some("sweep-issue-9-1".to_string()),
        entered_at: Some(ts() - Duration::seconds(60)),
        entered_at_source: Some(EnteredAtSource::Forge),
        ..base.clone()
    };
    for record in [base, with_extras] {
        assert_eq!(ids("h", &TelemetryRecord::StageOutcome(record), 0).0, golden);
    }
}

/// A failed exit read with a forge-dated entry keys on the entry; a
/// checkpoint entry without a sweep id is no key.
#[test]
fn a_forge_dated_entry_keys_an_exit_with_no_forge_instant() {
    let entry = |source| StageOutcomeRecord {
        forge_transition_at: None,
        entered_at: Some(ts() - Duration::seconds(600)),
        entered_at_source: Some(source),
        fact_id_missing_reason: Some(FactIdMissingReason::ForgeReadFailed),
        ..stage_record(9, FleetStage::MergeHold, FleetStage::MergeWait, 30)
    };
    let forge = TelemetryRecord::StageOutcome(entry(EnteredAtSource::Forge));
    let (fact, _) = ids("host-a", &forge, 0);
    assert_eq!(fact.len(), 16);
    assert_eq!(fact, ids("host-b", &forge, 300).0);
    assert_eq!(attr_of("h", &forge, "loom.fact_id_missing_reason"), "");
    let checkpoint = TelemetryRecord::StageOutcome(entry(EnteredAtSource::Checkpoint));
    assert_eq!(ids("h", &checkpoint, 0).0, "");
    assert_eq!(attr_of("h", &checkpoint, "loom.fact_id_missing_reason"), "forge_read_failed");
}

/// Every combination of the key sources maps to exactly one of
/// `loom.fact_id` and `loom.fact_id_missing_reason`, never neither.
#[test]
fn every_stage_outcome_carries_a_fact_id_or_a_reason() {
    let sources = [
        None,
        Some(EnteredAtSource::Forge),
        Some(EnteredAtSource::Checkpoint),
        Some(EnteredAtSource::Unknown),
    ];
    for forge_at in [None, Some(ts())] {
        for sweep in [None, Some(""), Some("sweep-issue-9-1")] {
            for entered in [None, Some(ts() - Duration::seconds(60))] {
                for source in sources {
                    for reason in [None, Some(FactIdMissingReason::ForgeReadFailed)] {
                        let r = StageOutcomeRecord {
                            forge_transition_at: forge_at,
                            sweep_id: sweep.map(str::to_string),
                            entered_at: entered,
                            entered_at_source: source,
                            fact_id_missing_reason: reason,
                            ..stage_record(9, FleetStage::Doctor, FleetStage::ReviewWait, 30)
                        };
                        let record = TelemetryRecord::StageOutcome(r.clone());
                        let fact = ids("h", &record, 0).0;
                        let why = attr_of("h", &record, "loom.fact_id_missing_reason");
                        assert!(fact.is_empty() != why.is_empty(), "{fact:?} {why:?} {r:?}");
                        assert_eq!(fact.is_empty(), !r.has_fact_key(), "{r:?}");
                    }
                }
            }
        }
    }
}

/// An item whose issue closed leaves with `abandoned` at the forge's close
/// instant (#11398): any host, polling at any time, shares the fact id.
#[test]
fn an_abandoned_exit_is_one_fact_across_hosts() {
    let closed = |observed: i64| StageOutcomeRecord {
        exit: StageExit::Abandoned,
        next_stage: None,
        ..stage_record(9, FleetStage::SweepCurator, FleetStage::SweepBuilder, observed)
    };
    let a = ids("host-a", &TelemetryRecord::StageOutcome(closed(30)), 0).0;
    let b = ids("host-b", &TelemetryRecord::StageOutcome(closed(30)), 400).0;
    assert_eq!(a.len(), 16);
    assert_eq!(a, b);
}

#[test]
fn a_pr_closed_reopened_and_closed_again_is_two_facts() {
    let first = TelemetryRecord::PrResolved(pr_record(7, PrResolution::Closed, 0));
    let second = TelemetryRecord::PrResolved(pr_record(7, PrResolution::Closed, 3600));
    assert_ne!(ids("h", &first, 0).0, ids("h", &second, 0).0);
}

#[test]
fn fact_id_differs_across_natural_keys() {
    let all = [
        pr(7, PrResolution::Merged),
        pr(7, PrResolution::Closed),
        pr(8, PrResolution::Merged),
        stage(9, FleetStage::ReviewWait),
        stage(9, FleetStage::Doctor),
        stage(10, FleetStage::ReviewWait),
        TelemetryRecord::StageOutcome(stage_record(
            9,
            FleetStage::ReviewWait,
            FleetStage::Doctor,
            30,
        )),
    ]
    .map(|r| ids("h", &r, 0).0);
    for (i, x) in all.iter().enumerate() {
        for y in &all[i + 1..] {
            assert_ne!(x, y);
        }
    }
}

/// End to end through the producer: two hosts poll the same review-label
/// move at different times and on different cadences, and map to one fact.
#[test]
fn two_hosts_running_the_producer_at_different_times_emit_one_fact() {
    use crate::observability::fleet_state::history::HistoryEvent;
    use crate::observability::fleet_state::outcomes::{
        diff, listed, remember, ForgeReads, IssueFacts, LabelHistory, Memory, Pass, PullFacts,
    };
    use crate::observability::fleet_state::{build_view, FleetInput, ListedPr, RepoListing};
    use std::collections::BTreeMap;

    struct Forge(DateTime<Utc>);
    impl ForgeReads for Forge {
        fn pull(&mut self, _: &str, _: u32) -> Option<PullFacts> {
            None
        }
        fn issue(&mut self, _: &str, _: u32) -> Option<IssueFacts> {
            Some(IssueFacts::Open)
        }

        fn label_history(&mut self, _: &str, _: u32) -> Option<LabelHistory> {
            Some(LabelHistory {
                events: vec![(self.0, HistoryEvent::Labeled("loom:pr".to_string()))],
            })
        }
    }
    let input = |label: &str| FleetInput {
        champion: BTreeMap::new(),
        host_id: "h".to_string(),
        managed: ["o/r".to_string()].into(),
        held: Vec::new(),
        listings: vec![RepoListing {
            repo: "o/r".to_string(),
            prs: vec![ListedPr {
                number: 8,
                labels: vec![label.to_string()],
                issue: Some(7),
            }],
        }],
        listed_at: None,
        ready: None,
        capacity: None,
        main_ci: BTreeMap::new(),
    };
    let approved_at = ts() + Duration::seconds(100);
    let host = |name: &str, first: i64, second: i64| {
        let mut memory = Memory::default();
        let mut prev = None;
        let mut records = Vec::new();
        for (at, label) in [(first, "loom:review-requested"), (second, "loom:pr")] {
            let now = ts() + Duration::seconds(at);
            let fleet = input(label);
            let view = build_view(&fleet, prev.as_ref(), now);
            let listed = listed(&fleet.listings);
            let pass = Pass {
                view: &view,
                listed: &listed,
                managed: &fleet.managed,
                now,
            };
            records = diff(&mut memory, &pass, &mut Forge(approved_at), &provenance());
            memory = remember(memory, &pass);
            prev = Some(view);
        }
        assert_eq!(records.len(), 1, "{records:?}");
        ids(name, &records[0], second)
    };
    let (fact_a, record_a) = host("host-a", 0, 130);
    let (fact_b, record_b) = host("host-b", 60, 360);
    assert_eq!(fact_a.len(), 16);
    assert_eq!(fact_a, fact_b, "one transition, one fact");
    assert_ne!(record_a, record_b);
}

/// End to end through the producer (#11367): the host whose sweep claimed a
/// ready issue and a host that only saw it leave the ready queue key the exit
/// on the same forge facts, so they map to one fact.
#[test]
fn a_claimed_ready_issue_is_one_fact_across_hosts() {
    use crate::observability::fleet_state::history::HistoryEvent;
    use crate::observability::fleet_state::outcomes::{
        diff, listed, remember, ForgeReads, IssueFacts, LabelHistory, Memory, Pass, PullFacts,
    };
    use crate::observability::fleet_state::{
        build_view, FleetInput, HeldSweep, ReadyItem, ReadyQueue,
    };
    use std::collections::BTreeMap;

    let claimed_at = ts() + Duration::seconds(100);
    struct Forge(DateTime<Utc>);
    impl ForgeReads for Forge {
        fn pull(&mut self, _: &str, _: u32) -> Option<PullFacts> {
            None
        }
        fn issue(&mut self, _: &str, _: u32) -> Option<IssueFacts> {
            Some(IssueFacts::Open)
        }

        fn label_history(&mut self, _: &str, _: u32) -> Option<LabelHistory> {
            let label = |l: &str| l.to_string();
            Some(LabelHistory {
                events: vec![
                    (self.0 - Duration::hours(1), HistoryEvent::Labeled(label("loom:issue"))),
                    (self.0, HistoryEvent::Unlabeled(label("loom:issue"))),
                    (self.0, HistoryEvent::Labeled(label("loom:building"))),
                ],
            })
        }
    }
    let input = |ready: bool, held: bool| FleetInput {
        champion: BTreeMap::new(),
        host_id: "h".to_string(),
        managed: ["o/r".to_string()].into(),
        held: held
            .then(|| HeldSweep {
                repo: "o/r".to_string(),
                issue: 7,
                stage: FleetStage::SweepCurator,
                entered_at: claimed_at,
                entered_at_lower_bound: false,
                pr: None,
                overflow: false,
                start: Default::default(),
                sweep_id: None,
            })
            .into_iter()
            .collect(),
        listings: Vec::new(),
        listed_at: None,
        ready: Some(ReadyQueue {
            items: ready
                .then(|| ReadyItem {
                    repo: "o/r".to_string(),
                    issue: 7,
                    rank: 1,
                    level: 0,
                    fleet_priority: 0,
                    created_at: None,
                    main_red_fix: false,
                    eligible_reason: None,
                    eligible_at: None,
                })
                .into_iter()
                .collect(),
            listed: ["o/r".to_string()].into(),
            complete: ["o/r".to_string()].into(),
            slots: None,
            halts: Default::default(),
            limits: Default::default(),
        }),
        capacity: None,
        main_ci: BTreeMap::new(),
    };
    let host = |name: &str, claims: bool, first: i64, second: i64| {
        let mut memory = Memory::default();
        let mut prev = None;
        let mut records = Vec::new();
        for (at, fleet) in [(first, input(true, false)), (second, input(false, claims))] {
            let now = ts() + Duration::seconds(at);
            let view = build_view(&fleet, prev.as_ref(), now);
            let listed = listed(&fleet.listings);
            let pass = Pass {
                view: &view,
                listed: &listed,
                managed: &fleet.managed,
                now,
            };
            records = diff(&mut memory, &pass, &mut Forge(claimed_at), &provenance());
            memory = remember(memory, &pass);
            prev = Some(view);
        }
        assert_eq!(records.len(), 1, "{records:?}");
        ids(name, &records[0], second)
    };
    let (fact_a, _) = host("host-a", true, 0, 130);
    let (fact_b, _) = host("host-b", false, 60, 360);
    assert_eq!(fact_a.len(), 16);
    assert_eq!(fact_a, fact_b, "one claim, one fact");
}

/// End to end through the producer (#11398): an issue closed by hand as
/// `completed` mid-sweep, with no closing commit, is one `abandoned` exit at
/// its `closed_at` with dwell, and two hosts on different cadences map it to
/// one fact.
#[test]
fn a_hand_closed_issue_mid_sweep_is_one_abandoned_fact_across_hosts() {
    use crate::observability::fleet_state::outcomes::{
        diff, listed, remember, ForgeReads, IssueFacts, LabelHistory, Memory, Pass, PullFacts,
    };
    use crate::observability::fleet_state::{build_view, FleetInput, HeldSweep};
    use std::collections::BTreeMap;

    let closed_at = ts() + Duration::seconds(100);
    struct Forge(DateTime<Utc>);
    impl ForgeReads for Forge {
        fn pull(&mut self, _: &str, _: u32) -> Option<PullFacts> {
            None
        }
        fn issue(&mut self, _: &str, _: u32) -> Option<IssueFacts> {
            let close = serde_json::json!([{
                "event": "closed",
                "created_at": "2026-10-11T00:00:00Z",
                "commit_id": null,
            }]);
            Some(IssueFacts::closed("rjwalters/loom", 1, self.0, &[close], &[]))
        }
        fn label_history(&mut self, _: &str, _: u32) -> Option<LabelHistory> {
            None
        }
    }
    let input = |held: bool| FleetInput {
        champion: BTreeMap::new(),
        host_id: "h".to_string(),
        managed: ["o/r".to_string()].into(),
        held: held
            .then(|| HeldSweep {
                repo: "o/r".to_string(),
                issue: 7,
                stage: FleetStage::SweepCurator,
                entered_at: ts(),
                entered_at_lower_bound: false,
                pr: None,
                overflow: false,
                start: Default::default(),
                sweep_id: None,
            })
            .into_iter()
            .collect(),
        listings: Vec::new(),
        listed_at: None,
        ready: None,
        capacity: None,
        main_ci: BTreeMap::new(),
    };
    let host = |name: &str, first: i64, step: i64| {
        let mut memory = Memory::default();
        let mut prev = None;
        let mut all = Vec::new();
        for (i, held) in [true, false, false].into_iter().enumerate() {
            let now = ts() + Duration::seconds(first + step * i as i64);
            let fleet = input(held);
            let view = build_view(&fleet, prev.as_ref(), now);
            let listed = listed(&fleet.listings);
            let pass = Pass {
                view: &view,
                listed: &listed,
                managed: &fleet.managed,
                now,
            };
            all.extend(diff(&mut memory, &pass, &mut Forge(closed_at), &provenance()));
            memory = remember(memory, &pass);
            prev = Some(view);
        }
        assert_eq!(all.len(), 1, "exactly one exit: {all:?}");
        let TelemetryRecord::StageOutcome(r) = &all[0] else {
            panic!("{all:?}");
        };
        assert_eq!((r.exit, r.left_at), (StageExit::Abandoned, closed_at));
        assert_eq!((r.forge_transition_at, r.dwell_sec), (Some(closed_at), Some(100)));
        ids(name, &all[0], first)
    };
    let (fact_a, record_a) = host("host-a", 30, 120);
    let (fact_b, record_b) = host("host-b", 60, 400);
    assert_eq!(fact_a.len(), 16);
    assert_eq!(fact_a, fact_b, "one close, one fact");
    assert_ne!(record_a, record_b);
}
