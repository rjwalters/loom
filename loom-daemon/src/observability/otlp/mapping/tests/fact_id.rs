//! `loom.fact_id` -- the cross-host id of outcome facts (Issues #11125,
//! #11126): keyed on forge-observed instants only.

use super::*;
use crate::telemetry::kinds::fleet_state::FleetStage;
use crate::telemetry::kinds::pr_resolved::{PrResolution, PrResolvedRecord};
use crate::telemetry::kinds::stage_outcome::{StageExit, StageOutcomeRecord};
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
        left_at: ts(),
        dwell_sec: None,
        exit: StageExit::between(stage, next),
        next_stage: Some(next),
        event: "fleet.state".to_string(),
        observed_at: ts() + Duration::seconds(poll),
        resolution_sec: Some(0),
        forge_transition_at: Some(ts()),
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

#[test]
fn a_transition_with_no_forge_instant_carries_no_fact_id() {
    let record = StageOutcomeRecord {
        forge_transition_at: None,
        ..stage_record(9, FleetStage::SweepCurator, FleetStage::SweepBuilder, 30)
    };
    let (fact, record_id) = ids("host-a", &TelemetryRecord::StageOutcome(record), 0);
    assert_eq!(fact, "", "no forge instant, no fact id");
    assert_eq!(record_id.len(), 16);
    let pr = PrResolvedRecord {
        closed_at: None,
        ..pr_record(7, PrResolution::Merged, 0)
    };
    assert_eq!(ids("host-a", &TelemetryRecord::PrResolved(pr), 0).0, "");
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
    use crate::observability::fleet_state::outcomes::{
        diff, listed, remember, ForgeReads, Memory, Pass, PullFacts,
    };
    use crate::observability::fleet_state::{build_view, FleetInput, ListedPr, RepoListing};
    use std::collections::BTreeMap;

    struct Forge(DateTime<Utc>);
    impl ForgeReads for Forge {
        fn pull(&mut self, _: &str, _: u32) -> Option<PullFacts> {
            None
        }
        fn label_times(&mut self, _: &str, _: u32) -> Option<BTreeMap<String, DateTime<Utc>>> {
            Some([("loom:pr".to_string(), self.0)].into())
        }
    }
    let input = |label: &str| FleetInput {
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
