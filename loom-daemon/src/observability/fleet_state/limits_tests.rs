//! The per-repo dispatch limits on `fleet.state` (Issue #11606): `in_flight`,
//! `cap` (null = uncapped, absent = unknown) and `deferred_repo_cap`.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Duration, TimeZone, Utc};

use super::sources::limits_by_slug;
use super::{build_view, decide, Emitted, FleetInput, ReadyItem, ReadyQueue, RepoLimits};
use crate::telemetry::kinds::fleet_state::{
    split_into_chunks, FleetStateRecord, FleetStateRepo, PlannerStamps,
};
use crate::types::RepoLimitView;

const FULL: &str = "acme/full";
const FREE: &str = "acme/free";
const IDLE: &str = "acme/idle";

fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 12, 9, 0, 0).unwrap()
}

fn limits(in_flight: u32, cap: Option<u32>, deferred_repo_cap: u32) -> RepoLimits {
    RepoLimits {
        in_flight,
        cap,
        deferred_repo_cap,
    }
}

fn input(items: Vec<ReadyItem>, limits: &[(&str, RepoLimits)]) -> FleetInput {
    let repos: BTreeSet<String> = limits
        .iter()
        .map(|(r, _)| (*r).to_string())
        .chain(items.iter().map(|i| i.repo.clone()))
        .collect();
    FleetInput {
        host_id: "robb-studio".to_string(),
        managed: repos.clone(),
        ready: Some(ReadyQueue {
            items,
            listed: repos.clone(),
            complete: repos,
            limits: limits.iter().map(|(r, l)| ((*r).to_string(), *l)).collect(),
            ..ReadyQueue::default()
        }),
        ..FleetInput::default()
    }
}

fn item(repo: &str, issue: u32) -> ReadyItem {
    ReadyItem {
        repo: repo.to_string(),
        issue,
        rank: issue,
        level: 0,
        fleet_priority: 200,
        created_at: None,
        main_red_fix: false,
        eligible_reason: None,
        eligible_at: None,
    }
}

fn entry<'a>(record: &'a FleetStateRecord, repo: &str) -> &'a FleetStateRepo {
    record.repos.iter().find(|r| r.repo == repo).unwrap()
}

fn emitted(view: super::FleetView, record: &FleetStateRecord) -> Emitted {
    Emitted {
        view,
        stamps: PlannerStamps::default(),
        as_of: record.as_of,
        anchor_as_of: record.anchor_as_of,
    }
}

fn two_repo_input() -> FleetInput {
    input(
        vec![item(FULL, 1), item(FULL, 2)],
        &[
            (FULL, limits(2, Some(2), 2)),
            (FREE, limits(1, Some(2), 0)),
            (IDLE, limits(0, Some(2), 0)),
        ],
    )
}

/// The anchor names every managed repo, rows or not; a repo at its cap shows
/// `in_flight == cap` and its deferrals; zero deferrals is sent as zero.
#[test]
fn the_anchor_carries_limits_for_every_repo_including_rowless_ones() {
    let view = build_view(&two_repo_input(), None, t0());
    let anchor = decide(&view, &PlannerStamps::default(), None, t0()).unwrap();
    let full = entry(&anchor, FULL);
    assert_eq!(
        (full.in_flight, full.cap, full.deferred_repo_cap),
        (Some(2), Some(Some(2)), Some(2))
    );
    let free = entry(&anchor, FREE);
    assert_eq!(
        (free.in_flight, free.cap, free.deferred_repo_cap),
        (Some(1), Some(Some(2)), Some(0))
    );
    let idle = entry(&anchor, IDLE);
    assert!(idle.rows.is_empty());
    assert_eq!(idle.in_flight, Some(0));
}

/// Uncapped is `cap: null` on the wire; an unread repo omits all three.
#[test]
fn uncapped_is_null_and_unknown_is_absent_on_the_wire() {
    let input = input(vec![item("acme/unread", 9)], &[(FREE, limits(3, None, 0))]);
    let view = build_view(&input, None, t0());
    let anchor = decide(&view, &PlannerStamps::default(), None, t0()).unwrap();
    let wire = serde_json::to_value(&anchor).unwrap();
    let repo = |name: &str| {
        wire["repos"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["repo"] == name)
            .unwrap()
            .clone()
    };
    let free = repo(FREE);
    assert!(free["cap"].is_null() && free.get("cap").is_some(), "{free}");
    assert_eq!(free["in_flight"], 3);
    assert_eq!(free["deferred_repo_cap"], 0);
    let unread = repo("acme/unread");
    for key in ["in_flight", "cap", "deferred_repo_cap"] {
        assert!(unread.get(key).is_none(), "{key} must be absent: {unread}");
    }
    // And back: null decodes as observed-uncapped, absent as unknown.
    let back: FleetStateRecord = serde_json::from_value(wire).unwrap();
    assert_eq!(entry(&back, FREE).cap, Some(None));
    assert_eq!(entry(&back, "acme/unread").cap, None);
}

/// A record from an older emitter decodes with the fields absent, not zero.
#[test]
fn an_old_record_decodes_with_the_limits_absent() {
    let old: FleetStateRepo =
        serde_json::from_value(serde_json::json!({"repo": "acme/old"})).unwrap();
    assert_eq!((old.in_flight, old.cap, old.deferred_repo_cap), (None, None, None));
    let capped: FleetStateRepo =
        serde_json::from_value(serde_json::json!({"repo": "a", "cap": 3})).unwrap();
    assert_eq!(capped.cap, Some(Some(3)));
}

/// A slot-only change (same rows) is a repo delta; an unchanged tick is not.
#[test]
fn a_slot_only_change_produces_a_repo_delta_with_no_rows() {
    let first = build_view(&two_repo_input(), None, t0());
    let anchor = decide(&first, &PlannerStamps::default(), None, t0()).unwrap();
    let last = emitted(first.clone(), &anchor);
    let t1 = t0() + Duration::minutes(1);

    let same = build_view(&two_repo_input(), Some(&first), t1);
    assert!(decide(&same, &PlannerStamps::default(), Some(&last), t1).is_none());

    // acme/free admits one: in_flight 1 -> 2; its deferral count is unchanged.
    let mut moved = two_repo_input();
    moved
        .ready
        .as_mut()
        .unwrap()
        .limits
        .insert(FREE.to_string(), limits(2, Some(2), 0));
    let view = build_view(&moved, Some(&first), t1);
    let delta = decide(&view, &PlannerStamps::default(), Some(&last), t1).unwrap();
    assert!(!delta.anchor);
    assert_eq!(delta.repos.len(), 1);
    let free = entry(&delta, FREE);
    assert!(free.rows.is_empty() && free.removed.is_empty());
    assert_eq!(free.in_flight, Some(2));

    // A changed cap alone, and a deferral count alone, are deltas too.
    for changed in [
        limits(1, Some(3), 0),
        limits(1, None, 0),
        limits(1, Some(2), 4),
    ] {
        let mut input = two_repo_input();
        input
            .ready
            .as_mut()
            .unwrap()
            .limits
            .insert(FREE.to_string(), changed);
        let view = build_view(&input, Some(&first), t1);
        let delta = decide(&view, &PlannerStamps::default(), Some(&last), t1).unwrap();
        assert_eq!(entry(&delta, FREE).cap, Some(changed.cap));
        assert_eq!(entry(&delta, FREE).deferred_repo_cap, Some(changed.deferred_repo_cap));
    }
}

/// A pass whose tick no longer covers a repo sends it with the limits absent
/// (unknown), never as zero.
#[test]
fn a_repo_the_tick_no_longer_covers_goes_unknown_not_zero() {
    let first = build_view(&two_repo_input(), None, t0());
    let anchor = decide(&first, &PlannerStamps::default(), None, t0()).unwrap();
    let last = emitted(first.clone(), &anchor);
    let t1 = t0() + Duration::minutes(1);
    let mut input = two_repo_input();
    input.ready.as_mut().unwrap().limits.remove(IDLE);
    let view = build_view(&input, Some(&first), t1);
    let delta = decide(&view, &PlannerStamps::default(), Some(&last), t1).unwrap();
    let idle = entry(&delta, IDLE);
    assert_eq!((idle.in_flight, idle.cap, idle.deferred_repo_cap), (None, None, None));
}

/// Chunking never drops the limits, and a repo split across chunks repeats
/// them in each; nothing is capped or truncated.
#[test]
fn chunks_keep_the_limits_of_a_split_repo() {
    let items: Vec<ReadyItem> = (1..=300).map(|n| item(FULL, n)).collect();
    let view = build_view(&input(items, &[(FULL, limits(2, Some(2), 7))]), None, t0());
    let anchor = decide(&view, &PlannerStamps::default(), None, t0()).unwrap();
    let chunks = split_into_chunks(anchor, 6_000);
    assert!(chunks.len() > 2);
    for chunk in &chunks {
        for repo in &chunk.repos {
            assert_eq!(repo.in_flight, Some(2));
            assert_eq!(repo.cap, Some(Some(2)));
            assert_eq!(repo.deferred_repo_cap, Some(7));
        }
    }
}

/// Every limit entry the tick holds is kept (no cap), a root with no slug is
/// left out, and counts convert to the wire type.
#[test]
fn limits_by_slug_keeps_every_resolvable_entry() {
    let views: Vec<RepoLimitView> = (0..50)
        .map(|i| RepoLimitView {
            repo: format!("/src/r{i}"),
            in_flight: i,
            cap: (i % 2 == 0).then_some(3),
            deferred_repo_cap: i / 2,
        })
        .chain([RepoLimitView {
            repo: "/src/unknown".to_string(),
            ..RepoLimitView::default()
        }])
        .collect();
    let slug = |root: &str| root.strip_prefix("/src/r").map(|n| format!("acme/r{n}"));
    let out: BTreeMap<String, RepoLimits> = limits_by_slug(&views, slug);
    assert_eq!(out.len(), 50);
    assert_eq!(out["acme/r4"], limits(4, Some(3), 2));
    assert_eq!(out["acme/r5"], limits(5, None, 2));
}
