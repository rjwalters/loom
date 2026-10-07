use super::*;
use loom_daemon::eta::planner_sim::SideRow;
use loom_daemon::eta::NoEstimateReason;
use loom_daemon::types::PlanState;
use serde_json::json;

fn at() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-10-07T12:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
}

#[test]
fn a_queue_json_document_is_a_roster() {
    let doc = json!({
        "freshness": "fresh",
        "tick_at": "2026-10-07T11:59:30Z",
        "plan": {
            "slots": {"max_concurrent": 4, "occupancy": 4, "free": 0,
                      "max_admissions_per_tick": 2},
            "tick_interval_secs": 60,
            "complete": true
        },
        "queue": [
            {"rank": 1, "repo": "/no/such/workspace/loom", "issue": 7,
             "workspace_priority": 100, "urgent": false,
             "disposition": "deferred_capacity",
             "position": 1, "plan_state": "next", "gate": "capacity"}
        ]
    });
    let roster = roster_from_queue_json(&doc, |r| format!("slug:{r}")).unwrap();
    assert_eq!(roster.context.slots.max_concurrent, 4);
    assert_eq!(roster.context.tick_interval_secs, Some(60));
    assert_eq!(roster.rows.len(), 1);
    let row = &roster.rows[0];
    assert_eq!((row.repo.as_str(), row.issue), ("slug:/no/such/workspace/loom", 7));
    assert_eq!((row.plan.position, row.plan.plan_state), (Some(1), PlanState::Next));
    // The default resolver keeps a path that is not a directory as given.
    assert_eq!(slug_resolver()("/no/such/workspace/loom"), "/no/such/workspace/loom");
}

#[test]
fn a_roster_without_a_plan_is_refused() {
    let doc = json!({"tick_at": null, "plan": null, "queue": []});
    let err = roster_from_queue_json(&doc, str::to_string).unwrap_err();
    assert!(err.to_string().contains("no dispatch plan"), "{err}");
}

fn eta(p50: Option<i64>) -> EtaCell {
    EtaCell {
        p50_sec: p50,
        p25_sec: p50,
        p75_sec: p50,
        no_estimate_reason: p50.is_none().then_some(NoEstimateReason::NoDispatchPlan),
    }
}

fn side(position: Option<u32>, start: Option<i64>, land: Option<i64>) -> SideRow {
    SideRow {
        plan_state: PlanState::Queued,
        position,
        turnovers: None,
        start: eta(start),
        land: eta(land),
    }
}

#[test]
fn the_table_labels_both_columns_and_shows_the_deltas() {
    let p = Preview {
        as_of: at(),
        plan_at: at(),
        before_version: "0.19.866+aaaaaaaaaaaa".to_string(),
        after_version: "0.19.866+bbbbbbbbbbbb".to_string(),
        simulated: vec!["maxConcurrent".to_string()],
        unsimulated: vec!["workFinder.maxConcurrentPerRepo".to_string()],
        start_heuristic: "start-v1".to_string(),
        land_heuristic: "land-v1".to_string(),
        rows: vec![
            PreviewRow {
                repo: "rjwalters/loom".to_string(),
                issue: 42,
                before: side(Some(3), Some(5400), Some(30_000)),
                after: side(Some(3), Some(1200), Some(25_800)),
            },
            PreviewRow {
                repo: "rjwalters/loom".to_string(),
                issue: 43,
                before: side(None, None, None),
                after: side(None, None, None),
            },
        ],
    };
    let text = render(&p, "proposed.json");
    assert!(text.contains("before: 0.19.866+aaaaaaaaaaaa (current config)"), "{text}");
    assert!(text.contains("after:  0.19.866+bbbbbbbbbbbb (proposed.json)"), "{text}");
    assert!(text.contains("simulated: maxConcurrent"), "{text}");
    assert!(
        text.contains("NOT simulated (changed, shown unchanged): workFinder.maxConcurrentPerRepo")
    );
    let row = text.lines().find(|l| l.contains("#42")).unwrap();
    for want in ["#3", "1h30m", "20m", "-1h10m", "8h20m", "7h10m"] {
        assert!(row.contains(want), "{want:?} missing from {row:?}");
    }
    let refused = text.lines().find(|l| l.contains("#43")).unwrap();
    assert!(refused.contains("no_dispatch_plan"), "{refused}");
}

#[test]
fn durations_render_compactly() {
    assert_eq!(duration(45), "45s");
    assert_eq!(duration(720), "12m");
    assert_eq!(duration(3900), "1h05m");
    assert_eq!(delta(&eta(Some(60)), &eta(Some(60))), "=");
    assert_eq!(delta(&eta(Some(60)), &eta(Some(180))), "+2m");
}
