//! The dispatch-blocked facts on `fleet.state` ready rows (Issue #11960):
//! `dispatch_blocked_reason` and the carried-forward `dispatch_blocked_since`.

use std::collections::BTreeMap;

use chrono::{DateTime, Duration, TimeZone, Utc};

use super::{build_view, FleetInput, FleetView, ReadyItem, ReadyQueue};
use crate::telemetry::kinds::pick_decision::PickSkipReason as R;

const REPO: &str = "rjwalters/anvil";

fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 14, 12, 0, 0).unwrap()
}

/// One managed repo with one ready item, skipped for `reason` (if any).
fn input(reason: Option<R>) -> FleetInput {
    let repos = || [REPO.to_string()].into_iter().collect();
    FleetInput {
        champion: BTreeMap::new(),
        curator: BTreeMap::new(),
        host_id: "robb-studio".to_string(),
        managed: repos(),
        held: vec![],
        listings: vec![],
        listed_at: Some(t0() - Duration::minutes(1)),
        ready: Some(ReadyQueue {
            items: vec![ReadyItem {
                repo: REPO.to_string(),
                issue: 5,
                rank: 1,
                level: 1,
                weight: 100,
                created_at: Some(t0() - Duration::days(3)),
                main_red_fix: false,
                eligible_reason: None,
                eligible_at: None,
                dispatch_blocked_reason: reason,
            }],
            listed: repos(),
            complete: repos(),
            slots: None,
            halts: BTreeMap::new(),
            limits: Default::default(),
        }),
        slots: None,
        capacity: None,
        main_ci: BTreeMap::new(),
    }
}

#[test]
fn dispatch_blocked_since_keeps_first_skip_and_resets_on_change() {
    let row = |v: &FleetView| v.repos[REPO].rows[&5].clone();
    let (t1, t2, t3, t4) = (
        t0() + Duration::minutes(1),
        t0() + Duration::minutes(2),
        t0() + Duration::minutes(3),
        t0() + Duration::minutes(4),
    );
    let v1 = build_view(&input(Some(R::Cap)), None, t1);
    assert_eq!(row(&v1).dispatch_blocked_reason, Some(R::Cap));
    assert_eq!(row(&v1).dispatch_blocked_since, Some(t1));
    // Same reason: the first-skip instant is kept.
    let v2 = build_view(&input(Some(R::Cap)), Some(&v1), t2);
    assert_eq!(row(&v2).dispatch_blocked_since, Some(t1));
    // New reason: restarts.
    let v3 = build_view(&input(Some(R::Backoff)), Some(&v2), t3);
    assert_eq!(row(&v3).dispatch_blocked_since, Some(t3));
    // No longer skipped: cleared.
    let v4 = build_view(&input(None), Some(&v3), t4);
    assert_eq!(row(&v4).dispatch_blocked_reason, None);
    assert_eq!(row(&v4).dispatch_blocked_since, None);
}
