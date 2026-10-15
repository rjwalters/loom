//! The Doctor queue facts on `fleet.state` `doctor` rows (Issue #12085):
//! `doctor_gate` and the carried-forward `treating_since`.

use std::collections::BTreeMap;

use chrono::{DateTime, Duration, TimeZone, Utc};

use super::{build_view, FleetInput, FleetView, ListedPr, RepoListing};
use crate::telemetry::kinds::fleet_state::{DoctorGate, FleetStage};

const REPO: &str = "rjwalters/anvil";

fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 15, 12, 0, 0).unwrap()
}

/// One repo with PR #50 (linking issue 5) carrying `labels`.
fn input(labels: &[&str]) -> FleetInput {
    FleetInput {
        champion: BTreeMap::new(),
        curator: BTreeMap::new(),
        host_id: "robb-studio".to_string(),
        managed: [REPO.to_string()].into_iter().collect(),
        held: vec![],
        listings: vec![RepoListing {
            repo: REPO.to_string(),
            prs: vec![ListedPr {
                number: 50,
                labels: labels.iter().map(ToString::to_string).collect(),
                issue: Some(5),
            }],
        }],
        listed_at: Some(t0()),
        ready: None,
        slots: None,
        capacity: None,
        main_ci: BTreeMap::new(),
    }
}

fn row(v: &FleetView) -> crate::telemetry::kinds::fleet_state::FleetStateRow {
    v.repos[REPO].rows[&5].clone()
}

#[test]
fn gate_splits_parked_treating_and_drainable() {
    let now = t0() + Duration::minutes(1);
    let gate = |labels: &[&str]| row(&build_view(&input(labels), None, now)).doctor_gate;
    assert_eq!(gate(&["loom:changes-requested"]), Some(DoctorGate::Drainable));
    assert_eq!(gate(&["loom:changes-requested", "loom:treating"]), Some(DoctorGate::Treating));
    assert_eq!(gate(&["loom:changes-requested", "loom:blocked"]), Some(DoctorGate::Parked));
    // A hold beats a live claim.
    assert_eq!(
        gate(&["loom:changes-requested", "loom:treating", "loom:operator"]),
        Some(DoctorGate::Parked)
    );
}

#[test]
fn non_doctor_rows_carry_no_gate() {
    let v = build_view(&input(&["loom:review-requested"]), None, t0());
    assert_eq!(row(&v).stage, FleetStage::ReviewWait);
    assert_eq!(row(&v).doctor_gate, None);
    assert_eq!(row(&v).treating_since, None);
}

#[test]
fn treating_since_keeps_first_sight_and_clears_when_claim_ends() {
    let (t1, t2, t3) = (
        t0() + Duration::minutes(1),
        t0() + Duration::minutes(2),
        t0() + Duration::minutes(3),
    );
    let claimed = ["loom:changes-requested", "loom:treating"];
    let v1 = build_view(&input(&claimed), None, t1);
    assert_eq!(row(&v1).treating_since, Some(t1));
    let v2 = build_view(&input(&claimed), Some(&v1), t2);
    assert_eq!(row(&v2).treating_since, Some(t1));
    let v3 = build_view(&input(&["loom:changes-requested"]), Some(&v2), t3);
    assert_eq!(row(&v3).doctor_gate, Some(DoctorGate::Drainable));
    assert_eq!(row(&v3).treating_since, None);
}
