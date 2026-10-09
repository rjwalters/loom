//! `loom.fact_id` -- the cross-host id of outcome facts (Issue #11125).

use super::*;
use crate::eta::Provenance;
use crate::eta::Stage;
use crate::telemetry::kinds::eta_stage_outcome::{EtaStageOutcomeRecord, StageExit};
use crate::telemetry::kinds::pr_resolved::{PrResolution, PrResolvedRecord};
use chrono::Duration;

fn provenance() -> Provenance {
    Provenance {
        version: "0.19.800".to_string(),
        revision: "9d8e226ce0123456789abcdef0123456789abcde".to_string(),
        tree_state: "clean".to_string(),
        complete: true,
    }
}

fn pr(number: u32, state: PrResolution) -> TelemetryRecord {
    TelemetryRecord::PrResolved(PrResolvedRecord {
        repo: "o/r".to_string(),
        pr_number: number,
        issue: Some(1),
        state,
        resolved_at: ts(),
        observed_at: ts() + Duration::seconds(30),
        resolution_sec: 0,
        loom: provenance(),
    })
}

fn stage(issue: u32, stage: Stage) -> TelemetryRecord {
    TelemetryRecord::EtaStageOutcome(EtaStageOutcomeRecord {
        repo: "o/r".to_string(),
        repo_id: None,
        issue,
        pr_number: None,
        stage,
        entered_at: None,
        left_at: ts(),
        dwell_sec: None,
        exit: StageExit::Pass,
        next_stage: None,
        event: "label.transition".to_string(),
        observed_at: ts() + Duration::seconds(30),
        resolution_sec: None,
        open_estimates: 0,
        estimate_ids: vec![],
        loom: provenance(),
    })
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
    for record in [pr(7, PrResolution::Merged), stage(9, Stage::ReviewWait)] {
        let (fact_a, rec_a) = ids("host-a", &record, 0);
        let (fact_b, rec_b) = ids("host-b", &record, 90);
        assert_eq!(fact_a.len(), 16);
        assert_eq!(fact_a, fact_b, "same natural key, same fact id");
        assert_ne!(rec_a, rec_b, "record_id still distinguishes deliveries");
    }
}

#[test]
fn fact_id_differs_across_natural_keys() {
    let all = [
        pr(7, PrResolution::Merged),
        pr(7, PrResolution::Closed),
        pr(8, PrResolution::Merged),
        stage(9, Stage::ReviewWait),
        stage(9, Stage::Doctor),
        stage(10, Stage::ReviewWait),
    ]
    .map(|r| ids("h", &r, 0).0);
    for (i, x) in all.iter().enumerate() {
        for y in &all[i + 1..] {
            assert_ne!(x, y);
        }
    }
}
