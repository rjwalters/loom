//! The ETA facts on `fleet.state` (Issue #11564): `repos[].halt` and
//! `rows[].eligible_at` / `eligible_reason`, uncapped, with the delta rule
//! and a replay of the 2am `install_incompatible` hold of 10-09 → 10-12.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use chrono::{DateTime, Duration, TimeZone, Utc};

use super::sources::halts_by_slug;
use super::{build_view, decide, Emitted, FleetInput, FleetView, ReadyItem, ReadyQueue};
use crate::telemetry::kinds::fleet_state::{
    split_into_chunks, FleetEligibleReason, FleetHaltReason, FleetRepoHalt, FleetStateRecord,
    FleetStateRepo, PlannerStamps,
};
use crate::work_finder::halt_cause::since::HaltClock;
use crate::work_finder::halt_cause::HaltCause;

const TWO_AM: &str = "2amlogic/2am";

fn ready_item(
    repo: &str,
    issue: u32,
    eligible: Option<(FleetEligibleReason, DateTime<Utc>)>,
) -> ReadyItem {
    ReadyItem {
        repo: repo.to_string(),
        issue,
        rank: issue,
        level: 0,
        weight: 100,
        created_at: None,
        main_red_fix: false,
        eligible_reason: eligible.map(|(reason, _)| reason),
        eligible_at: eligible.map(|(_, at)| at),
        dispatch_blocked_reason: None,
    }
}

fn input(items: Vec<ReadyItem>, halts: BTreeMap<String, FleetRepoHalt>) -> FleetInput {
    let repos: BTreeSet<String> = items
        .iter()
        .map(|i| i.repo.clone())
        .chain(halts.keys().cloned())
        .collect();
    FleetInput {
        host_id: "aws-worker-1".to_string(),
        managed: repos.clone(),
        ready: Some(ReadyQueue {
            items,
            listed: repos.clone(),
            complete: repos,
            slots: None,
            halts,
            limits: Default::default(),
        }),
        ..FleetInput::default()
    }
}

/// Every chunk's repo entries, as a reader reassembles them.
fn entries(chunks: &[FleetStateRecord]) -> Vec<&FleetStateRepo> {
    chunks.iter().flat_map(|c| c.repos.iter()).collect()
}

/// No cap: 300 held rows and 80 halted repos all reach the wire, through
/// chunking (a small chunk budget forces many chunks), never truncation.
#[test]
fn every_held_row_and_every_halted_repo_is_emitted_without_a_cap() {
    let now = Utc.with_ymd_and_hms(2026, 10, 11, 12, 0, 0).unwrap();
    let until = now + Duration::minutes(15);
    let items: Vec<ReadyItem> = (1..=300)
        .map(|n| ready_item("acme/busy", n, Some((FleetEligibleReason::DispatchBackoff, until))))
        .collect();
    let halts: BTreeMap<String, FleetRepoHalt> = (0..80)
        .map(|i| {
            let halt = FleetRepoHalt {
                reason: FleetHaltReason::InstallIncompatible,
                since: now - Duration::hours(1),
                since_lower_bound: false,
            };
            (format!("acme/held-{i:02}"), halt)
        })
        .collect();
    let view = build_view(&input(items, halts), None, now);
    let anchor = decide(&view, &PlannerStamps::default(), None, now).unwrap();
    let chunks = split_into_chunks(anchor, 8_000);
    assert!(chunks.len() > 2, "the budget must force chunking: {}", chunks.len());

    let all = entries(&chunks);
    let held_rows: BTreeSet<u32> = all
        .iter()
        .flat_map(|r| &r.rows)
        .filter(|row| row.eligible_at == Some(until))
        .filter(|row| row.eligible_reason == Some(FleetEligibleReason::DispatchBackoff))
        .map(|row| row.issue)
        .collect();
    assert_eq!(held_rows, (1..=300).collect::<BTreeSet<u32>>());
    let halted: BTreeSet<&str> = all
        .iter()
        .filter(|r| r.halt.is_some())
        .map(|r| r.repo.as_str())
        .collect();
    assert_eq!(halted.len(), 80);
}

/// A halt starting, changing reason or clearing names the repo in a delta,
/// even when nothing else about the repo changed.
#[test]
fn a_halt_start_change_and_clear_each_name_the_repo_in_a_delta() {
    let t0 = Utc.with_ymd_and_hms(2026, 10, 11, 12, 0, 0).unwrap();
    let halt = |reason, since| FleetRepoHalt {
        reason,
        since,
        since_lower_bound: false,
    };
    let steps = [
        (t0, None),
        (
            t0 + Duration::minutes(1),
            Some(halt(FleetHaltReason::MainRed, t0 + Duration::minutes(1))),
        ),
        (
            t0 + Duration::minutes(2),
            Some(halt(FleetHaltReason::MainRed, t0 + Duration::minutes(1))),
        ),
        (
            t0 + Duration::minutes(3),
            Some(halt(FleetHaltReason::Drain, t0 + Duration::minutes(3))),
        ),
        (t0 + Duration::minutes(4), None),
    ];
    let mut prev: Option<FleetView> = None;
    let mut last: Option<Emitted> = None;
    let mut named = Vec::new();
    for (now, halt) in steps {
        let halts: BTreeMap<String, FleetRepoHalt> = halt
            .iter()
            .map(|h| ("acme/app".to_string(), h.clone()))
            .collect();
        let items = vec![ready_item("acme/app", 7, None)];
        let view = build_view(&input(items, halts), prev.as_ref(), now);
        let record = decide(&view, &PlannerStamps::default(), last.as_ref(), now);
        if let Some(record) = &record {
            let entry = record.repos.iter().find(|r| r.repo == "acme/app");
            named.push((record.anchor, entry.map(|e| e.halt.as_ref().map(|h| h.reason))));
            last = Some(Emitted {
                view: view.clone(),
                stamps: PlannerStamps::default(),
                as_of: now,
                anchor_as_of: record.anchor_as_of,
            });
        } else {
            named.push((false, None));
        }
        prev = Some(view);
    }
    assert_eq!(
        named,
        vec![
            (true, Some(None)),
            (false, Some(Some(FleetHaltReason::MainRed))),
            // Unchanged: no record at all.
            (false, None),
            (false, Some(Some(FleetHaltReason::Drain))),
            // Cleared: the repo is named with no halt.
            (false, Some(None)),
        ]
    );
}

/// Replay of 2am on the AWS workers: held `install_incompatible` from about
/// 2026-10-09 03:05Z to 2026-10-12 00:49Z. Driven tick by tick through the
/// work finder's hold clock and the emitter, every record that names the repo
/// reads `{install_incompatible, since: <first held tick>}` throughout, and
/// the first unheld tick clears it.
#[test]
fn replay_2am_install_incompatible_hold_reads_one_since_and_clears() {
    let root = PathBuf::from("/home/ubuntu/GitHub/2am");
    let slug = |r: &str| (r == "/home/ubuntu/GitHub/2am").then(|| TWO_AM.to_string());
    let start = Utc.with_ymd_and_hms(2026, 10, 9, 3, 0, 0).unwrap();
    let first_held = Utc.with_ymd_and_hms(2026, 10, 9, 3, 5, 0).unwrap();
    let cleared = Utc.with_ymd_and_hms(2026, 10, 12, 0, 49, 0).unwrap();
    let end = cleared + Duration::minutes(10);

    let mut clock = HaltClock::new();
    let mut prev: Option<FleetView> = None;
    let mut last: Option<Emitted> = None;
    let (mut held_records, mut anchors) = (0, 0);
    let mut cleared_seen = false;
    let mut now = start;
    while now <= end {
        let held = now >= first_held && now < cleared;
        let cause = held.then_some(HaltCause::InstallIncompatible);
        clock.observe([(&root, held, cause)], now);
        let halts = halts_by_slug(&clock.halts(), slug);
        // Its ready queue sits under the hold, every row skipped `halted`.
        let items = (1..=3)
            .map(|n| ready_item(TWO_AM, 4000 + n, None))
            .collect();
        let view = build_view(&input(items, halts), prev.as_ref(), now);
        if let Some(record) = decide(&view, &PlannerStamps::default(), last.as_ref(), now) {
            anchors += usize::from(record.anchor);
            let anchor_as_of = record.anchor_as_of;
            for chunk in split_into_chunks(record, super::CHUNK_BYTES) {
                for entry in chunk.repos.iter().filter(|r| r.repo == TWO_AM) {
                    if held {
                        let halt = entry
                            .halt
                            .as_ref()
                            .expect("a held tick's record carries it");
                        assert_eq!(halt.reason, FleetHaltReason::InstallIncompatible);
                        assert_eq!(halt.reason.as_str(), "install_incompatible");
                        assert_eq!(halt.since, first_held, "at {now}");
                        assert!(!halt.since_lower_bound, "the hold began after start");
                        held_records += 1;
                    } else {
                        assert_eq!(entry.halt, None, "at {now}");
                        cleared_seen |= now == cleared;
                    }
                }
            }
            last = Some(Emitted {
                view: view.clone(),
                stamps: PlannerStamps::default(),
                as_of: now,
                anchor_as_of,
            });
        }
        prev = Some(view);
        now += Duration::minutes(1);
    }
    // Every 5-minute anchor across the ~70 h hold named it.
    assert!(held_records >= 830, "{held_records}");
    assert!(anchors >= 835, "{anchors}");
    assert!(cleared_seen, "the first unheld tick names the repo with no halt");
}

/// The same hold, already in force when the process started, is a lower
/// bound.
#[test]
fn a_hold_in_force_at_process_start_is_a_lower_bound_on_the_wire() {
    let root = PathBuf::from("/w/2am");
    let now = Utc.with_ymd_and_hms(2026, 10, 10, 9, 0, 0).unwrap();
    let mut clock = HaltClock::new();
    clock.observe([(&root, true, Some(HaltCause::InstallIncompatible))], now);
    let halts = halts_by_slug(&clock.halts(), |_| Some(TWO_AM.to_string()));
    let view = build_view(&input(Vec::new(), halts), None, now);
    let anchor = decide(&view, &PlannerStamps::default(), None, now).unwrap();
    let wire = serde_json::to_value(&anchor).unwrap();
    let halt = &wire["repos"][0]["halt"];
    assert_eq!(halt["reason"], "install_incompatible");
    assert_eq!(halt["since"], "2026-10-10T09:00:00Z");
    assert_eq!(halt["since_lower_bound"], true);
}

/// A hold with no named cause is exported as `unknown`, never dropped.
#[test]
fn a_causeless_hold_is_exported_as_unknown() {
    let root = PathBuf::from("/w/app");
    let now = Utc.with_ymd_and_hms(2026, 10, 10, 9, 0, 0).unwrap();
    let mut clock = HaltClock::new();
    clock.observe([(&root, true, None)], now);
    let halts = halts_by_slug(&clock.halts(), |_| Some("acme/app".to_string()));
    assert_eq!(halts["acme/app"].reason, FleetHaltReason::Unknown);
    assert_eq!(serde_json::to_value(&halts["acme/app"]).unwrap()["reason"], "unknown");
}
