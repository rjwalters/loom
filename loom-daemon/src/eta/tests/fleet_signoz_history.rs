//! SigNoz as the primary fleet-refresh history source, forge as gap-fill
//! (#10520).
//!
//! Pure: no network, no clock. SigNoz is [`FileRows`] (over the recorded
//! fixture, or rows built here in exactly the `TIMELINE_SQL` shape); the
//! forge is [`Counting`], a fake that serves listings and timelines and
//! counts every `get`.
//!
//! # The polling tolerance (parity)
//!
//! A SigNoz event is dated by its producer: a webhook event by the loom-ui
//! Worker's receipt (within [`MATCH_SLACK_SEC`] of the forge's time), a
//! daemon-only event by the daemon's listing diff on the collector's
//! 5-minute pass (within [`DAEMON_POLL_TOLERANCE_SEC`]). Webhook events are
//! never polled, so the poll slack applies to daemon-sourced events only.

use crate::eta::fleet::{self, FleetSnapshot};
use crate::eta::fleet_fetch::{history, ForgeRead, ListedPr, Read, Reader, RepoTarget, PER_PAGE};
use crate::eta::fleet_refresh::{
    read_state, run_cycle, run_cycle_with, staging_path, state_path, Budgets, PassKind, StopReason,
};
use crate::eta::fleet_signoz_history::{
    gap_fill_history, item_complete, pr_history, HistorySource, DAEMON_POLL_TOLERANCE_SEC,
    GAP_FILL_COUNTER,
};
use crate::eta::fleet_signoz_refresh::{FileRows, Limits, PageQuery, ReadError, SignozRead};
use crate::eta::fleet_signoz_timeline::{Timeline, MATCH_SLACK_SEC};
use crate::eta::fleet_signoz_timeline_rows::{walk, ItemKey, Source, Target, Transition};
use crate::forge_call_stats::{counters, ForgeOp};
use crate::pr_latency::timeline::parse_timeline;
use crate::pr_latency::{PrEvent, PrHistory, PrState};
use chrono::{DateTime, Duration, TimeZone, Utc};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::Path;

const A: &str = "acme/alpha";
const RR: &str = "loom:review-requested";
const APPROVED: &str = "loom:pr";
const CR: &str = "loom:changes-requested";

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 4, 12, 0, 0).unwrap()
}

fn limits() -> Limits {
    Limits {
        page_size: 500,
        max_pages: 50,
    }
}

fn budgets(gap_fill: u64) -> Budgets {
    Budgets {
        refresh: 300,
        backfill: 1500,
        reserve: 1500,
        backfill_days: 21,
        gap_fill,
    }
}

fn target(root: &Path) -> RepoTarget {
    RepoTarget {
        repo: A.to_string(),
        host: Some("github.com".to_string()),
        cwd: root.to_path_buf(),
        reader: Ok(Reader {
            app_id: "1".to_string(),
            dir: root.join("reader-1"),
        }),
    }
}

// -- one source of truth, rendered for both backends -------------------------

/// One PR as the forge knows it.
#[derive(Clone)]
struct Truth {
    number: u32,
    opened: DateTime<Utc>,
    /// `(added, label, at)`, in order.
    labels: Vec<(bool, &'static str, DateTime<Utc>)>,
    merged: Option<DateTime<Utc>>,
    /// A push after the rejection (forge-only: SigNoz records no push).
    pushed: Option<DateTime<Utc>>,
}

/// Requested, approved and merged in the three hours after `opened`.
fn approved(number: u32, opened: DateTime<Utc>) -> Truth {
    Truth {
        number,
        opened,
        labels: vec![
            (true, RR, opened),
            (false, RR, opened + Duration::hours(1)),
            (true, APPROVED, opened + Duration::hours(1)),
        ],
        merged: Some(opened + Duration::hours(2)),
        pushed: None,
    }
}

/// Requested, rejected, pushed, re-requested: needs the forge's push.
fn rejected(number: u32, opened: DateTime<Utc>) -> Truth {
    Truth {
        number,
        opened,
        labels: vec![
            (true, RR, opened),
            (false, RR, opened + Duration::minutes(30)),
            (true, CR, opened + Duration::minutes(30)),
            (false, CR, opened + Duration::minutes(90)),
            (true, RR, opened + Duration::minutes(90)),
        ],
        merged: None,
        pushed: Some(opened + Duration::minutes(80)),
    }
}

fn ns(at: DateTime<Utc>) -> String {
    at.timestamp_nanos_opt().unwrap().to_string()
}

/// One loom-ui webhook export row, as `TIMELINE_SQL` returns it: the D1
/// record as a flat body (the live shape, #10671).
fn webhook(
    id: &str,
    target: &str,
    number: u32,
    action: &str,
    label: Option<&str>,
    at: DateTime<Utc>,
    merged: bool,
) -> String {
    let mut payload = json!({
        "kind": "label.transition",
        "at": at.to_rfc3339(),
        "repo": A,
        "target": target,
        "number": number,
        "action": action,
        "labels_after": [],
    });
    if let Some(label) = label {
        payload["label"] = json!(label);
    }
    if merged {
        payload["merged"] = json!(true);
    }
    json!({
        "record_id": id,
        "kind": "label.transition",
        "service": "loom-ui-d1-export",
        "repo": A,
        "attrs": "{}",
        "nums": "{}",
        "body": payload.to_string(),
        "event_time_ns": ns(at),
        "knowable_time_ns": ns(at),
    })
    .to_string()
}

/// One daemon stage-journal row (`label.first_seen`, as `TIMELINE_SQL`
/// returns it): PR `number`'s whole label set, observed at `seen`.
/// Synthetic: no producer exports these to SigNoz yet (#10756).
fn label_set_row(id: &str, number: u32, labels: &[&str], seen: DateTime<Utc>) -> String {
    let body = json!({
        "schema": "eta-stage-sample/v1",
        "observed_at": seen.to_rfc3339(),
        "event": "label.first_seen",
        "repo": A,
        "issue": 1,
        "pr_number": number,
        "stage": null,
        "resolution_sec": 300,
        "in_sweep": false,
        "raw": {"labels": labels},
    });
    json!({
        "record_id": id,
        "kind": "label.first_seen",
        "service": "loom",
        "repo": A,
        "attrs": json!({"loom.repo": A}).to_string(),
        "nums": "{}",
        "body": body.to_string(),
        "event_time_ns": ns(seen),
        "knowable_time_ns": ns(seen),
    })
    .to_string()
}

/// The SigNoz rows for `prs`, plus (when `anchored`) one issue opened and
/// labelled 25 days back, so both families cover the 21-day window.
fn signoz_rows(prs: &[Truth], anchored: bool) -> FileRows {
    FileRows::parse(&signoz_lines(prs, anchored).join("\n")).unwrap()
}

/// [`signoz_rows`]' export lines.
fn signoz_lines(prs: &[Truth], anchored: bool) -> Vec<String> {
    let mut lines = Vec::new();
    if anchored {
        let old = now() - Duration::days(25);
        lines.push(webhook("anchor-o", "issue", 1, "opened", None, old, false));
        lines.push(webhook("anchor-l", "issue", 1, "labeled", Some("loom:issue"), old, false));
    }
    for pr in prs {
        let n = pr.number;
        lines.push(webhook(&format!("{n}-o"), "pr", n, "opened", None, pr.opened, false));
        for (i, (added, label, at)) in pr.labels.iter().enumerate() {
            let action = if *added { "labeled" } else { "unlabeled" };
            lines.push(webhook(&format!("{n}-l{i}"), "pr", n, action, Some(label), *at, false));
        }
        if let Some(at) = pr.merged {
            lines.push(webhook(&format!("{n}-m"), "pr", n, "closed", None, at, true));
        }
    }
    lines
}

/// The REST timeline `pr`'s forge read returns.
fn forge_timeline(pr: &Truth) -> Vec<Value> {
    let mut entries: Vec<Value> = pr
        .labels
        .iter()
        .map(|(added, label, at)| {
            json!({"event": if *added { "labeled" } else { "unlabeled" },
                   "created_at": at.to_rfc3339(), "label": {"name": label}})
        })
        .collect();
    if let Some(at) = pr.pushed {
        entries.push(json!({"event": "head_ref_force_pushed", "created_at": at.to_rfc3339()}));
    }
    if let Some(at) = pr.merged {
        entries.push(json!({"event": "merged", "created_at": at.to_rfc3339()}));
    }
    entries
}

/// A forge fake over `prs` that counts every `get`.
#[derive(Default)]
struct Counting {
    prs: Vec<Truth>,
    gets: Vec<String>,
}

impl Counting {
    fn with(prs: &[Truth]) -> Self {
        let mut prs = prs.to_vec();
        prs.sort_by_key(|p| std::cmp::Reverse(last_activity(p)));
        Counting {
            prs,
            gets: Vec::new(),
        }
    }

    fn timeline_reads(&self, number: u32) -> usize {
        let needle = format!("repos/{A}/issues/{number}/timeline");
        self.gets.iter().filter(|u| u.starts_with(&needle)).count()
    }
}

/// The labels `pr` carries after all its changes: what the forge listing
/// reports.
fn final_labels(pr: &Truth) -> std::collections::BTreeSet<&'static str> {
    let mut labels = std::collections::BTreeSet::new();
    for (added, label, _) in &pr.labels {
        if *added {
            labels.insert(*label);
        } else {
            labels.remove(label);
        }
    }
    labels
}

fn last_activity(pr: &Truth) -> DateTime<Utc> {
    let labels = pr.labels.iter().map(|(_, _, at)| *at);
    labels
        .chain(pr.merged)
        .chain(pr.pushed)
        .chain([pr.opened])
        .max()
        .unwrap()
}

impl ForgeRead for Counting {
    fn breaker_open(&self) -> bool {
        false
    }

    fn get(&mut self, _: &RepoTarget, _: &Reader, url: &str, _: Option<&str>, _: ForgeOp) -> Read {
        self.gets.push(url.to_string());
        let ok = |body: String| Read::Ok {
            status: 200,
            etag: Some("W/\"x\"".to_string()),
            body,
            remaining: Some(4000),
        };
        let page: usize = url.rsplit("page=").next().unwrap().parse().unwrap();
        if let Some(rest) = url
            .split("/issues/")
            .nth(1)
            .filter(|r| r.contains("/timeline?"))
        {
            let number: u32 = rest.split('/').next().unwrap().parse().unwrap();
            let entries = self
                .prs
                .iter()
                .find(|p| p.number == number)
                .map(forge_timeline)
                .unwrap_or_default();
            let slice: Vec<Value> = entries
                .into_iter()
                .skip((page - 1) * PER_PAGE)
                .take(PER_PAGE)
                .collect();
            return ok(serde_json::to_string(&slice).unwrap());
        }
        let rows: Vec<Value> = self
            .prs
            .iter()
            .skip((page - 1) * PER_PAGE)
            .take(PER_PAGE)
            .map(|p| {
                json!({"number": p.number,
                       "state": if p.merged.is_some() { "closed" } else { "open" },
                       "created_at": p.opened.to_rfc3339(),
                       "updated_at": last_activity(p).to_rfc3339(),
                       "labels": final_labels(p).iter().map(|l| json!({"name": l}))
                           .collect::<Vec<_>>(),
                       "pull_request": {"merged_at": p.merged.map(|m| m.to_rfc3339())}})
            })
            .collect();
        ok(serde_json::to_string(&rows).unwrap())
    }
}

/// A SigNoz that never answers.
struct Down;

impl SignozRead for Down {
    fn page(&mut self, _: &PageQuery) -> Result<String, ReadError> {
        Err(ReadError::Unavailable("connection refused".to_string()))
    }
}

fn published(root: &Path) -> FleetSnapshot {
    fleet::read(&fleet::snapshot_path(root, A)).expect("published")
}

// -- zero forge ---------------------------------------------------------------

/// The acceptance case: a pass over a window SigNoz fully covers makes no
/// forge request at all — no listing, no timeline — on the counting fake
/// and on the `forge_call_stats` counter, and reports `0` gap-fill calls.
#[test]
fn a_fully_covered_window_makes_zero_forge_requests() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let prs = [
        approved(101, now() - Duration::hours(10)),
        approved(102, now() - Duration::hours(5)),
    ];
    let mut signoz = signoz_rows(&prs, true);
    let mut forge = Counting::with(&prs);
    let before = counters::get(GAP_FILL_COUNTER);

    let report = run_cycle_with(
        root,
        &[target(root)],
        &mut forge,
        Some((&mut signoz, limits())),
        budgets(100),
        now(),
    );

    let r = &report.repos[0];
    assert!(forge.gets.is_empty(), "no forge request: {:?}", forge.gets);
    assert_eq!(counters::get(GAP_FILL_COUNTER), before, "the facade counter did not move");
    assert_eq!(r.forge_calls, 0);
    assert_eq!(r.gap_fill_calls, Some(0));
    assert_eq!(r.history, Some(HistorySource::Signoz));
    assert_eq!(
        (r.stop, r.pass, r.promoted),
        (StopReason::Complete, Some(PassKind::Backfill), true)
    );
    assert_eq!(r.prs_read, 2);
    assert_eq!(report.backfill_calls, 0);
    let snapshot = published(root);
    assert_eq!(snapshot.prs, vec![101, 102]);
    assert_eq!(snapshot.as_of, now());
    let state = read_state(&state_path(root, A)).unwrap();
    assert!(state.pass.is_none());
    let note = state.history.expect("the doctor's fact is persisted");
    assert_eq!((note.source, note.gap_fill_calls), (HistorySource::Signoz, 0));

    // The next cycle is a refresh: still no call — the listing `304`
    // shortcut is skipped, not merely made cheap.
    let later = now() + Duration::hours(1);
    let report = run_cycle_with(
        root,
        &[target(root)],
        &mut forge,
        Some((&mut signoz, limits())),
        budgets(100),
        later,
    );
    let r = &report.repos[0];
    assert_eq!(
        (r.pass, r.stop, r.forge_calls),
        (Some(PassKind::Refresh), StopReason::Complete, 0)
    );
    assert!(forge.gets.is_empty());
    assert_eq!(published(root).as_of, later);
}

/// What the fit trains on is the same whichever backend answered: on a
/// covered window, the SigNoz-built snapshot equals the forge-built one.
#[test]
fn a_covered_window_publishes_the_snapshot_the_forge_would() {
    let prs = [
        approved(101, now() - Duration::hours(30)),
        approved(102, now() - Duration::hours(5)),
    ];

    let via_signoz = tempfile::tempdir().unwrap();
    let mut signoz = signoz_rows(&prs, true);
    let mut forge = Counting::with(&prs);
    let root = via_signoz.path();
    run_cycle_with(
        root,
        &[target(root)],
        &mut forge,
        Some((&mut signoz, limits())),
        budgets(100),
        now(),
    );
    assert!(forge.gets.is_empty());

    let via_forge = tempfile::tempdir().unwrap();
    let mut forge = Counting::with(&prs);
    let root = via_forge.path();
    run_cycle(root, &[target(root)], &mut forge, budgets(100), now());
    assert_eq!(forge.gets.len(), 1 + prs.len());

    let (s, f) = (published(via_signoz.path()), published(via_forge.path()));
    assert!(!f.samples.is_empty() && f.merges.len() == 2, "a non-trivial comparison");
    assert_eq!(s.prs, f.prs);
    assert_eq!(s.samples, f.samples);
    assert_eq!(s.episodes, f.episodes);
    assert_eq!(s.flag_changes, f.flag_changes);
    assert_eq!(s.merges, f.merges);
}

// -- gap-fill -----------------------------------------------------------------

/// Only the PRs SigNoz cannot answer alone hit the forge, one timeline read
/// each; a budget of N stops after exactly N, checkpoints, and the pass
/// resumes next cycle with no PR read twice.
#[test]
fn gap_fill_reads_only_uncovered_items_and_stops_at_its_budget() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let base = now() - Duration::hours(40);
    let prs: Vec<Truth> = (0..5)
        .map(|i| rejected(201 + i, base + Duration::hours(i64::from(i))))
        .chain((0..2).map(|i| approved(301 + i, base + Duration::hours(i64::from(i)))))
        .collect();
    let mut signoz = signoz_rows(&prs, true);
    let mut forge = Counting::with(&prs);
    let before = counters::get(GAP_FILL_COUNTER);

    let report = run_cycle_with(
        root,
        &[target(root)],
        &mut forge,
        Some((&mut signoz, limits())),
        budgets(2),
        now(),
    );
    let r = &report.repos[0];
    assert_eq!(r.stop, StopReason::Budget);
    assert_eq!(forge.gets.len(), 2, "exactly the gap budget, no overrun: {:?}", forge.gets);
    assert!(forge.gets.iter().all(|u| u.contains("/timeline?")), "no listing call");
    assert_eq!((r.forge_calls, r.gap_fill_calls), (2, Some(2)));
    assert_eq!(counters::get(GAP_FILL_COUNTER) - before, 2);
    assert_eq!(r.history, Some(HistorySource::SignozGapFill));
    assert!(!r.promoted);
    let state = read_state(&state_path(root, A)).unwrap();
    let pass = state.pass.expect("checkpointed");
    assert_eq!(pass.done, vec![201, 202], "the complete PRs (301, 302) sort after the stop");
    assert!(staging_path(root, A).exists());
    assert_eq!(state.history.unwrap().gap_fill_calls, 2);

    let mut cycles = 1;
    loop {
        let at = now() + Duration::hours(cycles);
        let report = run_cycle_with(
            root,
            &[target(root)],
            &mut forge,
            Some((&mut signoz, limits())),
            budgets(2),
            at,
        );
        cycles += 1;
        if report.repos[0].stop == StopReason::Complete {
            break;
        }
        assert_eq!(report.repos[0].forge_calls, 2);
        assert!(cycles < 10);
    }
    for pr in &prs {
        let expected = usize::from(pr.labels.iter().any(|(_, l, _)| *l == CR));
        assert_eq!(forge.timeline_reads(pr.number), expected, "PR {}", pr.number);
    }
    let snapshot = published(root);
    assert_eq!(snapshot.prs.len(), prs.len());
    assert_eq!(snapshot.as_of, now(), "every merge of the pass used its `L`");
}

/// SigNoz reachable but not reaching back over the window: the forge walk,
/// under the gap-fill budget.
#[test]
fn an_uncovered_window_falls_back_to_the_forge_under_the_gap_budget() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let prs: Vec<Truth> = (0..3)
        .map(|i| approved(101 + i, now() - Duration::hours(i64::from(i) + 4)))
        .collect();
    let mut signoz = signoz_rows(&prs, false);
    let mut forge = Counting::with(&prs);

    let report = run_cycle_with(
        root,
        &[target(root)],
        &mut forge,
        Some((&mut signoz, limits())),
        budgets(2),
        now(),
    );
    let r = &report.repos[0];
    assert_eq!(r.history, Some(HistorySource::ForgeUncovered));
    assert_eq!(r.stop, StopReason::Budget);
    assert_eq!(forge.gets.len(), 2, "one listing page and one timeline");
    assert_eq!(r.gap_fill_calls, Some(2));
}

/// SigNoz down: today's forge behaviour, bounded by the pass-kind budgets
/// only, and still counted.
#[test]
fn signoz_unavailable_degrades_to_the_forge_under_the_existing_budgets() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let prs: Vec<Truth> = (0..3)
        .map(|i| approved(101 + i, now() - Duration::hours(i64::from(i) + 4)))
        .collect();
    let mut forge = Counting::with(&prs);

    let report = run_cycle_with(
        root,
        &[target(root)],
        &mut forge,
        Some((&mut Down, limits())),
        budgets(1),
        now(),
    );
    let r = &report.repos[0];
    assert_eq!(r.history, Some(HistorySource::ForgeUnavailable));
    assert_eq!(
        (r.stop, r.promoted),
        (StopReason::Complete, true),
        "the gap budget of 1 does not apply"
    );
    assert_eq!(forge.gets.len(), 4);
    assert_eq!(r.gap_fill_calls, Some(4));

    // Without SigNoz configured nothing is counted as gap-fill.
    let dir = tempfile::tempdir().unwrap();
    let mut forge = Counting::with(&prs);
    let report = run_cycle(dir.path(), &[target(dir.path())], &mut forge, budgets(1), now());
    assert_eq!((report.repos[0].gap_fill_calls, report.repos[0].history), (None, None));
    assert!(read_state(&state_path(dir.path(), A))
        .unwrap()
        .history
        .is_none());
}

/// A walk cut short by the page ceiling saw rows that would cover the
/// window, yet it is never treated as covered: the pass degrades to the
/// forge exactly as when SigNoz is down.
#[test]
fn a_partial_signoz_walk_is_never_treated_as_covered() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let prs = [approved(101, now() - Duration::hours(10))];
    let mut signoz = signoz_rows(&prs, true);
    let mut forge = Counting::with(&prs);
    let partial = Limits {
        page_size: 2,
        max_pages: 1,
    };

    let report = run_cycle_with(
        root,
        &[target(root)],
        &mut forge,
        Some((&mut signoz, partial)),
        budgets(100),
        now(),
    );
    let r = &report.repos[0];
    assert_eq!(r.history, Some(HistorySource::ForgeUnavailable));
    assert_eq!(r.stop, StopReason::Complete);
    assert_eq!(forge.gets.len(), 1 + prs.len(), "the forge walk ran: {:?}", forge.gets);
    assert_eq!(r.gap_fill_calls, Some(2));
}

// -- progress under a small budget ---------------------------------------------

/// [`rejected`], with `extra` add/remove toggles of a bystander label first:
/// a timeline longer than one forge page.
fn long_rejected(number: u32, opened: DateTime<Utc>, extra: usize) -> Truth {
    let mut pr = rejected(number, opened);
    let toggles = (0..extra).map(|i| {
        let at = opened + Duration::seconds(i as i64 + 1);
        (i % 2 == 0, "loom:bystander", at)
    });
    pr.labels.splice(1..1, toggles);
    pr
}

/// One cycle with `gap` as the gap-fill budget, at `now() + hours`.
fn cycle_at(
    root: &Path,
    forge: &mut Counting,
    signoz: &mut FileRows,
    gap: u64,
    hours: i64,
) -> crate::eta::fleet_refresh::RepoReport {
    let at = now() + Duration::hours(hours);
    let report =
        run_cycle_with(root, &[target(root)], forge, Some((signoz, limits())), budgets(gap), at);
    report.repos[0].clone()
}

/// A covered window whose one gap-filled PR has a three-page timeline, under
/// a budget of 1: each cycle reads exactly one page, the pages already read
/// are checkpointed, no page is read twice, and the pass publishes the same
/// snapshot an uninterrupted pass does.
#[test]
fn an_interrupted_timeline_resumes_at_its_next_page_across_cycles() {
    let prs = [
        long_rejected(201, now() - Duration::hours(20), 240),
        approved(301, now() - Duration::hours(10)),
    ];
    assert_eq!(forge_timeline(&prs[0]).len().div_ceil(PER_PAGE), 3);
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let mut signoz = signoz_rows(&prs, true);
    let mut forge = Counting::with(&prs);

    let mut cycles = 0;
    loop {
        let r = cycle_at(root, &mut forge, &mut signoz, 1, cycles);
        cycles += 1;
        assert!(r.forge_calls <= 1, "never over the budget: {r:?}");
        if r.stop == StopReason::Complete {
            break;
        }
        assert_eq!(r.stop, StopReason::Budget);
        let partial = read_state(&state_path(root, A))
            .unwrap()
            .pass
            .unwrap()
            .partial;
        let partial = partial.expect("the pages read are checkpointed");
        assert_eq!((partial.number, partial.next_page), (201, cycles as u32 + 1));
        assert_eq!(partial.events.len(), PER_PAGE * cycles as usize);
        assert!(cycles < 10, "bounded");
    }
    assert_eq!(cycles, 3, "one page per cycle");
    assert_eq!(forge.timeline_reads(201), 3, "no page read twice: {:?}", forge.gets);
    assert!(read_state(&state_path(root, A)).unwrap().pass.is_none());

    let whole = tempfile::tempdir().unwrap();
    let mut signoz = signoz_rows(&prs, true);
    let mut forge = Counting::with(&prs);
    assert_eq!(
        cycle_at(whole.path(), &mut forge, &mut signoz, 100, 0).stop,
        StopReason::Complete
    );
    let (resumed, once) = (published(root), published(whole.path()));
    assert_eq!(resumed.prs, once.prs);
    assert_eq!(resumed.samples, once.samples);
    assert_eq!(resumed.episodes, once.episodes);
    assert_eq!(resumed.merges, once.merges);
}

/// An uncovered window at the config floor (2): each cycle re-reads the
/// listing page and still reads one timeline page, so a multi-page timeline
/// completes and the pass ends — the shape a budget of 1 stalled on.
#[test]
fn an_uncovered_pass_at_the_budget_floor_completes() {
    let floor = crate::eta::config::MIN_FLEET_REFRESH_GAP_FILL_MAX_CALLS;
    let prs = [
        long_rejected(201, now() - Duration::hours(20), 240),
        approved(101, now() - Duration::hours(4)),
    ];
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let mut signoz = signoz_rows(&prs, false);
    let mut forge = Counting::with(&prs);

    let mut cycles = 0;
    loop {
        let r = cycle_at(root, &mut forge, &mut signoz, floor, cycles);
        cycles += 1;
        assert_eq!(r.history, Some(HistorySource::ForgeUncovered));
        assert!(r.forge_calls <= floor, "never over the budget: {r:?}");
        if r.stop == StopReason::Complete {
            assert!(r.promoted);
            break;
        }
        assert!(cycles < 10, "bounded");
    }
    // 101's one page, then 201's three, one timeline page per cycle.
    assert_eq!(cycles, 4);
    assert_eq!((forge.timeline_reads(101), forge.timeline_reads(201)), (1, 3));
    assert_eq!(published(root).prs, vec![101, 201]);
}

// -- the production cycle boundary (raw events too) ----------------------------

/// What the events seam saw: `(endpoint, calls allowed)`.
type Seen = Vec<(crate::eta::fleet_events_forge::ForgeEndpoint, u64)>;

/// [`crate::observability::eta_fleet_refresh::cycle_with`] over `prs`: the
/// snapshot phase *and* the raw-event phase, with an events seam that spends
/// up to 3 calls per listing. Returns the repo's report and what the seam saw.
fn production_cycle(
    root: &Path,
    forge: &mut Counting,
    signoz: Option<&mut dyn SignozRead>,
    gap: u64,
) -> (crate::eta::fleet_refresh::RepoReport, Seen) {
    use crate::eta::config::FleetRefreshConfig;
    use crate::observability::eta_fleet_refresh::{cycle_with, TaskState};
    let config = FleetRefreshConfig {
        gap_fill_max_calls_per_pass: gap,
        ..FleetRefreshConfig::default()
    };
    let mut seen = Seen::new();
    let mut events = |_: &RepoTarget,
                      _: &Reader,
                      endpoint: crate::eta::fleet_events_forge::ForgeEndpoint,
                      left: u64,
                      _: crate::eta::fleet_events::SyncMode| {
        seen.push((endpoint, left));
        (0, left.min(3), None)
    };
    let outcome = cycle_with(
        root,
        &[target(root)],
        (forge, signoz.map(|s| (s, limits()))),
        &mut events,
        &config,
        &mut TaskState::default(),
        now(),
    );
    (outcome.report.repos[0].clone(), seen)
}

/// The judge's case (#10520): at the production cycle boundary a covered
/// window makes no forge read in either phase — no `ForgeRead` get and no
/// raw-event sync — and reports 0; an uncovered window's raw-event reads are
/// gap-fill, drawn from what the snapshot pass left of the same budget and
/// counted in the doctor's note; an unavailable SigNoz leaves them under the
/// pass-kind budgets, counted; with SigNoz history off nothing changes.
#[test]
fn the_production_cycle_counts_and_caps_raw_event_reads_too() {
    let prs = [approved(101, now() - Duration::hours(10))];
    let note = |root: &Path| read_state(&state_path(root, A)).unwrap().history.unwrap();

    // Covered: zero, in both phases.
    let dir = tempfile::tempdir().unwrap();
    let mut forge = Counting::with(&prs);
    let before = counters::get(GAP_FILL_COUNTER);
    let mut signoz = signoz_rows(&prs, true);
    let (r, seen) = production_cycle(dir.path(), &mut forge, Some(&mut signoz), 5);
    assert!(forge.gets.is_empty(), "no snapshot read: {:?}", forge.gets);
    assert!(seen.is_empty(), "no raw-event read: {seen:?}");
    assert_eq!((r.forge_calls, r.gap_fill_calls, r.raw_events_added), (0, Some(0), None));
    assert_eq!(counters::get(GAP_FILL_COUNTER), before);
    assert_eq!(note(dir.path()).gap_fill_calls, 0);

    // Uncovered: listing + timeline (2), then the 3 calls left of 5 go to the
    // first events listing and the second gets none.
    let dir = tempfile::tempdir().unwrap();
    let mut forge = Counting::with(&prs);
    let before = counters::get(GAP_FILL_COUNTER);
    let mut signoz = signoz_rows(&prs, false);
    let (r, seen) = production_cycle(dir.path(), &mut forge, Some(&mut signoz), 5);
    assert_eq!(forge.gets.len(), 2);
    assert_eq!(seen.iter().map(|s| s.1).collect::<Vec<_>>(), vec![3], "capped: {seen:?}");
    assert_eq!((r.forge_calls, r.gap_fill_calls), (5, Some(5)), "both phases, one cap");
    assert_eq!(counters::get(GAP_FILL_COUNTER) - before, 5);
    let n = note(dir.path());
    assert_eq!((n.source, n.gap_fill_calls), (HistorySource::ForgeUncovered, 5));

    // Unavailable: the pass-kind budgets, not the gap cap — but counted.
    let dir = tempfile::tempdir().unwrap();
    let mut forge = Counting::with(&prs);
    let (r, seen) = production_cycle(dir.path(), &mut forge, Some(&mut Down), 5);
    assert_eq!(seen.len(), 2);
    assert!(seen.iter().all(|s| s.1 > 5), "not gap-capped: {seen:?}");
    assert_eq!((r.forge_calls, r.gap_fill_calls), (8, Some(8)));
    assert_eq!(note(dir.path()).gap_fill_calls, 8);

    // Off: pre-#10520 behaviour, nothing counted.
    let dir = tempfile::tempdir().unwrap();
    let mut forge = Counting::with(&prs);
    let (r, seen) = production_cycle(dir.path(), &mut forge, None, 5);
    assert_eq!((seen.len(), r.forge_calls, r.gap_fill_calls), (2, 8, None));
}

// -- star coverage of a raw cache SigNoz stops (#10520, judge round 3) ---------

/// A pre-#10520 raw cache for [`A`]: PR 101 links issue 7 (known at
/// `linked`), and issue 7 is labelled (no star) at `linked` and again at
/// `labeled`, the newest row. No cursor stamp.
fn plant_raw_cache(root: &Path, linked: DateTime<Utc>, labeled: DateTime<Utc>) {
    use crate::eta::fleet_events::{self, EventKind, EventLog, ItemKind, RawEvent};
    let rows = [
        RawEvent::new(
            A,
            101,
            ItemKind::Pr,
            EventKind::ClosingRef,
            Some("closes".into()),
            linked,
            "forge",
            1,
            linked,
        )
        .with_target(Some(7)),
        RawEvent::new(
            A,
            7,
            ItemKind::Issue,
            EventKind::LabelAdded,
            Some("loom:issue".into()),
            linked,
            "forge",
            2,
            linked,
        ),
        RawEvent::new(
            A,
            7,
            ItemKind::Issue,
            EventKind::LabelAdded,
            Some("loom:ready".into()),
            labeled,
            "forge",
            3,
            labeled,
        ),
    ];
    EventLog::open(&fleet_events::events_path(root, A))
        .unwrap()
        .append(&rows)
        .unwrap();
}

/// The judge's star finding at the production boundary: SigNoz covers the
/// repo, so its raw cache is not read and stops advancing. Before the fix the
/// cache still "covered" every later cutoff, so a link or star added after it
/// stopped read as known-unstarred. Now the cycle freezes the cache's
/// coverage at its newest row, and a cutoff after it reads unknown (`None`);
/// before it the cache still answers. A completed refresh (SigNoz history
/// off) stamps the listings at the cycle's `now` instead.
#[test]
fn a_covered_repo_freezes_raw_cache_coverage_so_stars_read_unknown_after_it() {
    use crate::eta::fleet_events::{self, EventsCursor};
    use crate::eta::star::{listing_keys, StarInputs};
    let prs = [approved(101, now() - Duration::hours(10))];
    let (linked, labeled) = (now() - Duration::days(3), now() - Duration::days(2));
    let repos = [A.to_string()];

    // Covered: no raw-event read, coverage frozen at the newest cached row.
    let dir = tempfile::tempdir().unwrap();
    plant_raw_cache(dir.path(), linked, labeled);
    let mut forge = Counting::with(&prs);
    let mut signoz = signoz_rows(&prs, true);
    let (r, seen) = production_cycle(dir.path(), &mut forge, Some(&mut signoz), 5);
    assert_eq!((r.history, seen.len()), (Some(HistorySource::Signoz), 0));
    let star = StarInputs::load(dir.path(), &repos);
    let star = &star.repos[A];
    assert_eq!(star.synced_through, Some(labeled), "frozen at the newest cached row");
    let after = labeled + Duration::hours(1);
    assert_eq!(star.state_at(101, 0, None, after), None, "after the cache stopped: unknown");
    assert!(star.state_at(101, 0, None, labeled).is_some(), "before it, the cache answers");

    // A frozen stamp stays put on later covered cycles.
    let mut signoz = signoz_rows(&prs, true);
    production_cycle(dir.path(), &mut forge, Some(&mut signoz), 5);
    let cursor = EventsCursor::read(&fleet_events::cursor_path(dir.path(), A), A);
    assert_eq!(cursor.synced_through(&listing_keys()), Some(labeled));

    // History off, both listings refreshing to completion: stamped at `now`.
    let dir = tempfile::tempdir().unwrap();
    plant_raw_cache(dir.path(), linked, labeled);
    let cursor_file = fleet_events::cursor_path(dir.path(), A);
    let mut cursor = EventsCursor::read(&cursor_file, A);
    for key in listing_keys() {
        cursor.endpoints.entry(key).or_default().backfill_complete = true;
    }
    cursor.write(&cursor_file).unwrap();
    let mut forge = Counting::with(&prs);
    let (_, seen) = production_cycle(dir.path(), &mut forge, None, 5);
    assert_eq!(seen.len(), 2, "both listings refreshed");
    let star = StarInputs::load(dir.path(), &repos);
    assert_eq!(star.repos[A].synced_through, Some(now()));
    assert!(star.repos[A].state_at(101, 0, None, now()).is_some());
}

// -- baseline-only PRs (#10520, judge round 3) --------------------------------

/// Requested at `opened` and nothing since: open, awaiting review.
fn requested(number: u32, opened: DateTime<Utc>) -> Truth {
    Truth {
        number,
        opened,
        labels: vec![(true, RR, opened)],
        merged: None,
        pushed: None,
    }
}

/// [`signoz_rows`] for `complete`, plus PR `baseline`'s daemon label sets
/// only — `sets` identical `[RR]` sets five minutes apart, no `opened` and no
/// transition row: what SigNoz holds for a PR the daemon first saw mid-pass.
fn with_baseline(complete: &[Truth], baseline: &Truth, sets: usize) -> FileRows {
    let mut lines = signoz_lines(complete, true);
    for i in 0..sets {
        let seen = baseline.opened + Duration::minutes(3 + 5 * i as i64);
        lines.push(label_set_row(&format!("set-{i}"), baseline.number, &[RR], seen));
    }
    FileRows::parse(&lines.join("\n")).unwrap()
}

/// The judge's case: old issue rows cover both families; PR 401 is known to
/// SigNoz only by its daemon label-set baseline (once, or repeated
/// identically) — no `opened`, no transition. The covered pass must not drop
/// it: it is planned, gap-filled with exactly one timeline read (no listing,
/// no invented transition time), and the published snapshot equals the
/// forge-built one. At the production boundary the raw-event phase still
/// makes no read.
#[test]
fn a_baseline_only_pr_is_gap_filled_not_dropped() {
    let complete = [approved(101, now() - Duration::hours(30))];
    let baseline = requested(401, now() - Duration::hours(6));
    let all = [complete[0].clone(), baseline.clone()];

    for sets in [1, 3] {
        let via_signoz = tempfile::tempdir().unwrap();
        let root = via_signoz.path();
        let mut signoz = with_baseline(&complete, &baseline, sets);
        let mut forge = Counting::with(&all);
        let report = run_cycle_with(
            root,
            &[target(root)],
            &mut forge,
            Some((&mut signoz, limits())),
            budgets(100),
            now(),
        );
        let r = &report.repos[0];
        assert_eq!(forge.timeline_reads(401), 1, "{sets} set(s): 401 is gap-filled");
        assert_eq!(forge.gets.len(), 1, "no listing, nothing else: {:?}", forge.gets);
        assert_eq!(r.history, Some(HistorySource::SignozGapFill));
        assert_eq!((r.stop, r.gap_fill_calls), (StopReason::Complete, Some(1)));

        let via_forge = tempfile::tempdir().unwrap();
        let mut forge = Counting::with(&all);
        run_cycle(via_forge.path(), &[target(via_forge.path())], &mut forge, budgets(100), now());

        let (s, f) = (published(root), published(via_forge.path()));
        assert_eq!(s.prs, vec![101, 401], "{sets} set(s): the baseline-only PR is published");
        assert_eq!(s.prs, f.prs);
        assert!(!f.samples.is_empty(), "a non-trivial comparison");
        assert_eq!(s.samples, f.samples, "{sets} set(s)");
        assert_eq!(s.episodes, f.episodes, "{sets} set(s)");
        assert_eq!(s.flag_changes, f.flag_changes, "{sets} set(s)");
        assert_eq!(s.merges, f.merges, "{sets} set(s)");

        // The production cycle: same single read, and no raw-event read.
        let dir = tempfile::tempdir().unwrap();
        let mut forge = Counting::with(&all);
        let mut signoz = with_baseline(&complete, &baseline, sets);
        let (r, seen) = production_cycle(dir.path(), &mut forge, Some(&mut signoz), 5);
        assert_eq!(forge.gets.len(), 1, "{:?}", forge.gets);
        assert_eq!(forge.timeline_reads(401), 1);
        assert!(seen.is_empty(), "covered: no raw-event read: {seen:?}");
        assert_eq!((r.history, r.gap_fill_calls), (Some(HistorySource::SignozGapFill), Some(1)));
        assert_eq!(published(dir.path()).prs, vec![101, 401]);
    }
}

// -- the adapter --------------------------------------------------------------

fn timeline_of(prs: &[Truth], anchored: bool) -> Timeline {
    let mut rows = signoz_rows(prs, anchored);
    let (rows, _) = walk(A, &mut rows, now() - Duration::days(40), now(), limits()).unwrap();
    Timeline::build(&rows, now())
}

#[test]
fn an_item_is_complete_only_when_born_in_the_window_and_never_rejected() {
    let base = now() - Duration::hours(10);
    let timeline = timeline_of(&[approved(1, base), rejected(2, base)], true);
    let item = |n| timeline.item(&ItemKey::new(A, Target::Pr, n)).unwrap();
    assert!(item_complete(item(1)));
    assert!(!item_complete(item(2)), "a rejection needs the forge's push");

    let mut unborn = timeline
        .item(&ItemKey::new(A, Target::Pr, 1))
        .unwrap()
        .clone();
    unborn
        .lifecycle
        .retain(|e| e.event != crate::eta::fleet_signoz_timeline_rows::Lifecycle::Opened);
    assert!(!item_complete(&unborn), "no `opened`: its first labels may predate the window");

    // Born, never rejected, but its daemon baseline carries a label no
    // recorded change dates: not complete (its addition time is unknown).
    let mut undated = item(1).clone();
    let seen = base + Duration::minutes(5);
    undated.label_sets = vec![(seen, [RR, "loom:bystander"].map(String::from).into())];
    assert!(!item_complete(&undated), "an undated baseline label needs the forge");
    undated.label_sets = vec![(seen, [RR].map(String::from).into())];
    assert!(item_complete(&undated), "a baseline the recorded changes explain is fine");
}

#[test]
fn a_gap_filled_item_takes_the_forges_events_and_merge_instant() {
    let listed = ListedPr {
        created_at: now() - Duration::hours(3),
        state: PrState::Open,
        merged_at: None,
        labels: Vec::new(),
    };
    let merged_at = now() - Duration::hours(1);
    let events = vec![
        PrEvent::Labeled {
            label: RR.to_string(),
            at: now() - Duration::hours(2),
        },
        PrEvent::Merged { at: merged_at },
    ];
    let h = gap_fill_history(9, &listed, events.clone());
    assert_eq!((h.state, h.merged_at), (PrState::Merged, Some(merged_at)));
    assert_eq!(h.events, events);
    assert!(h.timeline_complete);
}

// -- parity on the recorded fixture -------------------------------------------

const FIXTURE_REPO: &str = "rjwalters/loom";
const FIXTURE: &str = include_str!("../fixtures/signoz-timeline.jsonl");

fn t(sec: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 1, 10, 0, 0).unwrap() + Duration::seconds(sec)
}

/// The forge's history for a fixture PR, from a REST timeline body.
fn forge_history(number: u32, merged_at: Option<DateTime<Utc>>, body: &Value) -> PrHistory {
    let events = parse_timeline(body.to_string().as_bytes()).unwrap();
    let listed = ListedPr {
        created_at: t(-3600),
        state: if merged_at.is_some() {
            PrState::Merged
        } else {
            PrState::Closed
        },
        merged_at,
        labels: Vec::new(),
    };
    history(number, &listed, events, true)
}

/// `(added, label)` of a label event.
fn label_key(e: &PrEvent) -> Option<(bool, String)> {
    match e {
        PrEvent::Labeled { label, .. } => Some((true, label.clone())),
        PrEvent::Unlabeled { label, .. } => Some((false, label.clone())),
        _ => None,
    }
}

/// On the fixture window the SigNoz-sourced timeline of each PR equals the
/// forge-sourced one in its label transitions and merge instant, within the
/// documented tolerance for the event's source: [`MATCH_SLACK_SEC`] for a
/// webhook receipt, [`DAEMON_POLL_TOLERANCE_SEC`] for a daemon poll.
#[test]
fn signoz_and_forge_timelines_agree_within_the_polling_tolerance() {
    let mut reader = FileRows::parse(FIXTURE).unwrap();
    let (rows, _) = walk(FIXTURE_REPO, &mut reader, t(-86_400), t(30 * 86_400), limits()).unwrap();
    let cutoff = t(30 * 86_400);
    let timeline = Timeline::build(&rows, cutoff);

    // The forge's record of the same two PRs (the fixture's ground truth:
    // #900 requested, approved and merged; #901 requested, rejected, then
    // closed unmerged).
    let truth: BTreeMap<u32, PrHistory> = [
        forge_history(
            900,
            Some(t(7200)),
            &json!([
                {"event": "labeled", "created_at": t(0).to_rfc3339(), "label": {"name": RR}},
                {"event": "unlabeled", "created_at": t(3610).to_rfc3339(), "label": {"name": RR}},
                {"event": "labeled", "created_at": t(3612).to_rfc3339(), "label": {"name": APPROVED}},
                {"event": "merged", "created_at": t(7200).to_rfc3339()},
            ]),
        ),
        forge_history(
            901,
            None,
            &json!([
                {"event": "labeled", "created_at": t(-60).to_rfc3339(), "label": {"name": RR}},
                {"event": "unlabeled", "created_at": t(720).to_rfc3339(), "label": {"name": RR}},
                {"event": "labeled", "created_at": t(720).to_rfc3339(), "label": {"name": CR}},
            ]),
        ),
    ]
    .into_iter()
    .map(|h| (h.number, h))
    .collect();

    for (number, forge) in &truth {
        let item = timeline
            .item(&ItemKey::new(FIXTURE_REPO, Target::Pr, *number))
            .unwrap();
        let signoz = pr_history(*number, item, cutoff);
        // Every SigNoz label event has its forge counterpart within its
        // source's tolerance.
        for e in &item.labels {
            let tolerance = match e.source {
                Source::Webhook => MATCH_SLACK_SEC,
                Source::Daemon => DAEMON_POLL_TOLERANCE_SEC,
            };
            let key = (e.transition == Transition::Added, e.label.clone());
            let nearest = forge
                .events
                .iter()
                .filter(|f| label_key(f).as_ref() == Some(&key))
                .map(|f| (f.at() - e.at).num_seconds().abs())
                .min()
                .unwrap_or_else(|| panic!("#{number}: no forge counterpart for {e:?}"));
            assert!(nearest <= tolerance, "#{number}: {e:?} is {nearest}s off (> {tolerance}s)");
        }
        // The merge instant.
        match (signoz.merged_at, forge.merged_at) {
            (Some(s), Some(f)) => {
                let source = item.resolution().unwrap().source;
                let tolerance = if source == Source::Webhook {
                    MATCH_SLACK_SEC
                } else {
                    DAEMON_POLL_TOLERANCE_SEC
                };
                assert!((s - f).num_seconds().abs() <= tolerance, "#{number} merge {s} vs {f}");
                assert!(signoz
                    .events
                    .iter()
                    .any(|e| matches!(e, PrEvent::Merged { at } if *at == s)));
            }
            (None, None) => {}
            other => panic!("#{number}: merge disagrees: {other:?}"),
        }
    }

    // #900 is all webhook-dated: the same transitions, in the same order.
    let keys = |h: &PrHistory| h.events.iter().filter_map(label_key).collect::<Vec<_>>();
    let item900 = timeline
        .item(&ItemKey::new(FIXTURE_REPO, Target::Pr, 900))
        .unwrap();
    assert_eq!(keys(&pr_history(900, item900, cutoff)), keys(&truth[&900]));
    // #901 is d1sync-only (the 09-14..09-28 shape): webhook-dated, never
    // self-corroborated, and with no `opened` row, so a pass gap-fills it.
    let item901 = timeline
        .item(&ItemKey::new(FIXTURE_REPO, Target::Pr, 901))
        .unwrap();
    assert!(item901
        .labels
        .iter()
        .all(|e| e.source == Source::Webhook && !e.corroborated));
    assert!(!item_complete(item901));
    assert!(!item_complete(item900), "the fixture carries no `opened` row");
}
